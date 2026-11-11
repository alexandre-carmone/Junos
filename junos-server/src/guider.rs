//! The link to junos-guider, the PHD2 port in `junos-guider/`.
//!
//! The daemon serves a local WebSocket (`--guider-ws`, default
//! `ws://127.0.0.1:4410`) and the relay dials it, retrying every 1 s up to
//! every 10 s: the daemon runs with or without the relay and restarts on its
//! own. Everything it sends is `guider_*` JSON (anything else is dropped),
//! folded into the replay cache and broadcast to every browser unchanged —
//! except `guider_frame`, which becomes a `+J` `new_preview_image` so the
//! per-browser media filter, the lite variant and the per-kind replay apply
//! to it, and `guider_rpc_result`, which goes to the one browser that made
//! the call. The browsers' `guider_*` messages (`guider_rpc`, intercepted by
//! `proxy.rs`) go the other way with their `id` wrapped as `"b{slot}:{id}"`
//! — every page counts its ids from 1 — and unwrapped in the reply. While
//! the daemon is away a call is answered at once with a `-32603` error.
//!
//! Both ways go through bounded queues filled with `try_send`: a stalled
//! daemon makes the relay drop browser commands rather than block a browser
//! task, and a slow re-encode drops frames rather than hold up the events.
//!
//! The relay asks for frames only while a browser shows them: each browser
//! task holds a `GuiderSlot` with what its media filter wants of `+J`, and
//! the sum goes to the daemon as `set_frame_filter` on every change and on
//! every (re)connect, so the daemon encodes nothing when nobody looks.
//!
//! When the link drops, browsers get `guider_state {attached:false}` (kept
//! for late joiners too) and the other `guider_*` snapshots are forgotten.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::hub::{FrameKind, Hub, Outbound, CMD_CAP, GUIDER_PREFIX};
use crate::kstars_ws::{frame_width, lite_jpeg, LITE_MAX_WIDTH, LITE_QUALITY};

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A session shorter than this does not reset the backoff: a daemon that
/// accepts and drops the link at once must not make it flap every second.
const STABLE_SESSION: Duration = Duration::from_secs(10);
/// A daemon that accepts the connection but never finishes the handshake
/// must not hold the link for the OS TCP timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The idle daemon sends nothing, so the relay pings it this often and
/// drops the link after `SILENCE_LIMIT` without a word (a pong or any
/// message): a hung daemon or a half-open link.
const PING_EVERY: Duration = Duration::from_secs(10);
const SILENCE_LIMIT: Duration = Duration::from_secs(30);
/// Frames waiting for their lite re-encode. More are dropped: the next one
/// is a few seconds away and supersedes them.
const FRAME_CAP: usize = 2;
/// The `id` of the relay's own `guider_rpc` calls, whose replies are not for
/// the browsers.
const RELAY_RPC_ID: &str = "junos-relay";

type DaemonSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What one browser wants of the `+J` frames, from its media filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameWant {
    pub frames: bool,
    /// The 16-bit star crop (`guider.star_image`) for the star profile.
    pub profile: bool,
    /// It takes the lite variant.
    pub lite: bool,
}

/// The daemon's `set_frame_filter`, summed over the browsers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FrameFilter {
    frames: bool,
    profile: bool,
    /// Set only when every browser showing `+J` is lite: nobody needs the
    /// full-size JPEG then, and the daemon may encode the lite one directly.
    max_width: Option<u32>,
    quality: Option<u8>,
}

