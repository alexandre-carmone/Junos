//! The local WebSocket the junos-server relay dials (`--ws-addr`, default
//! `127.0.0.1:4410`): the daemon's state for the junos-web Guide tab as
//! `{"type":"guider_*","payload":…}` texts, and the tab's commands back.
//!
//! **Snapshots** (sticky: the relay replays the last of each type) go to
//! each new connection first, `guider_state` leading, then whenever their
//! text changes: `guider_state` (the engine's snapshot; also re-sent every
//! 5 s while capturing or a tool runs), `guider_settings` (every profile
//! key under PHD2's name, plus `algo_params`), `guider_profiles`,
//! `guider_calibration`, `guider_devices`, `guider_darks`, `guider_ga`,
//! `guider_pa`, `guider_tools` (the secondary tools' runs); and `guider_history` — the last 300 s (at most 2000) of
//! guide steps, calibration steps and state changes, kept here from the
//! events — sent on connect and on `get_history` only, the events carrying
//! it on. What lives in the profile is read on the engine thread after each
//! `ConfigurationChange` (its getters' debug lines dropped: these reads are
//! not PHD2's).
//!
//! **Deltas**: `guider_event` (the PHD2 event as the event server writes
//! it, [`events::event`]),
//! `guider_alert {Type, Msg}`, `guider_log {line}` (the guide log's INFO
//! and calibration lines).
//!
//! **Commands**: `guider_rpc {method, params, id}`, answered by
//! `guider_rpc_result {id, result | error{code, message}}` through the
//! event server's dispatcher ([`methods::run`], the UI's extras
//! included); this socket's own are `set_frame_filter` and `get_history`.
//! Each call runs in its own task, so a slow one (a connect) holds none
//! of the others. `shutdown` stops capture and disconnects once its reply
//! is out, without ending the process: the relay is the UI, not Ekos.
//!
//! **Frames**: while the relay asks (`set_frame_filter {frames, profile,
//! max_width, quality}`, the sum of what its browsers show), each frame
//! goes out as `guider_frame {seq, metadata{resolution, frame, exposure_ms,
//! binning, subframe, scale_image}, data, stats, guider{lock, star, stars,
//! search_region, lock_shift_px_h, star_image}}`: a JPEG of the frame
//! stretched as PHD2 displays it (between its filtered min and max, with
//! `/Gamma`; `/guider/ScaleImage`, the fit to the window, is the
//! browser's to honour), the overlay in frame pixels, and with `profile`
//! the 16-bit crop `get_star_image` gives. Encoding runs off the hub; a
//! frame that comes while the previous one encodes is dropped.
//!
//! Ported from PHD2 src/usImage.cpp (buildGammaLookupTable, CopyToImage) — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::Message;

use crate::daemon::Daemon;
use crate::engine::algo::MountClass;
use crate::engine::assistant::{self, GaDetails};
use crate::engine::calibration;
use crate::engine::guider::{PauseType, ShiftUnits};
use crate::engine::image::UsImage;
use crate::engine::log::debug_log::PHD2_VERSION;
use crate::engine::star::FindResult;
use crate::engine::traits::Parity;
use crate::engine::{AlgoView, CalibrationView, Engine, EngineEvent, Snapshot};
use crate::server::events::{self, Origin};
use crate::server::rpc::{self, Call, RpcError};
use crate::server::{extras, methods};

/// How far back `guider_history` reaches, and how much it holds.
const HISTORY_SECS: f64 = 300.0;
const HISTORY_CAP: usize = 2000;
/// `guider_state` again while active, for a relay that lost one.
const STATE_EVERY: Duration = Duration::from_secs(5);
/// Messages kept for a connection that lags.
const BACKLOG: usize = 1024;

/// What the relay asked of the frames (`set_frame_filter`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameFilter {
    pub frames: bool,
    /// The 16-bit star crop with each frame.
    pub profile: bool,
    pub max_width: Option<u32>,
    pub quality: Option<u8>,
}

/// What every connection shares.
struct Shared {
    /// The last text of each snapshot type.
    stickies: Mutex<BTreeMap<&'static str, String>>,
    /// `guider_state` as last compared, without its time stamp.
    state_key: Mutex<String>,
    out: broadcast::Sender<Arc<String>>,
    history: Mutex<History>,
    filter: Mutex<FrameFilter>,
    /// The profile changed: read it again.
    refresh: Notify,
    /// A frame is being encoded: the next ones are dropped meanwhile.
    encoding: AtomicBool,
}

#[derive(Default)]
struct History {
    /// `(Timestamp, event text)`.
    events: VecDeque<(f64, String)>,
    states: VecDeque<(f64, &'static str)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn message(ty: &str, payload: &str) -> String {
    format!("{{\"type\":\"{ty}\",\"payload\":{payload}}}")
}

impl Shared {
    fn new() -> Shared {
        Shared {
            stickies: Mutex::default(),
            state_key: Mutex::default(),
            out: broadcast::channel(BACKLOG).0,
            history: Mutex::default(),
            filter: Mutex::default(),
            refresh: Notify::new(),
            encoding: AtomicBool::new(false),
        }
    }

    /// A delta, to every connection.
    fn send(&self, ty: &str, payload: &str) {
        let _ = self.out.send(Arc::new(message(ty, payload)));
    }

