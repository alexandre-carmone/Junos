//! Outbound WebSocket handler for browsers (`/ws`).
//!
//! Browsers subscribe to the KStars event broadcast and push commands into
//! the per-session KStars queue via the shared `Hub`.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use tracing::{debug, warn};

use crate::apps::build_app_state_msg;
use crate::AppState;

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

    // Replay the cached KStars state (last preview image, recent guiding
    // metrics, FOV geometry) so a dropped/refreshed browser recovers it
    // immediately instead of waiting for KStars to push again.
    for msg in hub.replay_snapshot().await {
        if sink.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            // KStars event → browser
            result = kstars_rx.recv() => {
                match result {
                    Ok(msg) => {
                        if sink.send(Message::Text(msg.into())).await.is_err() {
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

            // Browser command → KStars
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
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
