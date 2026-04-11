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
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::hub::{Hub, CMD_CAP};
use crate::AppState;

/// Metadata header size in binary media frames (from `kstars/ekos/ekoslive/media.h`).
const METADATA_PACKET: usize = 512;

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
    info!("KStars connected to /message/ekos");

    let (mut sink, mut stream) = socket.split();

    // Per-session command queue: browsers push into `cmd_tx`, this handler
    // drains `cmd_rx` and writes to the KStars socket.
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(CMD_CAP);

    // Publish the sender so browsers can reach this session.
    {
        let mut guard = hub.kstars_msg_tx.lock().await;
        *guard = Some(cmd_tx);
    }

    // Prime KStars: tell it a client is ready so it starts emitting responses.
    // Without this, `Node::sendResponse` in KStars silently drops events because
    // `m_ClientState` defaults to false.
    let handshake = r#"{"type":"set_client_state","payload":{"state":true}}"#;
    if let Err(e) = sink.send(Message::Text(handshake.into())).await {
        error!("Failed to send set_client_state handshake to KStars: {e}");
        clear_kstars_sender(&hub).await;
        return;
    }

    // Notify browsers that a KStars session is live.
    let _ = hub.browser_tx.send(
        r#"{"type":"new_connection_state","payload":{"connected":true}}"#.to_string(),
    );

    loop {
        tokio::select! {
            // KStars → browsers
            frame = stream.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        debug!(len = text.len(), "KStars message → browsers");
                        let _ = hub.browser_tx.send(text.to_string());
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if sink.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("KStars closed /message/ekos");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("KStars /message/ekos error: {e}");
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

    let _ = hub.browser_tx.send(
        r#"{"type":"new_connection_state","payload":{"connected":false}}"#.to_string(),
    );

    info!("KStars /message/ekos session ended");
}

async fn clear_kstars_sender(hub: &Hub) {
    let mut guard = hub.kstars_msg_tx.lock().await;
    *guard = None;
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
    info!("KStars connected to /media/ekos");

    let (_sink, mut stream) = socket.split();

    while let Some(frame) = stream.next().await {
        match frame {
            Ok(Message::Binary(data)) => {
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
                info!("KStars closed /media/ekos");
                break;
            }
            Err(e) => {
                warn!("KStars /media/ekos error: {e}");
                break;
            }
            _ => {}
        }
    }

    info!("KStars /media/ekos session ended");
}

/// Decode a binary media frame into a `new_preview_image` JSON string.
///
/// Frame layout (from `kstars/ekos/ekoslive/media.h`):
/// - Bytes 0..512: JSON metadata, null-padded to `METADATA_PACKET` bytes
/// - Bytes 512..: raw JPEG (or FITS) data
fn decode_media_frame(data: &[u8]) -> Option<String> {
    if data.len() <= METADATA_PACKET {
        warn!("Media frame too short: {} bytes", data.len());
        return None;
    }

    let header = &data[..METADATA_PACKET];
    let end = header.iter().position(|&b| b == 0).unwrap_or(METADATA_PACKET);
    let meta_str = std::str::from_utf8(&header[..end]).ok()?;
    let metadata: serde_json::Value = serde_json::from_str(meta_str)
        .unwrap_or_else(|_| serde_json::json!({ "raw": meta_str }));

    let payload = &data[METADATA_PACKET..];
    let data_b64 = BASE64.encode(payload);

    let msg = serde_json::json!({
        "type": "new_preview_image",
        "payload": {
            "metadata": metadata,
            "data":     data_b64,
        }
    });

    Some(msg.to_string())
}