    /// A snapshot, kept and sent when it changed (under the lock, so a new
    /// connection gets it either way and never twice).
    fn sticky(&self, ty: &'static str, payload: &Value) {
        let msg = message(ty, &payload.to_string());
        let mut st = lock(&self.stickies);
        if st.get(ty) != Some(&msg) {
            st.insert(ty, msg.clone());
            let _ = self.out.send(Arc::new(msg));
        }
    }

    /// `guider_state` when it changed or `force`, stamped with the time.
    fn state(&self, mut payload: Value, force: bool) {
        let key = payload.to_string();
        {
            let mut last = lock(&self.state_key);
            if *last == key && !force {
                return;
            }
            *last = key;
        }
        payload["Timestamp"] = rpc::num_f(events::now(), 3);
        let msg = message("guider_state", &payload.to_string());
        let mut st = lock(&self.stickies);
        st.insert("guider_state", msg.clone());
        let _ = self.out.send(Arc::new(msg));
    }

    fn history_payload(&self) -> String {
        let h = lock(&self.history);
        let events: Vec<&str> = h.events.iter().map(|(_, e)| e.as_str()).collect();
        let states: Vec<String> = h
            .states
            .iter()
            .map(|(t, s)| json!({"Timestamp": rpc::num_f(*t, 3), "app_state": s}).to_string())
            .collect();
        format!("{{\"events\":[{}],\"states\":[{}]}}", events.join(","), states.join(","))
    }
}

/// What the profile holds, read on the engine thread.
#[derive(Default)]
struct ProfilePart {
    settings: Map<String, Value>,
    profiles: Value,
    /// The scope's and the AO's stored calibration and its details.
    cal: [Value; 2],
    ga_history: Vec<String>,
    ga_last: Value,
}

/// The hub's own state, from the events.
#[derive(Default)]
struct Track {
    settling: Option<Value>,
    limit_reached: bool,
    app_state: Option<&'static str>,
    frame_seq: u64,
}

/// Binds `addr` and serves the relay from there on; the bound address
/// (`port 0` picks one, for the tests).
pub async fn spawn(d: Daemon, addr: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let sh = Arc::new(Shared::new());
    // The snapshots exist before anyone connects.
    let part = read_part(&d).await;
    let mut track = Track::default();
    publish(&d, &sh, &d.snapshot(), &part, &mut track, false);
    tokio::spawn(hub(d.clone(), sh.clone(), part, track));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(client(stream, sh.clone(), d.clone()));
                }
                Err(e) => {
                    tracing::warn!("relay socket: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok(local)
}

async fn read_part(d: &Daemon) -> ProfilePart {
    d.with_engine(profile_part).await.unwrap_or_default()
}

fn profile_part(e: &mut Engine) -> ProfilePart {
    // The engine's own getter lines go to the debug log first.
    for l in e.store().take_debug() {
        e.debug(&l);
    }
    let mut settings: Map<String, Value> = e.store().profile_entries().map(|(k, v)| (k.to_string(), v.clone())).collect();
    // The globals the settings sheet edits, beside the profile's keys.
    for (k, v) in e.store().global_entries().filter(|(k, _)| extras::GLOBAL_KEYS.contains(k)) {
        settings.insert(k.to_string(), v.clone());
    }
    let profiles: Vec<Value> =
        e.store().profiles().into_iter().map(|(id, name, sel)| json!({"id": id, "name": name, "selected": sel})).collect();
    let p = e.store().profile();
    let cal = [MountClass::Scope, MountClass::StepGuider].map(|class| {
        let det = calibration::load_calibration_details(&p, class);
        let stored = calibration::load_calibration(&p, class);
        let steps = |v: &[crate::engine::traits::Point]| v.iter().map(|s| json!([s.x, s.y])).collect::<Vec<_>>();
        json!({
            "pier_side": stored.as_ref().map(|c| c.pier_side.label()),
            "binning": stored.as_ref().map(|c| c.binning),
            "timestamp": stored.as_ref().map(|c| c.timestamp.clone()),
            "ortho_error": det.ortho_error,
            "ra_steps": steps(&det.ra_steps),
            "dec_steps": steps(&det.dec_steps),
            "last_issue": format!("{:?}", det.last_issue),
            "focal_length": det.focal_length,
            "image_scale": det.image_scale,
            "ra_guide_rate": det.ra_guide_speed,
            "dec_guide_rate": det.dec_guide_speed,
        })
    });
    let ga_history = assistant::history(&p);
    let ga_last = assistant::last_run(&p).map_or(Value::Null, |(ts, d)| json!({"timestamp": ts, "details": ga_details(&d)}));
    drop(p);
    let _ = e.store().take_debug();
    ProfilePart { settings, profiles: Value::Array(profiles), cal, ga_history, ga_last }
}

fn ga_details(d: &GaDetails) -> Value {
    let mut d = d.clone();
    let mut m: Map<String, Value> = d.string_fields().into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    m.insert("recommendations".into(), json!(d.recommendations));
    m.insert("blt_north".into(), json!(d.blt_north));
    m.insert("blt_south".into(), json!(d.blt_south));
    m.insert("blt_msmt_pulse".into(), json!(d.blt_msmt_pulse));
    m.insert("blt_amount".into(), json!(d.blt_amount));
    Value::Object(m)
}

/// The events, the snapshots and the profile, turned into messages.
async fn hub(d: Daemon, sh: Arc<Shared>, mut part: ProfilePart, mut track: Track) {
    let origin = Origin::new(d.options().engine.instance);
    let mut events = d.subscribe();
    let mut snaps = d.snapshots();
    let mut frames = d.frames();
    let (ptx, mut prx) = mpsc::channel(1);
    let (d2, sh2) = (d.clone(), sh.clone());
    tokio::spawn(async move {
        loop {
            sh2.refresh.notified().await;
            if ptx.send(read_part(&d2).await).await.is_err() {
                return;
            }
        }
    });
    let mut tick = tokio::time::interval(STATE_EVERY);
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(ev) => on_event(&ev, &sh, &origin, &mut track),
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            },
            r = snaps.changed() => {
                if r.is_err() {
                    return;
                }
                let s = snaps.borrow_and_update().clone();
                publish(&d, &sh, &s, &part, &mut track, false);
            }
            r = frames.changed() => {
                if r.is_err() {
                    return;
                }
                // The frame's snapshot was published before it.
                let img = frames.borrow_and_update().clone();
                if let Some(img) = img.filter(|_| lock(&sh.filter).frames) {
                    track.frame_seq += 1;
                    send_frame(&sh, img, d.snapshot(), &part, track.frame_seq);
                }
            }
            Some(p) = prx.recv() => {
                part = p;
                publish(&d, &sh, &d.snapshot(), &part, &mut track, false);
            }
            _ = tick.tick() => {
                let s = d.snapshot();
                let active = s.capture_active || s.ga.is_some() || s.polar_drift.is_some() || s.darks_build.is_some();
                publish(&d, &sh, &s, &part, &mut track, active);
            }
        }
    }
}