impl FrameFilter {
    fn sum<'a>(wants: impl Iterator<Item = &'a FrameWant>) -> Self {
        let mut f = Self::default();
        let mut all_lite = true;
        for w in wants.filter(|w| w.frames) {
            f.frames = true;
            f.profile |= w.profile;
            all_lite &= w.lite;
        }
        if f.frames && all_lite {
            f.max_width = Some(LITE_MAX_WIDTH);
            f.quality = Some(LITE_QUALITY);
        }
        f
    }

    fn rpc(&self) -> String {
        let mut kinds = Vec::new();
        if self.frames {
            kinds.push("frame");
        }
        if self.profile {
            kinds.push("profile");
        }
        json!({
            "type": "guider_rpc",
            "payload": {
                "method": "set_frame_filter",
                "id": RELAY_RPC_ID,
                "params": {
                    "frames": self.frames,
                    "profile": self.profile,
                    // The same wish in the spelling of the daemon's method
                    // table (`junos-guider/src/server/rpc.rs`).
                    "kinds": kinds,
                    "max_width": self.max_width,
                    "quality": self.quality,
                },
            },
        })
        .to_string()
    }
}

#[derive(Default)]
struct Wants {
    next_id: u64,
    by_browser: HashMap<u64, FrameWant>,
    /// The filter last queued for the daemon; `None` when that failed, so
    /// the next change goes out whatever it is.
    sent: Option<FrameFilter>,
}

/// The relay's end of the daemon link, shared by every browser task.
#[derive(Clone)]
pub struct GuiderLink {
    /// Browser → daemon messages, drained by the connection task.
    tx: mpsc::Sender<String>,
    wants: Arc<Mutex<Wants>>,
    /// A session with the daemon is up.
    attached: Arc<AtomicBool>,
}

impl GuiderLink {
    /// Start dialing the daemon at `url`, for the life of the server.
    pub fn spawn(hub: Hub, url: String) -> Self {
        let (tx, rx) = mpsc::channel(CMD_CAP);
        let link = Self { tx, wants: Arc::default(), attached: Arc::default() };
        tokio::spawn(link.clone().run(hub, url, rx));
        link
    }

    /// A session with the daemon is up.
    pub fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// Queue browser `browser`'s `guider_*` message for the daemon, its
    /// `id` wrapped so the reply comes back to that browser alone. `Err`
    /// holds the `guider_rpc_result` error the browser gets instead when the
    /// daemon is away or stopped reading.
    fn forward(&self, browser: u64, text: &str) -> Result<(), String> {
        let mut v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let Some(p) = v.get_mut("payload").and_then(Value::as_object_mut) else {
            return Ok(()); // nothing the daemon would act on
        };
        let id = p.get("id").cloned().unwrap_or(Value::Null);
        p.insert("id".into(), json!(format!("b{browser}:{id}")));
        if !self.attached.load(Ordering::SeqCst) {
            return Err(rpc_error(id, "junos-guider is not connected"));
        }
        self.tx.try_send(v.to_string()).map_err(|_| rpc_error(id, "junos-guider is not answering"))
    }

    /// A browser's place in the frame filter.
    pub fn slot(&self) -> GuiderSlot {
        let id = {
            let mut w = self.wants.lock().unwrap_or_else(PoisonError::into_inner);
            w.next_id += 1;
            w.next_id
        };
        GuiderSlot { link: self.clone(), id }
    }

    fn set_want(&self, id: u64, want: Option<FrameWant>) {
        let mut w = self.wants.lock().unwrap_or_else(PoisonError::into_inner);
        match want {
            Some(want) => w.by_browser.insert(id, want),
            None => w.by_browser.remove(&id),
        };
        let filter = FrameFilter::sum(w.by_browser.values());
        if w.sent == Some(filter) {
            return;
        }
        // Queued under the lock, so two browsers' changes reach the daemon
        // in the order `sent` saw them. Lost while the daemon is away (each
        // (re)connect sends the current filter anyway) or stalled.
        w.sent = self.tx.try_send(filter.rpc()).is_ok().then_some(filter);
    }

    fn filter_rpc(&self) -> String {
        let mut w = self.wants.lock().unwrap_or_else(PoisonError::into_inner);
        let filter = FrameFilter::sum(w.by_browser.values());
        w.sent = Some(filter);
        filter.rpc()
    }

    /// Whether a browser showing `+J` takes the lite variant: only then is
    /// a frame re-encoded.
    fn lite_wanted(&self) -> bool {
        let w = self.wants.lock().unwrap_or_else(PoisonError::into_inner);
        w.by_browser.values().any(|w| w.frames && w.lite)
    }

