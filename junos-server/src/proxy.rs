//! Outbound WebSocket handler for browsers (`/ws`).
//!
//! Browsers subscribe to the KStars event broadcast and push commands into
//! the per-session KStars queue via the shared `Hub`.
//!
//! One exception to "the relay translates nothing": messages whose type
//! starts with `junos_` are for the relay itself and never reach KStars.
//! There is one, `junos_media_filter`, by which a browser says which preview
//! kinds it is displaying and whether it wants the lite re-encode:
//!
//! ```json
//! {"type":"junos_media_filter","payload":{"kinds":["+G"],"max_width":960,"quality":70}}
//! ```
//!
//! Until a browser has sent it, it receives no preview at all — KStars pushes
//! a 1920 px guide frame every few seconds whether or not anyone looks at it,
//! and the field use case is a phone on a weak wifi link.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tracing::{debug, warn};

use crate::apps::build_app_state_msg;
use crate::hub::{FrameKind, Outbound};
use crate::AppState;

const FILTER_TYPE: &str = "junos_media_filter";

/// What one browser wants from the preview stream.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MediaFilter {
    kinds: HashSet<FrameKind>,
    /// The browser asked for the lite re-encode (`max_width` present).
    lite: bool,
}

impl MediaFilter {
    /// Parse a `junos_media_filter` payload. Unknown kinds are ignored.
    fn parse(payload: &Value) -> Self {
        let kinds = payload
            .get("kinds")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).filter_map(FrameKind::parse).collect())
            .unwrap_or_default();
        let lite = payload.get("max_width").and_then(Value::as_u64).is_some_and(|w| w > 0);
        Self { kinds, lite }
    }

    /// The variant of a preview this browser should get, if any.
    fn pick<'a>(&self, kind: FrameKind, full: &'a str, lite: Option<&'a str>) -> Option<&'a str> {
        if !self.kinds.contains(&kind) {
            return None;
        }
        Some(match (self.lite, lite) {
            (true, Some(l)) => l,
            _ => full,
        })
    }
}

/// Holds one slot in the hub's lite-subscriber count while a browser wants
/// lite previews, and releases it however the connection ends.
struct LiteSlot {
    counter: Arc<AtomicUsize>,
    held: bool,
}