fn on_event(ev: &EngineEvent, sh: &Shared, origin: &Origin, track: &mut Track) {
    let now = events::now();
    if let Some(text) = events::event(ev, origin, now) {
        sh.send("guider_event", &text);
        use EngineEvent as E;
        let keep = matches!(
            ev,
            E::GuideStep { .. }
                | E::Calibrating { .. }
                | E::StartCalibration { .. }
                | E::CalibrationComplete { .. }
                | E::CalibrationFailed { .. }
                | E::StartGuiding
                | E::GuidingStopped
                | E::Paused
                | E::Resumed
                | E::GuidingDithered { .. }
                | E::SettleDone { .. }
                | E::StarLost(_)
                | E::LockPositionLost
                | E::LoopingExposuresStopped
        );
        if keep {
            let mut h = lock(&sh.history);
            h.events.push_back((now, text));
            prune(&mut h.events, now);
        }
    }
    match ev {
        EngineEvent::Alert { msg, kind, .. } => {
            sh.send("guider_alert", &json!({"Type": kind.type_str(), "Msg": msg}).to_string());
        }
        EngineEvent::GuideLogLine(line) => sh.send("guider_log", &json!({"line": line}).to_string()),
        EngineEvent::Settling { distance, time, settle_time, star_locked } => {
            track.settling = Some(json!({
                "distance": rpc::num_f(*distance, 2),
                "time": rpc::num_f(*time, 1),
                "settle_time": rpc::num_f(*settle_time, 1),
                "locked": star_locked,
            }));
        }
        EngineEvent::SettleDone { .. } => track.settling = None,
        EngineEvent::LockPositionShiftLimitReached => track.limit_reached = true,
        EngineEvent::LockPositionSet(_) => track.limit_reached = false,
        EngineEvent::ConfigurationChange => sh.refresh.notify_one(),
        _ => {}
    }
}

/// Oldest first, at most 300 s before `now` and 2000 entries.
fn prune<T>(q: &mut VecDeque<(f64, T)>, now: f64) {
    while q.front().is_some_and(|(t, _)| *t < now - HISTORY_SECS) || q.len() > HISTORY_CAP {
        q.pop_front();
    }
}

fn parity_str(p: Parity) -> &'static str {
    match p {
        Parity::Even => "+",
        Parity::Odd => "-",
        Parity::Unknown => "?",
    }
}

fn algo_json(a: &Option<AlgoView>) -> Value {
    let Some(a) = a else { return Value::Null };
    let mut m = Map::new();
    m.insert("algorithmName".into(), json!(a.class_name));
    for (n, v) in &a.params {
        m.insert(n.clone(), rpc::num_g(*v));
    }
    Value::Object(m)
}

fn device(v: &Option<crate::engine::DeviceView>) -> Value {
    v.as_ref().map_or(Value::Null, |v| json!({"name": v.name, "connected": v.connected}))
}

fn calibration_json(v: &CalibrationView, stored: &Value) -> Value {
    if !v.present {
        return Value::Null;
    }
    let mut r = json!({
        "calibrated": v.calibrated,
        "x_angle_deg": v.x_angle.to_degrees(),
        "y_angle_deg": v.y_angle.to_degrees(),
        // px/s, as `get_calibration_data`.
        "x_rate": v.x_rate * 1000.0,
        "y_rate": v.y_rate * 1000.0,
        "x_parity": parity_str(v.x_parity),
        "y_parity": parity_str(v.y_parity),
        "declination": v.declination.to_degrees(),
    });
    if let (Some(r), Some(s)) = (r.as_object_mut(), stored.as_object()) {
        r.extend(s.clone());
    }
    r
}

