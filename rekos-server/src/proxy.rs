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

    // On connect, tell the browser whether KStars is currently attached.
    let connected = hub.kstars_msg_tx.lock().await.is_some();
    let init = format!(
        r#"{{"type":"new_connection_state","payload":{{"connected":{}}}}}"#,
        connected
    );
    let _ = sink.send(Message::Text(init.into())).await;

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