impl LiteSlot {
    fn set(&mut self, want: bool) {
        if want && !self.held {
            self.counter.fetch_add(1, Ordering::Relaxed);
        } else if !want && self.held {
            self.counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.held = want;
    }
}

impl Drop for LiteSlot {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// `Some(payload)` when `text` is a relay control message of type
/// `junos_media_filter`. Cheap on the common path: a substring test before
/// any JSON parse.
fn media_filter_payload(text: &str) -> Option<Value> {
    if !text.contains(FILTER_TYPE) {
        return None;
    }
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("type").and_then(Value::as_str) != Some(FILTER_TYPE) {
        return None;
    }
    Some(v.get("payload").cloned().unwrap_or(Value::Null))
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_browser_ws(socket, state))
}

async fn handle_browser_ws(socket: WebSocket, state: AppState) {
    let hub = state.hub.clone();
    let mut kstars_rx = hub.subscribe();

    let (mut sink, mut stream) = socket.split();

    // On connect, replay the last connection state from KStars so the browser
    // gets the full connected+online flags even after a page refresh.
    let init = match hub.last_connection_state.lock().await.clone() {
        Some(state) => state,
        None => {
            let connected = hub.kstars_msg_tx.lock().await.is_some();
            let home = std::env::var("HOME").unwrap_or_default();
            format!(
                r#"{{"type":"new_connection_state","payload":{{"connected":{},"home_dir":"{}"}}}}"#,
                connected, home
            )
        }
    };
    let _ = sink.send(Message::Text(init.into())).await;

    // Replay the cached observer site (if KStars is attached and has answered)
    // so this browser gets KStars' real location immediately, even when it
    // opened after KStars was already up.
    if let Some(site) = hub.last_site_location.lock().await.clone() {
        let _ = sink.send(Message::Text(site.into())).await;
    }

    // Also send the current app (KStars / PHD2) running state so the browser
    // gets an up-to-date badge immediately on (re)connect.
    let app_status = state.app_manager.status_json().await;
    let kstars = app_status["kstars"].as_str() == Some("running");
    let phd2   = app_status["phd2"].as_str()   == Some("running");
    let app_init = build_app_state_msg(kstars, phd2);
    let _ = sink.send(Message::Text(app_init.into())).await;

    // Replay the cached KStars state (recent guiding metrics, FOV geometry,
    // module states) so a dropped/refreshed browser recovers it immediately
    // instead of waiting for KStars to push again. Previews follow once the
    // browser says which views it shows.
    for msg in hub.replay_snapshot().await {
        if sink.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }

    let mut filter = MediaFilter::default();
    let mut lite_slot = LiteSlot { counter: hub.lite_subscribers(), held: false };

    loop {
        tokio::select! {
            // KStars event → browser
            result = kstars_rx.recv() => {
                match result {
                    Ok(out) => {
                        let text = match out.as_ref() {
                            Outbound::Text(t) => t.as_str(),
                            Outbound::Preview { kind, full, lite } => {
                                match filter.pick(*kind, full, lite.as_deref()) {
                                    Some(t) => t,
                                    None => continue,
                                }
                            }
                        };
                        if sink.send(Message::Text(text.to_string())).await.is_err() {
                            debug!("Browser WebSocket closed (send failed)");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Browser lagged behind by {n} messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // Browser command → KStars (or the relay, for `junos_*`)
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(payload) = media_filter_payload(&text) {
                            let new = MediaFilter::parse(&payload);
                            debug!(?new, "Browser media filter");
                            lite_slot.set(new.lite);
                            // Push the current frame of every view the browser
                            // just subscribed to (all of them when the variant
                            // changed), so a tab switch shows an image at once.
                            let resend: Vec<FrameKind> = if new.lite != filter.lite {
                                new.kinds.iter().copied().collect()
                            } else {
                                new.kinds.difference(&filter.kinds).copied().collect()
                            };
                            filter = new;
                            for kind in resend {
                                if let Some(out) = hub.last_preview(kind).await {
                                    if let Outbound::Preview { full, lite, .. } = out.as_ref() {
                                        if let Some(t) = filter.pick(kind, full, lite.as_deref()) {
                                            if sink.send(Message::Text(t.to_string())).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                        if !hub.send_to_kstars(text.to_string()).await {
                            debug!("Dropping browser command — no KStars session attached");
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        warn!("Browser WebSocket error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_parses_kinds_and_lite() {
        let f = MediaFilter::parse(&json!({"kinds": ["+G", "capture", "+X"], "max_width": 960, "quality": 70}));
        assert_eq!(f.kinds, HashSet::from([FrameKind::Guide, FrameKind::Capture]));
        assert!(f.lite);
        let f = MediaFilter::parse(&json!({"kinds": ["+F"]}));
        assert!(!f.lite);
        assert!(MediaFilter::parse(&Value::Null).kinds.is_empty());
    }

    #[test]
    fn nothing_is_forwarded_before_a_filter_arrives() {
        let f = MediaFilter::default();
        for kind in FrameKind::ALL {
            assert_eq!(f.pick(kind, "full", Some("lite")), None);
        }
    }

    #[test]
    fn pick_follows_kind_and_variant() {
        let f = MediaFilter { kinds: HashSet::from([FrameKind::Guide]), lite: true };
        assert_eq!(f.pick(FrameKind::Guide, "full", Some("lite")), Some("lite"));
        // No lite variant built (e.g. a PNG frame): the full one goes out.
        assert_eq!(f.pick(FrameKind::Guide, "full", None), Some("full"));
        assert_eq!(f.pick(FrameKind::Focus, "full", Some("lite")), None);
        let f = MediaFilter { kinds: HashSet::from([FrameKind::Guide]), lite: false };
        assert_eq!(f.pick(FrameKind::Guide, "full", Some("lite")), Some("full"));
    }

    #[test]
    fn control_messages_are_recognised_whatever_the_key_order() {
        // serde_json's `json!` on the client writes keys alphabetically, so
        // `type` comes after `payload`.
        let text = r#"{"payload":{"kinds":["+F"]},"type":"junos_media_filter"}"#;
        assert_eq!(media_filter_payload(text).unwrap()["kinds"][0], "+F");
        assert!(media_filter_payload(r#"{"type":"mount_goto_rade","payload":{}}"#).is_none());
        // A KStars command that merely mentions the name is not a control message.
        assert!(media_filter_payload(r#"{"type":"scheduler_add_jobs","payload":{"name":"junos_media_filter"}}"#).is_none());
    }

    #[test]
    fn lite_slot_counts_once_and_releases_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let mut slot = LiteSlot { counter: counter.clone(), held: false };
            slot.set(true);
            slot.set(true);
            assert_eq!(counter.load(Ordering::Relaxed), 1);
            slot.set(false);
            assert_eq!(counter.load(Ordering::Relaxed), 0);
            slot.set(true);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }
}