/// Every snapshot message from the engine's snapshot and the profile.
fn publish(d: &Daemon, sh: &Shared, s: &Snapshot, part: &ProfilePart, track: &mut Track, force: bool) {
    let app_state = s.app_state.map(|a| a.as_str());
    if app_state != track.app_state {
        track.app_state = app_state;
        if let Some(a) = app_state {
            let mut h = lock(&sh.history);
            let now = events::now();
            h.states.push_back((now, a));
            prune(&mut h.states, now);
        }
    }
    sh.state(state_json(d, s, part, track), force);

    let mut settings = part.settings.clone();
    settings.insert("algo_params".into(), json!({"ra": algo_json(&s.algo_x), "dec": algo_json(&s.algo_y)}));
    sh.sticky("guider_settings", &Value::Object(settings));
    sh.sticky("guider_profiles", &part.profiles);
    sh.sticky(
        "guider_calibration",
        &json!({
            "mount": calibration_json(&s.mount_calibration, &part.cal[0]),
            "ao": calibration_json(&s.ao_calibration, &part.cal[1]),
        }),
    );
    sh.sticky("guider_devices", &extras::list_devices(d));
    sh.sticky(
        "guider_darks",
        &json!({
            "dark": {"loaded": s.dark_loaded, "exp_ms": s.dark_exp_ms, "exposures": s.dark_exposures},
            "defect_map": {"loaded": s.defect_map_loaded},
            "build": s.darks_build.as_ref().map(|b| json!({
                "defect_map": b.defect_map, "status": b.status, "progress": [b.progress.0, b.progress.1],
            })),
        }),
    );
    let ga = s.ga.as_ref().map(|g| {
        json!({
            "state": format!("{:?}", g.state),
            "instructions": g.instructions,
            "status": g.status,
            "measuring": g.measuring,
            "measuring_backlash": g.measuring_backlash,
            "wait": g.wait.map(|(secs, backlash)| json!({"seconds": secs, "backlash": backlash})),
            "recommendations": g.recommendations.iter().map(|r| json!({
                "kind": r.kind.tag(), "text": r.text, "can_apply": r.can_apply, "applied": r.applied,
            })).collect::<Vec<_>>(),
            "details": ga_details(&g.details),
        })
    });
    sh.sticky(
        "guider_ga",
        &json!({"open": ga.is_some(), "window": ga, "history": part.ga_history, "last_run": part.ga_last}),
    );
    let drift = s.polar_drift.as_ref().map(|p| {
        json!({
            "phase": format!("{:?}", p.phase),
            "drifting": p.drifting,
            "status": p.status,
            "result": p.result.map(|r| json!({
                "time": r.time, "offset_px": r.offset_px, "alpha_deg": r.alpha_deg,
                "current": [r.current.x, r.current.y], "target": [r.target.x, r.target.y],
                "pa_error_arcmin": r.pa_error_arcmin, "angle_deg": r.angle_deg,
            })),
        })
    });
    sh.sticky("guider_pa", &json!({"drift": drift}));
    sh.sticky("guider_tools", &s.tools);
}

fn state_json(d: &Daemon, s: &Snapshot, part: &ProfilePart, track: &Track) -> Value {
    let o = d.options();
    let indi = match d.indi_client() {
        Some(c) => json!({"host": c.host(), "port": c.port(), "connected": c.lock().is_up()}),
        None => json!({
            "host": part.settings.get("/indi/INDIhost").cloned().unwrap_or(json!(o.indi_host)),
            "port": part.settings.get("/indi/INDIport").cloned().unwrap_or(json!(o.indi_port)),
            "connected": false,
        }),
    };
    let primary = [&s.ao_calibration, &s.mount_calibration].into_iter().find(|c| c.present);
    let l = &s.lock_shift;
    let camera = s.camera.as_ref().map(|c| {
        json!({
            "name": c.name, "connected": c.connected, "gain_supported": s.has_gain_control,
            "frame": [s.frame_size.0, s.frame_size.1], "binning": s.binning,
        })
    });
    json!({
        "attached": true,
        "version": PHD2_VERSION,
        // The event server's, once it listens; why it does not meanwhile.
        "port": s.port,
        "port_error": s.port_error,
        // The engine thread panicked: the guider is about to exit.
        "engine_error": s.engine_error,
        "profile": {"id": s.profile_id, "name": s.profile_name},
        "app_state": s.app_state.map(|a| a.as_str()),
        "guider_state": s.guider_state.map(|g| g.name()),
        "capture_active": s.capture_active,
        "paused": match s.paused { PauseType::None => "none", PauseType::Guiding => "guiding", PauseType::Full => "full" },
        "connecting": s.connecting,
        "settling": if s.settling { track.settling.clone() } else { None },
        "dithering": s.dithering,
        "calibrated": {"mount": s.mount_calibration.calibrated, "ao": s.ao_calibration.calibrated},
        "lock": s.lock_position.map(|p| json!([p.x, p.y])),
        "star": s.star_position.map(|p| json!({"x": p.x, "y": p.y})),
        "frame": {
            "w": s.image.as_ref().map(|i| i.w), "h": s.image.as_ref().map(|i| i.h), "bin": s.binning,
            "subframe": s.subframe.map(|r| json!([r.x, r.y, r.w, r.h])),
        },
        "exposure_ms": s.exposure_setting_ms,
        "exposure_options": s.exposure_durations,
        "auto_exposure": {"min": s.auto_exposure.0, "max": s.auto_exposure.1, "snr": s.auto_exposure.2},
        "pixel_scale": if s.pixel_scale == 1.0 { Value::Null } else { json!(s.pixel_scale) },
        "search_region": s.search_region,
        "use_subframes": s.use_subframes,
        "multistar": s.multistar,
        "guide_output_enabled": primary.map(|m| m.guiding_enabled),
        "dec_guide_mode": s.dec_guide_mode.map(|m| m.label()),
        "dither": {
            "mode": match s.dither_mode { crate::engine::control::DitherMode::Random => "random", _ => "spiral" },
            "ra_only": s.dither_ra_only, "scale": s.dither_scale,
        },
        "lock_shift": {
            "enabled": l.enabled, "rate": [l.rate.x, l.rate.y],
            "units": if l.units == ShiftUnits::Arcsec { "arcsec/hr" } else { "pixels/hr" },
            "axes": if l.mount_coords { "RA/Dec" } else { "X/Y" },
            "limit_reached": track.limit_reached,
        },
        "equipment": {
            "camera": camera, "mount": device(&s.mount), "aux_mount": device(&s.aux_mount),
            "ao": device(&s.ao), "rotator": device(&s.rotator),
        },
        "indi": indi,
        "dark": {"loaded": s.dark_loaded, "exp_ms": s.dark_exp_ms},
        "defect_map": s.defect_map_loaded,
        "ga_running": s.ga.as_ref().is_some_and(|g| g.measuring || g.measuring_backlash),
        "pa_running": s.polar_drift.as_ref().is_some_and(|p| p.drifting),
    })
}

