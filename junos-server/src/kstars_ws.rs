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

    // Prime the observer site once per attach. KStars answers with
    // {name, latitude, longitude, elevation, tz, tz0}; that reply is snapshotted
    // below and replayed to every browser on connect. Not gated on Ekos-online
    // (handled before KStars' startup gate, message.cpp:251).
    if let Err(e) = sink.send(Message::Text(astro_get_location_msg().into())).await {
        warn!("Failed to request astro_get_location from KStars: {e}");
    }

    // KStars never pushes a location change over Ekos Live (astro_get_location
    // is response-only, message.cpp:1861), so re-poll it periodically to pick
    // up site edits made in KStars after attach. Replies are only forwarded to
    // browsers when the value actually changed (see the stream branch), so a
    // steady site costs nothing beyond the request.
    let mut site_poll = tokio::time::interval(std::time::Duration::from_secs(10));
    site_poll.tick().await; // consume the immediate first tick — we just primed above

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
                        // Observer site: cache for late-joining browsers, but
                        // only forward when it actually changed so the periodic
                        // poll doesn't spam browsers with identical updates.
                        if message_is_type(&text, "astro_get_location") {
                            let mut cached = hub.last_site_location.lock().await;
                            if cached.as_deref() == Some(text.as_str()) {
                                continue; // unchanged — swallow
                            }
                            *cached = Some(text.to_string());
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

            // Periodically re-query the observer site to catch KStars-side edits.
            _ = site_poll.tick() => {
                if let Err(e) = sink.send(Message::Text(astro_get_location_msg().into())).await {
                    warn!("Failed to poll astro_get_location from KStars: {e}");
                    break;
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
    *hub.last_site_location.lock().await = None;
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

    let mut out_payload = json!({
        "metadata": metadata,
        "data":     data_b64,
    });

    // Focus frames (uuid "+F", kstars media.cpp:752) get a server-side star
    // detection pass so the browser can overlay detected stars with a per-star
    // HFR size ring. We detect on the exact JPEG that gets displayed, so the
    // star coordinates map straight onto the preview image with no rescaling.
    let uuid = metadata.get("uuid").and_then(Value::as_str).unwrap_or("");
    let ext = metadata.get("ext").and_then(Value::as_str).unwrap_or("");
    if uuid.starts_with("+F") && ext == "jpg" {
        if let Some((stars, w, h)) = detect_focus_stars(payload) {
            let arr: Vec<Value> = stars
                .iter()
                .map(|s| json!({ "x": s.x, "y": s.y, "hfr": s.hfr }))
                .collect();
            out_payload["stars"] = Value::Array(arr);
            out_payload["star_w"] = json!(w);
            out_payload["star_h"] = json!(h);
        }
    }

    let msg = json!({
        "type": "new_preview_image",
        "payload": out_payload,
    });

    Some(msg.to_string())
}

/// Decode a focus JPEG and detect its stars. Returns `(stars, width, height)`
/// in JPEG pixel space, or `None` if the payload isn't a decodable JPEG.
fn detect_focus_stars(jpeg: &[u8]) -> Option<(Vec<crate::starfind::Star>, u32, u32)> {
    use image::ImageFormat;
    let img = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg).ok()?;
    let luma = img.to_luma8();
    let (w, h) = luma.dimensions();
    let plane: Vec<f32> = luma.iter().map(|&p| p as f32).collect();
    let stars = crate::starfind::detect_stars(
        &plane,
        w as usize,
        h as usize,
        &crate::starfind::DetectParams::default(),
    );
    Some((stars, w, h))
}

fn set_client_state_msg() -> String {
    json!({
        "type": "set_client_state",
        "payload": { "state": true }
    })
    .to_string()
}

/// Ask KStars for the observer's geographic site. Sent once per KStars attach;
/// the reply (`astro_get_location`) is cached and replayed to browsers.
fn astro_get_location_msg() -> String {
    json!({
        "type": "astro_get_location",
        "payload": {}
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

    /// Encode a small grayscale image with a few bright blobs as a JPEG.
    fn synthetic_star_jpeg(w: u32, h: u32) -> Vec<u8> {
        use image::{codecs::jpeg::JpegEncoder, ExtendedColorType};
        let mut buf = vec![10u8; (w * h) as usize];
        for &(cx, cy) in &[(20u32, 20u32), (60, 40), (40, 70)] {
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    let x = cx as i32 + dx;
                    let y = cy as i32 + dy;
                    if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                        buf[(y as u32 * w + x as u32) as usize] = 240;
                    }
                }
            }
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode(&buf, w, h, ExtendedColorType::L8)
            .expect("encode jpeg");
        jpeg
    }

    #[test]
    fn focus_frame_gains_stars() {
        let jpeg = synthetic_star_jpeg(96, 96);
        let frame = build_media_frame(r#"{"uuid":"+F","ext":"jpg"}"#, &jpeg);
        let decoded = decode_media_frame(&frame).expect("frame should decode");
        let parsed: Value = serde_json::from_str(&decoded).expect("decoded json");
        let stars = parsed["payload"]["stars"].as_array().expect("stars array");
        assert!(!stars.is_empty(), "focus frame should yield detected stars");
        assert!(parsed["payload"]["star_w"].as_u64().unwrap() > 0);
        assert!(stars[0]["hfr"].is_number());
    }

    #[test]
    fn non_focus_frame_has_no_stars() {
        let jpeg = synthetic_star_jpeg(96, 96);
        let frame = build_media_frame(r#"{"uuid":"+A","ext":"jpg"}"#, &jpeg);
        let decoded = decode_media_frame(&frame).expect("frame should decode");
        let parsed: Value = serde_json::from_str(&decoded).expect("decoded json");
        assert!(parsed["payload"].get("stars").is_none(), "align frame must not detect stars");
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
