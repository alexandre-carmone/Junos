//! Inbound WebSocket handlers for KStars.
//!
//! KStars is the Ekos Live client: it dials us on `/message/ekos` (text JSON)
//! and `/media/ekos` (binary FITS/JPEG previews). Both accept query parameters
//! for auth (username, token, observatory, version, …) that we ignore — we're
//! a local bridge, not a real auth server.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::hub::{Hub, CMD_CAP};
use crate::AppState;

/// Metadata header size in binary media frames (from `kstars/ekos/ekoslive/media.h`).
const METADATA_PACKET: usize = 512;
/// Safety guard for oversized binary frames (metadata + payload).
const MAX_MEDIA_FRAME: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// /message/ekos — text JSON channel
// ---------------------------------------------------------------------------

pub async fn message_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_message(socket, state.hub.clone()))
}

async fn handle_message(socket: WebSocket, hub: Hub) {
    let Some(socket) = ensure_single_message_session(&hub, socket).await else {
        return;
    };

    info!(channel = "/message/ekos", "KStars session connected");

    let (mut sink, mut stream) = socket.split();

    // Per-session command queue: browsers push into `cmd_tx`, this handler
    // drains `cmd_rx` and writes to the KStars socket.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(CMD_CAP);
    publish_message_sender(&hub, cmd_tx).await;

    // Prime KStars: tell it a client is ready so it starts emitting responses.
    // Without this, `Node::sendResponse` in KStars silently drops events because
    // `m_ClientState` defaults to false.
    let handshake = set_client_state_msg();
    if let Err(e) = sink.send(Message::Text(handshake.into())).await {
        error!("Failed to send set_client_state handshake to KStars: {e}");
        clear_kstars_sender(&hub).await;
        return;
    }

    // Notify browsers that a KStars session is live, including the server-side
    // home directory so the WASM client can construct absolute paths for files
    // it saves via scheduler_save_sequence_file (which prepends homePath on the
    // KStars side).
    let home = std::env::var("HOME").unwrap_or_default();
    let _ = hub
        .browser_tx
        .send(connection_state_msg(true, Some(home.as_str())));

    loop {
        tokio::select! {
            // KStars → browsers
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        debug!(
                            len = text.len(),
                            event_type = event_type_for_log(&text),
                            "KStars message -> browsers"
                        );
                        // Snapshot new_connection_state so late-joining browsers
                        // get the full connected+online state on connect.
                        if message_is_type(&text, "new_connection_state") {
                            *hub.last_connection_state.lock().await = Some(text.to_string());
                        }
                        let _ = hub.browser_tx.send(text.to_string());
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!(channel = "/message/ekos", "KStars closed session");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!(channel = "/message/ekos", "KStars session error: {e}");
                        break;
                    }
                    _ => {}
                }
            }

            // Browsers → KStars
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        debug!(len = cmd.len(), "Browser command → KStars");
                        if let Err(e) = sink.send(Message::Text(cmd.into())).await {
                            warn!("Failed to forward command to KStars: {e}");
                            break;
                        }
                    }
                    None => {
                        // All senders dropped — shouldn't happen while we hold one.
                        break;
                    }
                }
            }
        }
    }

    clear_kstars_sender(&hub).await;

    let _ = hub.browser_tx.send(connection_state_msg(false, None));

    info!(channel = "/message/ekos", "KStars session ended");
}

async fn clear_kstars_sender(hub: &Hub) {
    let mut guard = hub.kstars_msg_tx.lock().await;
    *guard = None;
    *hub.last_connection_state.lock().await = None;
}

// ---------------------------------------------------------------------------
// /media/ekos — binary channel (FITS/JPEG previews)
// ---------------------------------------------------------------------------

pub async fn media_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_media(socket, state.hub.clone()))
}

async fn handle_media(socket: WebSocket, hub: Hub) {
    info!(channel = "/media/ekos", "KStars session connected");

    let (_sink, mut stream) = socket.split();

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Binary(data)) => {
                debug!(bytes = data.len(), channel = "/media/ekos", "KStars media frame");
                if let Some(json) = decode_media_frame(&data) {
                    let _ = hub.browser_tx.send(json);
                }
            }
            Ok(Message::Text(text)) => {
                // Some versions of Ekos Live send text JSON on media channel too.
                let _ = hub.browser_tx.send(text.to_string());
            }
            Ok(Message::Ping(_)) => { /* Axum auto-pongs */ }
            Ok(Message::Close(_)) => {
                info!(channel = "/media/ekos", "KStars closed session");
                break;
            }
            Err(e) => {
                warn!(channel = "/media/ekos", "KStars session error: {e}");
                break;
            }
            _ => {}
        }
    }

    info!(channel = "/media/ekos", "KStars session ended");
}