/// `buildGammaLookupTable`: black to `blevel`, `((v − b) / (w − b))^power`
/// up to `wlevel`, white from there, in float as PHD2 computes it.
fn gamma_lut(blevel: i32, wlevel: i32, power: f64) -> Vec<u8> {
    let mut lut = vec![0u8; 0x10000];
    let (b, w) = (blevel.clamp(0, 0xffff), wlevel.clamp(0, 0xffff));
    let range = (w - b) as f32;
    for i in b + 1..w {
        let d = (i - b) as f32 / range;
        lut[i as usize] = (d.powf(power as f32) as f64 * 255.0) as u8;
    }
    for v in &mut lut[w as usize..] {
        *v = 255;
    }
    lut
}

/// The frame as PHD2 shows it (`CopyToImage` between `FiltMin` and
/// `FiltMax`, the `/Gamma` setting), at most `max_width` wide, as a JPEG.
fn encode_jpeg(img: &UsImage, gamma: f64, max_width: Option<u32>, quality: u8) -> Option<Vec<u8>> {
    use image::imageops::{FilterType, resize};
    let lut = gamma_lut(img.filt_min as i32, img.filt_max as i32, gamma);
    let pixels: Vec<u8> = img.data.iter().map(|&v| lut[v as usize]).collect();
    let mut gray = image::GrayImage::from_raw(img.w as u32, img.h as u32, pixels)?;
    if let Some(mw) = max_width.filter(|&mw| mw < gray.width()) {
        let h = ((gray.height() as u64 * mw as u64) / gray.width() as u64).max(1) as u32;
        // Bilinear, as PHD2's display rescales.
        gray = resize(&gray, mw, h, FilterType::Triangle);
    }
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode(gray.as_raw(), gray.width(), gray.height(), image::ExtendedColorType::L8)
        .ok()?;
    Some(out)
}

/// `guider_frame`: the JPEG, its metadata, the frame's statistics and the
/// overlay in frame pixels (the relay scales it to the JPEG it sends).
fn frame_payload(img: &UsImage, s: &Snapshot, part: &ProfilePart, f: FrameFilter, seq: u64) -> Option<Value> {
    let setting = |key: &str, default: f64| part.settings.get(key).and_then(Value::as_f64).unwrap_or(default);
    let jpeg = encode_jpeg(img, setting("/Gamma", 100.0) / 100.0, f.max_width, f.quality.unwrap_or(85))?;
    let star = s.star.filter(|_| s.star_position.is_some()).map(|st| {
        json!({
            "x": st.x, "y": st.y, "mass": st.mass, "snr": st.snr, "hfd": st.hfd, "fwhm": null,
            "peak": st.peak_val, "err": st.result.error_code(), "saturated": st.result == FindResult::Saturated,
        })
    });
    let stars: Vec<Value> = s
        .guide_stars
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let st = &g.star;
            json!({"x": st.x, "y": st.y, "snr": st.snr, "hfd": st.hfd, "mass": st.mass, "primary": i == 0, "lost": g.was_lost})
        })
        .collect();
    let shift = s.lock_shift_rate.filter(|_| s.lock_shift.enabled).map(|r| json!([r.x * 3600.0, r.y * 3600.0]));
    let scale_image = part.settings.get("/guider/ScaleImage").and_then(Value::as_bool).unwrap_or(true);
    Some(json!({
        "seq": seq,
        "metadata": {
            "resolution": format!("{}x{}", img.w, img.h),
            "frame": img.frame_num,
            "exposure_ms": img.img_exp_dur,
            "binning": s.binning,
            "subframe": img.subframe.filter(|r| !r.is_empty()).map(|r| json!([r.x, r.y, r.w, r.h])),
            "scale_image": scale_image,
        },
        "data": methods::base64(&jpeg),
        "stats": {"min": img.min_adu, "max": img.max_adu, "median": img.median_adu, "filt_min": img.filt_min, "filt_max": img.filt_max},
        "guider": {
            "lock": s.lock_position.map(|p| json!([p.x, p.y])),
            "star": star,
            "stars": stars,
            "search_region": s.search_region,
            "lock_shift_px_h": shift,
            "star_image": if f.profile { methods::star_image(s, 31).ok() } else { None },
        },
    }))
}