    async fn run(self, hub: Hub, url: String, mut rx: mpsc::Receiver<String>) {
        let (frame_tx, frame_rx) = mpsc::channel(FRAME_CAP);
        tokio::spawn(self.clone().publish_frames(hub.clone(), frame_rx));
        let mut backoff = MIN_BACKOFF;
        let mut told = false;
        loop {
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())).await {
                Ok(Ok((ws, _))) => {
                    info!("junos-guider: connected to {url}");
                    let start = Instant::now();
                    self.session(ws, &hub, &mut rx, &frame_tx).await;
                    info!("junos-guider: link to {url} lost");
                    self.attached.store(false, Ordering::SeqCst);
                    detach(&hub).await;
                    if start.elapsed() >= STABLE_SESSION {
                        backoff = MIN_BACKOFF;
                    }
                    told = false;
                }
                // A relay without the daemon is a normal setup: say it once.
                failure => {
                    let why = match failure {
                        Ok(Err(e)) => e.to_string(),
                        _ => "handshake timed out".to_string(),
                    };
                    if told {
                        debug!("junos-guider: {url} still unreachable: {why}");
                    } else {
                        info!("junos-guider: not reachable at {url} ({why}), retrying");
                        told = true;
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn session(
        &self,
        ws: DaemonSocket,
        hub: &Hub,
        rx: &mut mpsc::Receiver<String>,
        frames: &mpsc::Sender<Value>,
    ) {
        let (mut sink, mut stream) = ws.split();
        // Browser commands queued as the last link dropped are stale (their
        // browser gave up on them); the frame filter is re-sent whole.
        while rx.try_recv().is_ok() {}
        self.attached.store(true, Ordering::SeqCst);
        if sink.send(Message::Text(self.filter_rpc())).await.is_err() {
            return;
        }
        let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                msg = stream.next() => {
                    heard = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(text))) => from_daemon(text, hub, frames).await,
                        // tungstenite queued the reply; `close` flushes it
                        // (a `send` after the peer's close is refused).
                        Some(Ok(Message::Close(_))) => {
                            let _ = sink.close().await;
                            break;
                        }
                        None => break,
                        Some(Err(e)) => {
                            warn!("junos-guider: link error: {e}");
                            break;
                        }
                        // The pongs; tungstenite answers pings itself.
                        Some(Ok(_)) => {}
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > SILENCE_LIMIT {
                        warn!("junos-guider: no word from the daemon for {} s", SILENCE_LIMIT.as_secs());
                        break;
                    }
                    if sink.send(Message::Ping(Vec::new())).await.is_err() {
                        break;
                    }
                }
                cmd = rx.recv() => match cmd {
                    Some(cmd) => {
                        if let Err(e) = sink.send(Message::Text(cmd)).await {
                            warn!("junos-guider: send failed: {e}");
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    }
}

/// One browser's place in the frame filter, released however its
/// connection ends.
pub struct GuiderSlot {
    link: GuiderLink,
    id: u64,
}

impl GuiderSlot {
    pub fn set_wanted(&self, want: FrameWant) {
        self.link.set_want(self.id, Some(want));
    }

    /// This browser's mark on the replies meant for it (`Outbound::Reply`).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Send this browser's `guider_*` message to the daemon (see
    /// `GuiderLink::forward`).
    pub fn forward(&self, text: &str) -> Result<(), String> {
        self.link.forward(self.id, text)
    }
}

impl Drop for GuiderSlot {
    fn drop(&mut self) {
        self.link.set_want(self.id, None);
    }
}

/// The `guider_rpc_result` error a browser gets for a call the daemon
/// cannot answer.
fn rpc_error(id: Value, message: &str) -> String {
    json!({
        "type": "guider_rpc_result",
        "payload": {"id": id, "error": {"code": -32603, "message": message}},
    })
    .to_string()
}

/// The browser and the original `id` a wrapped reply id (`"b{slot}:{id}"`,
/// from `GuiderLink::forward`) stands for.
fn unwrap_id(id: &str) -> Option<(u64, Value)> {
    let (slot, orig) = id.strip_prefix('b')?.split_once(':')?;
    Some((slot.parse().ok()?, serde_json::from_str(orig).ok()?))
}

/// One daemon message: a frame to the re-encode queue, the reply to the
/// relay's own call to the log, a browser's reply to that browser, any other
/// `guider_*` to the replay cache and every browser as is.
async fn from_daemon(text: String, hub: &Hub, frames: &mpsc::Sender<Value>) {
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let ty = v["type"].as_str().unwrap_or_default();
    if !ty.starts_with(GUIDER_PREFIX) {
        // It would escape both `reset_kstars` and `forget_guider`.
        debug!("junos-guider: {ty:?} message dropped");
        return;
    }
    match ty {
        "guider_frame" => {
            if frames.try_send(v["payload"].take()).is_err() {
                debug!("junos-guider: frame dropped, the previous one is still encoding");
            }
        }
        "guider_rpc_result" => {
            let id = &v["payload"]["id"];
            if id == RELAY_RPC_ID {
                if let Some(e) = v["payload"].get("error") {
                    warn!("junos-guider: set_frame_filter refused: {e}");
                }
                return;
            }
            let Some((to, orig)) = id.as_str().and_then(unwrap_id) else {
                debug!("junos-guider: reply to an unknown call dropped: {id}");
                return;
            };
            v["payload"]["id"] = orig;
            let _ = hub.browser_tx.send(Arc::new(Outbound::Reply { to, text: v.to_string() }));
        }
        _ => {
            hub.capture(&text).await;
            hub.broadcast_text(text);
        }
    }
}

impl GuiderLink {
    /// Turn the queued `guider_frame`s into `+J` previews, one at a time.
    async fn publish_frames(self, hub: Hub, mut rx: mpsc::Receiver<Value>) {
        while let Some(payload) = rx.recv().await {
            let want_lite = self.lite_wanted();
            match tokio::task::spawn_blocking(move || guider_preview(&payload, want_lite)).await {
                // The link dropped while the frame waited or encoded: `detach`
                // has cleared the `+J` frame, and the browsers left the view.
                Ok(Some(_)) if !self.attached.load(Ordering::SeqCst) => {}
                Ok(Some(out)) => {
                    let out = Arc::new(out);
                    hub.capture_preview(out.clone()).await;
                    let _ = hub.browser_tx.send(out);
                }
                Ok(None) => warn!("junos-guider: malformed guider_frame dropped"),
                Err(e) => warn!("junos-guider: frame task failed: {e}"),
            }
        }
    }
}

/// The daemon went away: browsers fall back to the Ekos guide UI.
async fn detach(hub: &Hub) {
    hub.forget_guider().await;
    hub.clear_preview(FrameKind::Guider).await;
    let msg = json!({ "type": "guider_state", "payload": { "attached": false } }).to_string();
    hub.capture(&msg).await;
    hub.broadcast_text(msg);
}

/// The `+J` preview of a `guider_frame` payload — `{seq, metadata{resolution,
/// frame, exposure_ms, binning, subframe, scale_image}, data (b64 JPEG),
/// stats{min, max, median, filt_min, filt_max}, guider{…}}`, the overlay in
/// the frame's pixels — as the `new_preview_image` a KStars media frame
/// becomes, `stats` (the frame's ADU, the stretch's bounds) kept as is, with
/// the lite variant when a browser asked for one.
fn guider_preview(payload: &Value, want_lite: bool) -> Option<Outbound> {
    let data = payload.get("data")?.as_str()?;
    let jpeg = BASE64.decode(data).ok()?;
    let (w, h) = image::ImageReader::with_format(Cursor::new(&jpeg), image::ImageFormat::Jpeg)
        .into_dimensions()
        .ok()?;
    let mut metadata = match payload.get("metadata") {
        Some(Value::Object(m)) => Value::Object(m.clone()),
        _ => json!({}),
    };
    // A new uuid per frame, so the browser's `<img>` always reloads.
    let seq = payload.get("seq").and_then(Value::as_u64).unwrap_or(0);
    metadata["uuid"] = json!(format!("+J-{seq}"));
    metadata["ext"] = json!("jpg");
    if metadata.get("resolution").is_none() {
        metadata["resolution"] = json!(format!("{w}x{h}"));
    }
    // 1 unless the daemon encoded below the frame's size (`max_width`).
    let ratio = frame_width(&metadata).filter(|fw| *fw > 0.0).map_or(1.0, |fw| w as f64 / fw);
    let full = preview_text(payload, &metadata, data.to_string(), ratio);
    // A frame already within the lite width goes out as is to lite browsers
    // too: re-encoding it would only lose quality.
    let lite = if want_lite && w > LITE_MAX_WIDTH {
        lite_jpeg(&jpeg, LITE_MAX_WIDTH, LITE_QUALITY)
            .map(|(bytes, r)| preview_text(payload, &metadata, BASE64.encode(bytes), ratio * r))
    } else {
        None
    };
    Some(Outbound::Preview { kind: FrameKind::Guider, full, lite })
}

/// One variant of the preview of `frame` (the `guider_frame` payload): its
/// JPEG, the overlay scaled by `ratio`, the stats as they came.
fn preview_text(frame: &Value, metadata: &Value, data_b64: String, ratio: f64) -> String {
    let mut payload = json!({ "metadata": metadata, "data": data_b64 });
    if let Some(g) = frame.get("guider") {
        payload["guider"] = scale_guider_overlay(g, ratio);
    }
    if let Some(s) = frame.get("stats") {
        payload["stats"] = s.clone();
    }
    json!({ "type": "new_preview_image", "payload": payload }).to_string()
}

/// The `guider` overlay in the pixels of a JPEG `ratio` times the frame's
/// width, as `kstars_ws::preview_json` scales the focus stars: the positions
/// (`lock`, `star`, `stars`), the search box half-size and the lock-shift
/// vector. The measured values (mass, SNR, HFD, FWHM, peak) stay as the
/// daemon gave them, since the UI prints them, and so does `star_image`, the
/// raw 16-bit crop around the star.
fn scale_guider_overlay(overlay: &Value, ratio: f64) -> Value {
    let mut o = overlay.clone();
    if ratio == 1.0 {
        return o;
    }
    if let Some(m) = o.as_object_mut() {
        for key in ["lock", "lock_shift_px_h"] {
            if let Some(Value::Array(xy)) = m.get_mut(key) {
                xy.iter_mut().for_each(|v| scale(v, ratio));
            }
        }
        if let Some(v) = m.get_mut("search_region") {
            scale(v, ratio);
        }
        if let Some(star) = m.get_mut("star") {
            scale_xy(star, ratio);
        }
        if let Some(Value::Array(stars)) = m.get_mut("stars") {
            stars.iter_mut().for_each(|s| scale_xy(s, ratio));
        }
    }
    o
}

fn scale(v: &mut Value, ratio: f64) {
    if let Some(x) = v.as_f64() {
        *v = json!(x * ratio);
    }
}

fn scale_xy(p: &mut Value, ratio: f64) {
    for k in ["x", "y"] {
        if let Some(v) = p.get_mut(k) {
            scale(v, ratio);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_positions_scale_and_measurements_do_not() {
        let overlay = json!({
            "lock": [100.0, 50.0],
            "star": {"x": 101.0, "y": 49.0, "mass": 5000.0, "snr": 30.0, "hfd": 2.4, "fwhm": 2.1, "peak": 900, "err": 0, "saturated": false},
            "stars": [{"x": 101.0, "y": 49.0, "snr": 30.0, "hfd": 2.4, "mass": 5000.0, "primary": true, "lost": false},
                      {"x": 300.0, "y": 200.0, "snr": 12.0, "hfd": 3.0, "mass": 900.0, "primary": false, "lost": true}],
            "search_region": 15,
            "lock_shift_px_h": [2.0, -4.0],
            "star_image": {"width": 31, "height": 31, "star_pos": [15.2, 14.8], "pixels": "AAEC"},
        });
        let s = scale_guider_overlay(&overlay, 0.5);
        assert_eq!(s["lock"], json!([50.0, 25.0]));
        assert_eq!(s["star"]["x"], 50.5);
        assert_eq!(s["star"]["y"], 24.5);
        assert_eq!(s["star"]["hfd"], 2.4);
        assert_eq!(s["star"]["snr"], 30.0);
        assert_eq!(s["star"]["mass"], 5000.0);
        assert_eq!(s["stars"][1]["x"], 150.0);
        assert_eq!(s["stars"][1]["y"], 100.0);
        assert_eq!(s["stars"][1]["lost"], true);
        assert_eq!(s["stars"][0]["primary"], true);
        assert_eq!(s["search_region"], 7.5);
        assert_eq!(s["lock_shift_px_h"], json!([1.0, -2.0]));
        assert_eq!(s["star_image"], overlay["star_image"]);
        // The full-size variant goes out untouched, integers included.
        assert_eq!(scale_guider_overlay(&overlay, 1.0), overlay);
        // A lost lock or star is null, and stays so.
        let none = json!({"lock": null, "star": null, "stars": [], "star_image": null});
        assert_eq!(scale_guider_overlay(&none, 0.5), none);
    }

    fn gray_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        image::GrayImage::from_fn(w, h, |x, _| image::Luma([(x % 256) as u8]))
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
            .unwrap();
        buf
    }

    fn frame(jpeg: &[u8]) -> Value {
        json!({
            "seq": 7,
            "metadata": {"exposure_ms": 2000, "binning": 1, "subframe": null},
            "data": BASE64.encode(jpeg),
            "stats": {"min": 120, "max": 65535, "median": 830, "filt_min": 300, "filt_max": 4100},
            "guider": {"lock": [600.0, 20.0], "star_image": {"width": 3, "height": 1, "star_pos": [1, 0], "pixels": "AAEAAgAD"}},
        })
    }

    #[test]
    fn guider_frame_becomes_a_plus_j_preview() {
        let Some(Outbound::Preview { kind, full, lite }) = guider_preview(&frame(&gray_jpeg(1200, 40)), true) else {
            panic!("frame should convert");
        };
        assert_eq!(kind, FrameKind::Guider);
        let full: Value = serde_json::from_str(&full).unwrap();
        assert_eq!(full["type"], "new_preview_image");
        let meta = &full["payload"]["metadata"];
        assert_eq!(meta["uuid"], "+J-7");
        assert_eq!(meta["ext"], "jpg");
        assert_eq!(meta["resolution"], "1200x40");
        assert_eq!(meta["exposure_ms"], 2000);
        assert_eq!(full["payload"]["data"], frame(&gray_jpeg(1200, 40))["data"]);
        assert_eq!(full["payload"]["guider"]["lock"], json!([600.0, 20.0]));

        // 960 / 1200: the overlay follows the lite pixels, the crop does not.
        let lite: Value = serde_json::from_str(&lite.expect("lite variant requested")).unwrap();
        assert_eq!(lite["payload"]["metadata"]["resolution"], "1200x40");
        let lock = lite["payload"]["guider"]["lock"].as_array().unwrap();
        assert!((lock[0].as_f64().unwrap() - 480.0).abs() < 1e-9);
        assert!((lock[1].as_f64().unwrap() - 16.0).abs() < 1e-9);
        assert_eq!(lite["payload"]["guider"]["star_image"], full["payload"]["guider"]["star_image"]);
        // The ADU statistics go out in both variants, untouched.
        let stats = json!({"min": 120, "max": 65535, "median": 830, "filt_min": 300, "filt_max": 4100});
        assert_eq!(full["payload"]["stats"], stats);
        assert_eq!(lite["payload"]["stats"], stats);
    }

    #[test]
    fn a_narrow_or_downsized_frame_gets_no_lite_reencode() {
        let Some(Outbound::Preview { lite, .. }) = guider_preview(&frame(&gray_jpeg(640, 40)), true) else {
            panic!("frame should convert");
        };
        assert!(lite.is_none());

        // The daemon encoded at a set_frame_filter max_width: the overlay,
        // in frame pixels, is brought onto the JPEG's.
        let mut f = frame(&gray_jpeg(960, 30));
        f["metadata"]["resolution"] = json!("1920x60");
        let Some(Outbound::Preview { full, lite, .. }) = guider_preview(&f, true) else {
            panic!("frame should convert");
        };
        assert!(lite.is_none());
        let full: Value = serde_json::from_str(&full).unwrap();
        assert_eq!(full["payload"]["metadata"]["resolution"], "1920x60");
        assert_eq!(full["payload"]["guider"]["lock"], json!([300.0, 10.0]));

        assert!(guider_preview(&json!({"seq": 1, "data": "not base64!"}), false).is_none());
    }

    #[test]
    fn a_browser_call_is_wrapped_and_its_reply_unwrapped() {
        let (tx, mut rx) = mpsc::channel(8);
        let link = GuiderLink { tx, wants: Arc::default(), attached: Arc::default() };
        let call = r#"{"payload":{"id":3,"method":"loop"},"type":"guider_rpc"}"#;
        // The daemon is away: the browser is answered at once.
        let err: Value = serde_json::from_str(&link.forward(7, call).unwrap_err()).unwrap();
        assert_eq!(err["type"], "guider_rpc_result");
        assert_eq!(
            err["payload"],
            json!({"id": 3, "error": {"code": -32603, "message": "junos-guider is not connected"}})
        );
        assert!(rx.try_recv().is_err());

        link.attached.store(true, Ordering::SeqCst);
        link.forward(7, call).unwrap();
        let sent: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(sent["payload"]["id"], "b7:3");
        assert_eq!(sent["payload"]["method"], "loop");
        assert_eq!(unwrap_id("b7:3"), Some((7, json!(3))));
        assert_eq!(unwrap_id(r#"b12:"abc""#), Some((12, json!("abc"))));
        assert_eq!(unwrap_id("b4:null"), Some((4, Value::Null)));
        assert_eq!(unwrap_id(RELAY_RPC_ID), None);
    }

    #[test]
    fn frame_filter_sums_the_browsers_and_is_sent_on_change_only() {
        let (tx, mut rx) = mpsc::channel(8);
        let link = GuiderLink { tx, wants: Arc::default(), attached: Arc::default() };
        let params = |rpc: String| -> Value {
            let v: Value = serde_json::from_str(&rpc).unwrap();
            assert_eq!(v["type"], "guider_rpc");
            assert_eq!(v["payload"]["method"], "set_frame_filter");
            assert_eq!(v["payload"]["id"], RELAY_RPC_ID);
            v["payload"]["params"].clone()
        };

        let phone = link.slot();
        let desktop = link.slot();
        phone.set_wanted(FrameWant { frames: true, profile: false, lite: true });
        let p = params(rx.try_recv().unwrap());
        assert_eq!(p["frames"], true);
        assert_eq!(p["profile"], false);
        assert_eq!(p["kinds"], json!(["frame"]));
        assert_eq!(p["max_width"], LITE_MAX_WIDTH);
        assert_eq!(p["quality"], LITE_QUALITY);

        // A browser on another tab changes nothing.
        desktop.set_wanted(FrameWant { frames: false, profile: true, lite: false });
        assert!(rx.try_recv().is_err());

        // A full-size browser on the Guide tab: no width cap, its profile.
        desktop.set_wanted(FrameWant { frames: true, profile: true, lite: false });
        let p = params(rx.try_recv().unwrap());
        assert_eq!(p["kinds"], json!(["frame", "profile"]));
        assert_eq!(p["max_width"], Value::Null);
        assert_eq!(p["quality"], Value::Null);

        drop(desktop);
        assert_eq!(params(rx.try_recv().unwrap())["max_width"], LITE_MAX_WIDTH);
        drop(phone);
        let p = params(rx.try_recv().unwrap());
        assert_eq!(p["frames"], false);
        assert_eq!(p["kinds"], json!([]));
        assert_eq!(params(link.filter_rpc()), p);
    }

    type FakeDaemon = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn daemon_text(ws: &mut FakeDaemon) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("the relay is silent");
            if let Some(Ok(Message::Text(t))) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    async fn browser_next(rx: &mut tokio::sync::broadcast::Receiver<Arc<Outbound>>) -> Arc<Outbound> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("nothing for the browsers").unwrap()
    }

    #[tokio::test]
    async fn the_link_routes_the_daemon_and_detaches_cleanly() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = Hub::new();
        let mut browsers = hub.subscribe();
        let link = GuiderLink::spawn(hub.clone(), format!("ws://{}", listener.local_addr().unwrap()));
        let browser = link.slot();
        let mut daemon = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0).await.unwrap();
        assert_eq!(daemon_text(&mut daemon).await["payload"]["method"], "set_frame_filter");

        // A browser's call reaches the daemon with its id wrapped.
        let call = r#"{"type":"guider_rpc","payload":{"method":"loop","id":1}}"#;
        browser.forward(call).unwrap();
        let sent = daemon_text(&mut daemon).await;
        assert_eq!(sent["payload"]["method"], "loop");

        for m in [
            json!({"type": "guider_rpc_result", "payload": {"id": RELAY_RPC_ID, "result": 0}}),
            json!({"type": "guider_rpc_result", "payload": {"id": sent["payload"]["id"], "result": 0}}),
            json!({"type": "new_focus_state", "payload": {}}),
            json!({"type": "guider_settings", "payload": {"a": 1}}),
            json!({"type": "guider_frame", "payload": frame(&gray_jpeg(64, 16))}),
        ] {
            daemon.send(Message::Text(m.to_string())).await.unwrap();
        }
        // The relay's own reply and the non-guider message stop at the relay.
        match browser_next(&mut browsers).await.as_ref() {
            Outbound::Reply { to, text } => {
                assert_eq!(*to, browser.id());
                assert_eq!(serde_json::from_str::<Value>(text).unwrap()["payload"], json!({"id": 1, "result": 0}));
            }
            other => panic!("expected the reply, got {other:?}"),
        }
        assert!(matches!(browser_next(&mut browsers).await.as_ref(), Outbound::Text(t) if t.contains("guider_settings")));
        assert!(matches!(
            browser_next(&mut browsers).await.as_ref(),
            Outbound::Preview { kind: FrameKind::Guider, .. }
        ));
        assert_eq!(hub.replay_snapshot().await.len(), 1);
        assert!(hub.last_preview(FrameKind::Guider).await.is_some());

        // The daemon leaves: the relay answers its close, the browsers are
        // told, the guider replay goes and calls fail at once.
        daemon.close(None).await.unwrap();
        let mut closed = false;
        while let Ok(Some(Ok(m))) = tokio::time::timeout(Duration::from_secs(5), daemon.next()).await {
            closed |= m.is_close();
        }
        assert!(closed, "the relay did not answer the close");
        assert!(matches!(browser_next(&mut browsers).await.as_ref(), Outbound::Text(t) if t.contains(r#""attached":false"#)));
        let snap = hub.replay_snapshot().await;
        assert_eq!(snap.len(), 1);
        assert!(snap[0].contains(r#""attached":false"#));
        assert!(hub.last_preview(FrameKind::Guider).await.is_none());
        assert!(browser.forward(call).is_err());

        // A change while the daemon is away goes out whole on the reconnect.
        browser.set_wanted(FrameWant { frames: true, profile: false, lite: false });
        let mut daemon = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0).await.unwrap();
        let filter = daemon_text(&mut daemon).await;
        assert_eq!(filter["payload"]["method"], "set_frame_filter");
        assert_eq!(filter["payload"]["params"]["frames"], true);
        browser.forward(call).unwrap();
    }
}