/// Decode a binary media frame into a `new_preview_image` JSON string.
///
/// Frame layout (from `kstars/ekos/ekoslive/media.h`):
/// - Bytes 0..512: JSON metadata, null-padded to `METADATA_PACKET` bytes
/// - Bytes 512..: raw JPEG (or FITS) data
fn decode_media_frame(data: &[u8]) -> Option<String> {
    if data.len() > MAX_MEDIA_FRAME {
        warn!(
            bytes = data.len(),
            limit = MAX_MEDIA_FRAME,
            "Media frame too large, dropping"
        );
        return None;
    }

    if data.len() <= METADATA_PACKET {
        warn!("Media frame too short: {} bytes", data.len());
        return None;
    }

    let header = &data[..METADATA_PACKET];
    let end = header.iter().position(|&b| b == 0).unwrap_or(METADATA_PACKET);
    let meta_str = std::str::from_utf8(&header[..end]).ok()?;
    let metadata: Value = match serde_json::from_str(meta_str) {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "Media metadata is not JSON, forwarding as raw");
            json!({ "raw": meta_str })
        }
    };

    let payload = &data[METADATA_PACKET..];
    let data_b64 = BASE64.encode(payload);

    let msg = json!({
        "type": "new_preview_image",
        "payload": {
            "metadata": metadata,
            "data":     data_b64,
        }
    });

    Some(msg.to_string())
}

fn set_client_state_msg() -> String {
    json!({
        "type": "set_client_state",
        "payload": { "state": true }
    })
    .to_string()
}

fn connection_state_msg(connected: bool, home_dir: Option<&str>) -> String {
    if let Some(home) = home_dir {
        return json!({
            "type": "new_connection_state",
            "payload": {
                "connected": connected,
                "home_dir": home,
            },
        })
        .to_string();
    }

    json!({
        "type": "new_connection_state",
        "payload": { "connected": connected },
    })
    .to_string()
}

fn message_is_type(msg: &str, expected: &str) -> bool {
    serde_json::from_str::<Value>(msg)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|s| s == expected))
        .unwrap_or(false)
}

fn event_type_for_log(msg: &str) -> &'static str {
    if message_is_type(msg, "new_connection_state") {
        "new_connection_state"
    } else {
        "unknown"
    }
}

async fn ensure_single_message_session(hub: &Hub, socket: WebSocket) -> Option<WebSocket> {
    // Keep a single active KStars text channel. If another session tries to
    // connect while one is active, reject it to avoid racing writers.
    let guard = hub.kstars_msg_tx.lock().await;
    if guard.is_some() {
        warn!(
            channel = "/message/ekos",
            "Rejecting second KStars message session while one is active"
        );
        let mut socket = socket;
        let _ = socket.send(Message::Close(None)).await;
        return None;
    }
    Some(socket)
}

async fn publish_message_sender(hub: &Hub, tx: mpsc::Sender<String>) {
    let mut guard = hub.kstars_msg_tx.lock().await;
    *guard = Some(tx);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_media_frame(metadata: &str, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0u8; METADATA_PACKET + payload.len()];
        let meta_bytes = metadata.as_bytes();
        let copy_len = meta_bytes.len().min(METADATA_PACKET);
        frame[..copy_len].copy_from_slice(&meta_bytes[..copy_len]);
        frame[METADATA_PACKET..].copy_from_slice(payload);
        frame
    }

    #[test]
    fn decode_media_frame_rejects_short_payload() {
        assert!(decode_media_frame(&vec![0u8; METADATA_PACKET]).is_none());
    }

    #[test]
    fn decode_media_frame_with_json_metadata() {
        let frame = build_media_frame(r#"{"uuid":"+F","ext":"jpg"}"#, &[1, 2, 3]);
        let decoded = decode_media_frame(&frame).expect("frame should decode");
        let parsed: Value = serde_json::from_str(&decoded).expect("decoded json");
        assert_eq!(parsed["type"], "new_preview_image");
        assert_eq!(parsed["payload"]["metadata"]["uuid"], "+F");
        assert_eq!(parsed["payload"]["data"], "AQID");
    }

    #[test]
    fn decode_media_frame_falls_back_to_raw_metadata() {
        let frame = build_media_frame("not-json", &[255]);
        let decoded = decode_media_frame(&frame).expect("frame should decode");
        let parsed: Value = serde_json::from_str(&decoded).expect("decoded json");
        assert_eq!(parsed["payload"]["metadata"]["raw"], "not-json");
    }

    #[test]
    fn message_type_detection_works() {
        assert!(message_is_type(
            r#"{"type":"new_connection_state","payload":{}}"#,
            "new_connection_state"
        ));
        assert!(!message_is_type(r#"{"payload":{}}"#, "new_connection_state"));
        assert!(!message_is_type("not-json", "new_connection_state"));
    }
}