/// Encodes and sends one frame off the hub, unless one is still encoding.
fn send_frame(sh: &Arc<Shared>, img: Arc<UsImage>, s: Arc<Snapshot>, part: &ProfilePart, seq: u64) {
    let f = *lock(&sh.filter);
    if !f.frames || sh.encoding.swap(true, Ordering::SeqCst) {
        return;
    }
    let settings = ProfilePart { settings: part.settings.clone(), ..ProfilePart::default() };
    let sh = sh.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(p) = frame_payload(&img, &s, &settings, f, seq) {
            sh.send("guider_frame", &p.to_string());
        }
        sh.encoding.store(false, Ordering::SeqCst);
    });
}

/// One relay connection: the snapshots, then the messages and the calls.
async fn client(stream: TcpStream, sh: Arc<Shared>, d: Daemon) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let (mut sink, mut source) = ws.split();
    let (first, mut rx) = {
        let st = lock(&sh.stickies);
        let mut first: Vec<String> = st.get("guider_state").into_iter().cloned().collect();
        first.extend(st.iter().filter(|(k, _)| **k != "guider_state").map(|(_, v)| v.clone()));
        first.push(message("guider_history", &sh.history_payload()));
        (first, sh.out.subscribe())
    };
    for m in first {
        if sink.send(Message::Text(m)).await.is_err() {
            return;
        }
    }
    let (rtx, mut rrx) = mpsc::channel::<String>(64);
    loop {
        let out = tokio::select! {
            m = source.next() => match m {
                Some(Ok(Message::Text(t))) => {
                    on_text(&t, &d, &sh, &rtx);
                    continue;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return,
                Some(Ok(_)) => continue,
            },
            m = rx.recv() => match m {
                Ok(m) => (*m).clone(),
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!("relay socket: {n} messages dropped for a slow relay");
                    continue;
                }
                Err(RecvError::Closed) => return,
            },
            Some(m) = rrx.recv() => m,
        };
        if sink.send(Message::Text(out)).await.is_err() {
            return;
        }
    }
}

/// A relay message: a `guider_rpc`, answered in its own task.
fn on_text(text: &str, d: &Daemon, sh: &Arc<Shared>, replies: &mpsc::Sender<String>) {
    let Ok(Value::Object(mut msg)) = serde_json::from_str::<Value>(text) else { return };
    if msg.get("type").and_then(Value::as_str) != Some("guider_rpc") {
        return;
    }
    let Some(Value::Object(mut p)) = msg.remove("payload") else { return };
    // Every call is answered, its errors too, under the id it came with.
    let id = p.get("id").cloned().unwrap_or(Value::Null);
    // A null `params` is none here, where PHD2 refuses it: the UI's helper
    // sends one for a method without parameters.
    if p.get("params").is_some_and(Value::is_null) {
        p.remove("params");
    }
    let call = rpc::call(Value::Object(p), true);
    let name = call.as_ref().ok().map(|c| c.method.name);
    let (d, sh, replies) = (d.clone(), sh.clone(), replies.clone());
    tokio::spawn(async move {
        let outcome = match call {
            Ok(call) => match call.method.name {
                "set_frame_filter" => set_frame_filter(&sh, &call),
                "get_history" => {
                    let h = sh.history_payload();
                    // The relay's copy follows too.
                    sh.send("guider_history", &h);
                    Ok(serde_json::from_str(&h).unwrap_or(Value::Null))
                }
                _ => methods::run(&d, &call).await,
            },
            Err(e) => Err(e),
        };
        let shutdown = outcome.is_ok() && name == Some("shutdown");
        let payload = match outcome {
            Ok(result) => json!({"id": id, "result": result}),
            Err(e) => json!({"id": id, "error": {"code": e.code, "message": e.message}}),
        };
        let _ = replies.send(message("guider_rpc_result", &payload.to_string())).await;
        // `TerminateApp` after the reply, as the event server does.
        if shutdown {
            d.shutdown().await;
        }
    });
}

/// `set_frame_filter {frames, profile, max_width, quality}`, as the relay
/// sends it; a missing flag is read from `kinds` (`["frame", "profile"]`),
/// which the relay sends too.
fn set_frame_filter(sh: &Shared, call: &Call) -> Result<Value, RpcError> {
    let p = call.params();
    let kinds: Vec<&str> =
        p.param("kinds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let flag = |name: &str, kind: &str| p.param(name).and_then(rpc::bool_param).unwrap_or(kinds.contains(&kind));
    let f = FrameFilter {
        frames: flag("frames", "frame"),
        profile: flag("profile", "profile"),
        max_width: p.param("max_width").and_then(Value::as_u64).map(|w| w.clamp(16, 16384) as u32),
        quality: p.param("quality").and_then(Value::as_u64).map(|q| q.clamp(1, 100) as u8),
    };
    *lock(&sh.filter) = f;
    Ok(json!(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::tests::{dir, start};
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

    /// The relay's messages until one matches, within `secs`, and those
    /// before it.
    async fn until(ws: &mut Ws, secs: u64, f: impl Fn(&Value) -> bool) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(secs), async {
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Text(t) = m {
                    let v: Value = serde_json::from_str(&t).expect("JSON");
                    if f(&v) {
                        return Some(v);
                    }
                    seen.push(v);
                }
            }
            None
        })
        .await;
        let hit = r.ok().flatten();
        let types: Vec<&Value> = seen.iter().map(|v| &v["type"]).collect();
        (hit.unwrap_or_else(|| panic!("no match in {secs} s; saw {types:?}")), seen)
    }

    async fn rpc(ws: &mut Ws, id: i64, method: &str, params: Value) -> Value {
        let msg = json!({"type": "guider_rpc", "payload": {"method": method, "params": params, "id": id}});
        ws.send(Message::Text(msg.to_string())).await.unwrap();
        let (r, _) = until(ws, 30, |v| v["type"] == "guider_rpc_result" && v["payload"]["id"] == id).await;
        r["payload"].clone()
    }

    #[test]
    fn the_frame_filter_reads_the_relays_shape() {
        let sh = Shared::new();
        let call = |params| Call { method: rpc::method("set_frame_filter", true).unwrap(), params: Some(params), id: None };
        // What junos-server's guider.rs sends: the flags win over `kinds`.
        let relay = json!({"frames": true, "profile": false, "kinds": ["frame"], "max_width": 960, "quality": 70});
        assert_eq!(set_frame_filter(&sh, &call(relay)), Ok(json!(0)));
        assert_eq!(*lock(&sh.filter), FrameFilter { frames: true, profile: false, max_width: Some(960), quality: Some(70) });
        set_frame_filter(&sh, &call(json!({"kinds": ["frame", "profile"], "max_width": null}))).unwrap();
        assert_eq!(*lock(&sh.filter), FrameFilter { frames: true, profile: true, max_width: None, quality: None });
    }

    #[test]
    fn the_stretch_is_phd2s() {
        let lut = gamma_lut(100, 200, 1.0);
        assert_eq!((lut[0], lut[100], lut[101], lut[150], lut[199], lut[200], lut[65535]), (0, 0, 2, 127, 252, 255, 255));
        assert_eq!(gamma_lut(100, 200, 2.0)[150], 63);
        // An empty range: everything from the white level is white.
        assert_eq!((gamma_lut(50, 50, 1.0)[49], gamma_lut(50, 50, 1.0)[50]), (0, 255));
    }

    fn unbase64(s: &str) -> Vec<u8> {
        const E: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bits: Vec<u8> = s.bytes().filter(|&c| c != b'=').map(|c| E.iter().position(|&e| e == c).unwrap() as u8).collect();
        bits.chunks(4)
            .flat_map(|c| {
                let t = c.iter().enumerate().fold(0u32, |t, (i, &v)| t | (v as u32) << (18 - 6 * i));
                (0..c.len() - 1).map(move |k| (t >> (16 - 8 * k)) as u8)
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn frames_go_out_once_the_relay_asks() {
        use crate::daemon::tests::{find_star, wait_for};
        let d = start(&dir("ws-frames"), |_| {});
        let mut ev = d.subscribe();
        let addr = spawn(d.clone(), "127.0.0.1:0").await.unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
        assert_eq!(rpc(&mut ws, 1, "set_connected", json!([true])).await["result"], 0);
        assert_eq!(rpc(&mut ws, 2, "loop", json!([])).await["result"], 0);
        wait_for(&mut ev, 5, |e| matches!(e, EngineEvent::LoopingExposures { .. })).await;
        let lock = find_star(&d, &mut ev).await;
        // The relay's own shape (junos-server's `FrameFilter::rpc`).
        let filter = json!({"frames": true, "profile": true, "kinds": ["frame", "profile"], "max_width": 320, "quality": 70});
        assert_eq!(rpc(&mut ws, 3, "set_frame_filter", filter).await["result"], 0);
        let (f, _) = until(&mut ws, 10, |v| v["type"] == "guider_frame" && !v["payload"]["guider"]["star_image"].is_null()).await;
        let p = &f["payload"];
        let jpeg = unbase64(p["data"].as_str().unwrap());
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        let (fw, fh) = d.snapshot().frame_size;
        assert_eq!(p["metadata"]["resolution"], format!("{fw}x{fh}"));
        assert_eq!((img.width(), img.height()), (320, (fh as u32 * 320) / fw as u32));
        assert!(p["stats"]["filt_max"].as_u64() > p["stats"]["filt_min"].as_u64(), "{}", p["stats"]);
        let g = &p["guider"];
        // 31 px across, less where the frame edge cuts it.
        let (w, h) = (g["star_image"]["width"].as_u64().unwrap(), g["star_image"]["height"].as_u64().unwrap());
        assert!((16..=31).contains(&w) && (16..=31).contains(&h), "{}", g["star_image"]);
        assert_eq!(unbase64(g["star_image"]["pixels"].as_str().unwrap()).len() as u64, w * h * 2);
        assert!(g["star"]["snr"].as_f64().unwrap() > 0.0);
        assert_eq!(g["stars"][0]["primary"], true);
        let l = g["lock"].as_array().unwrap();
        assert!((l[0].as_f64().unwrap() - lock[0].as_f64().unwrap()).abs() < 0.01, "{l:?} vs {lock}");
        // Nothing once nobody looks.
        let none = json!({"frames": false, "profile": false, "kinds": [], "max_width": null, "quality": null});
        assert_eq!(rpc(&mut ws, 4, "set_frame_filter", none).await["result"], 0);
        // A frame already encoding may still come; then none for a second.
        let read_for = |ms| tokio::time::Instant::now() + Duration::from_millis(ms);
        let mut frames = 0;
        for (end, count) in [(read_for(500), false), (read_for(1500), true)] {
            while let Ok(Some(Ok(Message::Text(t)))) = tokio::time::timeout_at(end, ws.next()).await {
                let v: Value = serde_json::from_str(&t).unwrap();
                frames += (count && v["type"] == "guider_frame") as u32;
            }
        }
        assert_eq!(frames, 0, "frames after the filter");
        assert_eq!(rpc(&mut ws, 5, "stop_capture", json!([])).await["result"], 0);
        d.shutdown().await;
        d.close();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_relay_gets_snapshots_events_and_answers() {
        let d = start(&dir("ws"), |_| {});
        // No event server here: its port as the server would publish it.
        d.set_event_server(Some(4400), None);
        let addr = spawn(d.clone(), "127.0.0.1:0").await.unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();

        let (hist, first) = until(&mut ws, 10, |v| v["type"] == "guider_history").await;
        let types: Vec<&str> = first.iter().map(|v| v["type"].as_str().unwrap()).collect();
        assert_eq!(types[0], "guider_state");
        for t in [
            "guider_settings",
            "guider_profiles",
            "guider_calibration",
            "guider_devices",
            "guider_darks",
            "guider_ga",
            "guider_pa",
            "guider_tools",
        ] {
            assert!(types.contains(&t), "{t} missing from {types:?}");
        }
        let state = &first[0]["payload"];
        assert_eq!((state["attached"].as_bool(), state["app_state"].as_str()), (Some(true), Some("Stopped")));
        assert_eq!((state["port"].as_i64(), state["version"].as_str()), (Some(4400), Some("2.6.14")));
        assert!(state["Timestamp"].as_f64().is_some());
        let settings = &first[types.iter().position(|t| *t == "guider_settings").unwrap()]["payload"];
        assert_eq!(settings["/ExposureDurationMs"], 100);
        assert_eq!(hist["payload"]["events"], json!([]));
        assert_eq!(hist["payload"]["states"][0]["app_state"], "Stopped");
        // The server's bind error reaches the relay at once.
        d.set_event_server(None, Some("Could not listen at port 4400: in use".into()));
        let (state, _) = until(&mut ws, 10, |v| v["type"] == "guider_state" && !v["payload"]["port_error"].is_null()).await;
        assert_eq!((&state["payload"]["port"], &state["payload"]["port_error"]), (&Value::Null, &json!("Could not listen at port 4400: in use")));

        // PHD2's methods, the UI's and this socket's, through one envelope.
        assert_eq!(rpc(&mut ws, 7, "get_app_state", Value::Null).await, json!({"id": 7, "result": "Stopped"}));
        assert_eq!(rpc(&mut ws, 8, "no_such_method", json!([])).await["error"]["code"], rpc::METHOD_NOT_FOUND);
        let r = rpc(&mut ws, 9, "export_profile", json!({})).await;
        assert_eq!(r["result"]["name"], "My Equipment");
        // The new state and the reply race: wait for both.
        let msg = json!({"type": "guider_rpc", "payload": {"method": "set_connected", "params": [true], "id": 10}});
        ws.send(Message::Text(msg.to_string())).await.unwrap();
        let (state, reply) = (std::cell::Cell::new(false), std::cell::Cell::new(false));
        until(&mut ws, 30, |v| {
            let cam = &v["payload"]["equipment"]["camera"];
            state.set(state.get() || (v["type"] == "guider_state" && cam["name"] == "Simulator" && cam["connected"] == true));
            reply.set(reply.get() || (v["type"] == "guider_rpc_result" && v["payload"] == json!({"id": 10, "result": 0})));
            state.get() && reply.get()
        })
        .await;
        assert_eq!(rpc(&mut ws, 11, "loop", json!({})).await["result"], 0);
        let (ev, _) =
            until(&mut ws, 10, |v| v["type"] == "guider_event" && v["payload"]["Event"] == "LoopingExposures").await;
        assert!(ev["payload"]["Frame"].as_i64().is_some());
        assert_eq!(rpc(&mut ws, 12, "stop_capture", json!([])).await["result"], 0);
        // The stop's event and the reply race too: ask until it is kept.
        let stopped = |r: &Value| r["result"]["events"].as_array().unwrap().iter().any(|e| e["Event"] == "LoopingExposuresStopped");
        let mut r = Value::Null;
        for id in 13..40 {
            r = rpc(&mut ws, id, "get_history", json!({})).await;
            if stopped(&r) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(stopped(&r), "{r}");
        assert!(r["result"]["states"].as_array().unwrap().iter().any(|s| s["app_state"] == "Looping"), "{r}");
        d.shutdown().await;
        d.close();
    }
}
