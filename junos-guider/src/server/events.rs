//! The events of PHD2's event server as a client reads them: one JSON
//! object per event, its members in PHD2's order — `Event`, `Timestamp`
//! (UTC seconds, `%.3f`), `Host`, `Inst`, then the event's own fields with
//! PHD2's number formats — and the catch-up a client gets on connect.
//!
//! Kept as PHD2 sends them:
//! - The AO's `CalibrationComplete` never goes out live (`stepguider.cpp:762`
//!   only logs it); a client sees it only in the catch-up, with `Limit`.
//! - `AppState` goes out only in the catch-up (`NotifyAppState` has no
//!   caller): clients follow the state from the other events.
//! - `GuideStep`'s durations and directions only when the pulse is > 0, the
//!   AO's `Pos`, `ErrorCode` only when non-zero, `RALimited` / `DecLimited`
//!   only when true.
//! - `ConfigurationChange` is PHD2's leading edge: the first change of a
//!   pass goes out, the others of that pass are swallowed by its 0 ms
//!   debouncer. The engine already collapses a command's or a frame's
//!   changes into one event (`session.rs`), so it is sent as it comes.
//!
//! The frame, the guide-log lines and the status line are the relay's, not
//! the event server's: [`event`] gives `None` for them.
//!
//! Ported from PHD2 src/event_server.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::rpc::{json_text, num_f, num_g, num_point};
use crate::engine::guider::{AppState, GuiderState};
use crate::engine::multistar::FrameErrorInfo;
use crate::engine::traits::Point;
use crate::engine::{CalibrationView, EngineEvent, Snapshot};

/// PHD2's version, as the `Version` event gives it.
pub const PHD_VERSION: &str = "2.6.14";
/// `MSG_PROTOCOL_VERSION`.
const MSG_VERSION: i32 = 1;

/// Who sends the events: the `Host` and `Inst` of every one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub host: String,
    pub inst: u32,
}

impl Origin {
    /// This machine, instance `inst`.
    pub fn new(inst: u32) -> Origin {
        Origin { host: host_name(), inst }
    }
}

/// `wxGetHostName`: the host name up to its first dot, empty when unknown.
pub fn host_name() -> String {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    name.trim().split('.').next().unwrap_or_default().to_string()
}

/// `wxGetUTCTimeMillis() / 1000`: now, UTC seconds to the ms.
pub fn now() -> f64 {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    ms as f64 / 1000.0
}

/// One event being written (PHD2's `Ev`): the members in the order added.
struct Ev(String);

impl Ev {
    fn new(name: &str, o: &Origin, now: f64) -> Ev {
        let mut ev = Ev(String::from("{"));
        ev.nv("Event", name).nv("Timestamp", num_f(now, 3)).nv("Host", o.host.as_str()).nv("Inst", o.inst);
        ev
    }

    fn nv(&mut self, name: &str, v: impl Into<Value>) -> &mut Ev {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push('"');
        self.0.push_str(name);
        self.0.push_str("\":");
        self.0.push_str(&json_text(&v.into()));
        self
    }

    /// `JObj << PHD_Point`: `X` and `Y`, `%.3f`.
    fn xy(&mut self, p: Point) -> &mut Ev {
        self.nv("X", num_f(p.x, 3)).nv("Y", num_f(p.y, 3))
    }

    fn done(mut self) -> String {
        self.0.push('}');
        self.0
    }
}

/// `NV(wxPoint)`: `[x,y]` as integers.
fn int_pair((x, y): (i32, i32)) -> Value {
    Value::from(vec![x, y])
}

/// `NotifyGuidingParam`'s value: a double as `%g`, the rest as typed.
fn param_value(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => num_g(n.as_f64().unwrap_or_default()),
        v => v.clone(),
    }
}

/// `Version`, the first thing a client reads.
pub fn version(o: &Origin, now: f64) -> String {
    let mut ev = Ev::new("Version", o, now);
    ev.nv("PHDVersion", PHD_VERSION).nv("PHDSubver", "").nv("OverlapSupport", true).nv("MsgVersion", MSG_VERSION);
    ev.done()
}

/// `ev_calibration_complete`: the mount's name, and the AO's travel.
fn calibration_complete(mount: &str, limit: Option<(i32, i32)>, o: &Origin, now: f64) -> String {
    let mut ev = Ev::new("CalibrationComplete", o, now);
    ev.nv("Mount", mount);
    if let Some(l) = limit {
        ev.nv("Limit", int_pair(l));
    }
    ev.done()
}

/// The text of `e`, or `None` when the event server does not send it.
pub fn event(e: &EngineEvent, o: &Origin, now: f64) -> Option<String> {
    let simple = |name: &str| Some(Ev::new(name, o, now).done());
    let ev = |name: &str| Ev::new(name, o, now);
    let text = match e {
        EngineEvent::Version => version(o, now),
        EngineEvent::LockPositionSet(p) => {
            let mut ev = ev("LockPositionSet");
            ev.xy(*p);
            ev.done()
        }
        EngineEvent::LockPositionLost => return simple("LockPositionLost"),
        EngineEvent::LockPositionShiftLimitReached => return simple("LockPositionShiftLimitReached"),
        EngineEvent::StarSelected(p) => {
            let mut ev = ev("StarSelected");
            ev.xy(*p);
            ev.done()
        }
        EngineEvent::StarLost(info) => {
            let mut ev = ev("StarLost");
            ev.nv("Frame", info.frame_number)
                .nv("Time", num_f(info.time, 3))
                .nv("StarMass", num_f(info.star.star_mass, 0))
                .nv("SNR", num_f(info.star.star_snr, 2))
                .nv("HFD", num_f(info.star.star_hfd, 2))
                .nv("AvgDist", num_f(info.avg_dist, 2));
            error_status(&mut ev, info.star.star_error.error_code(), info.star.status);
            ev.done()
        }
        EngineEvent::LoopingExposures { frame, star, dropped } => {
            let mut ev = ev("LoopingExposures");
            ev.nv("Frame", *frame);
            // `NotifyLooping`: the star's measures, or a drop's when its
            // star was found all the same.
            let (mut mass, mut snr, mut hfd, mut err, mut status) = (0.0, 0.0, 0.0, 0, "");
            if let Some(s) = star {
                (mass, snr, hfd, err) = (s.mass, s.snr, s.hfd, s.result.error_code());
            } else if let Some(FrameErrorInfo { star_error, star_mass, star_snr, star_hfd, status: st }) = dropped {
                if star_error.was_found() {
                    (mass, snr, hfd) = (*star_mass, *star_snr, *star_hfd);
                }
                (err, status) = (star_error.error_code(), *st);
            }
            if mass != 0.0 {
                ev.nv("StarMass", num_f(mass, 0)).nv("SNR", num_f(snr, 2)).nv("HFD", num_f(hfd, 2));
            }
            error_status(&mut ev, err, status);
            ev.done()
        }
        EngineEvent::LoopingExposuresStopped => return simple("LoopingExposuresStopped"),
        EngineEvent::StartCalibration { mount } => {
            let mut ev = ev("StartCalibration");
            ev.nv("Mount", mount.as_str());
            ev.done()
        }
        EngineEvent::Calibrating { mount, step } => {
            let mut ev = ev("Calibrating");
            ev.nv("Mount", mount.as_str())
                .nv("dir", step.direction)
                .nv("dist", num_g(step.dist))
                .nv("dx", num_g(step.dx))
                .nv("dy", num_g(step.dy))
                .nv("pos", num_point(step.pos.x, step.pos.y))
                .nv("step", step.step);
            if !step.msg.is_empty() {
                ev.nv("State", step.msg.as_str());
            }
            ev.done()
        }
        // Quirk kept: the AO's is never sent live.
        EngineEvent::CalibrationComplete { ao: true, .. } => return None,
        EngineEvent::CalibrationComplete { mount, ao: false } => calibration_complete(mount, None, o, now),
        EngineEvent::CalibrationFailed { mount, reason } => {
            let mut ev = ev("CalibrationFailed");
            ev.nv("Mount", mount.as_str()).nv("Reason", reason.as_str());
            ev.done()
        }
        EngineEvent::CalibrationDataFlipped { mount } => {
            let mut ev = ev("CalibrationDataFlipped");
            ev.nv("Mount", mount.as_str());
            ev.done()
        }
        EngineEvent::StartGuiding => return simple("StartGuiding"),
        EngineEvent::GuidingStopped => return simple("GuidingStopped"),
        EngineEvent::Paused => return simple("Paused"),
        EngineEvent::Resumed => return simple("Resumed"),
        EngineEvent::GuideStep { mount, step: s } => {
            let dir = |d: crate::engine::traits::Dir| if s.is_ao { d.ao_label() } else { d.mount_label() };
            let mut ev = ev("GuideStep");
            ev.nv("Frame", s.frame_number)
                .nv("Time", num_f(s.time, 3))
                .nv("Mount", mount.as_str())
                .nv("dx", num_f(s.camera_offset.x, 3))
                .nv("dy", num_f(s.camera_offset.y, 3))
                .nv("RADistanceRaw", num_f(s.mount_offset.x, 3))
                .nv("DECDistanceRaw", num_f(s.mount_offset.y, 3))
                .nv("RADistanceGuide", num_f(s.guide_distance_ra, 3))
                .nv("DECDistanceGuide", num_f(s.guide_distance_dec, 3));
            if s.duration_ra > 0 {
                ev.nv("RADuration", s.duration_ra).nv("RADirection", dir(s.direction_ra));
            }
            if s.duration_dec > 0 {
                ev.nv("DECDuration", s.duration_dec).nv("DECDirection", dir(s.direction_dec));
            }
            if s.is_ao {
                ev.nv("Pos", int_pair(s.ao_pos));
            }
            ev.nv("StarMass", num_f(s.star_mass, 0))
                .nv("SNR", num_f(s.star_snr, 2))
                .nv("HFD", num_f(s.star_hfd, 2))
                .nv("AvgDist", num_f(s.avg_dist, 2));
            if s.star_error != 0 {
                ev.nv("ErrorCode", s.star_error);
            }
            if s.ra_limited {
                ev.nv("RALimited", true);
            }
            if s.dec_limited {
                ev.nv("DecLimited", true);
            }
            ev.done()
        }
        EngineEvent::GuidingDithered { dx, dy } => {
            let mut ev = ev("GuidingDithered");
            ev.nv("dx", num_f(*dx, 3)).nv("dy", num_f(*dy, 3));
            ev.done()
        }
        EngineEvent::SettleBegin => return simple("SettleBegin"),
        EngineEvent::Settling { distance, time, settle_time, star_locked } => {
            let mut ev = ev("Settling");
            ev.nv("Distance", num_f(*distance, 2))
                .nv("Time", num_f(*time, 1))
                .nv("SettleTime", num_f(*settle_time, 1))
                .nv("StarLocked", *star_locked);
            ev.done()
        }
        EngineEvent::SettleDone { error, total_frames, dropped_frames } => {
            let error = error.as_deref().filter(|e| !e.is_empty());
            let mut ev = ev("SettleDone");
            ev.nv("Status", error.is_some() as i32);
            if let Some(e) = error {
                ev.nv("Error", e);
            }
            ev.nv("TotalFrames", *total_frames).nv("DroppedFrames", *dropped_frames);
            ev.done()
        }
        EngineEvent::SingleFrameComplete { success, error, path } => {
            let mut ev = ev("SingleFrameComplete");
            ev.nv("Success", *success);
            if !success {
                ev.nv("Error", error.as_deref().unwrap_or_default());
            }
            if let Some(p) = path {
                ev.nv("Path", p.as_str());
            }
            ev.done()
        }
        EngineEvent::Alert { msg, kind, .. } => {
            let mut ev = ev("Alert");
            ev.nv("Msg", msg.as_str()).nv("Type", kind.type_str());
            ev.done()
        }
        EngineEvent::GuideParamChange { name, value } => {
            let mut ev = ev("GuideParamChange");
            ev.nv("Name", name.as_str()).nv("Value", param_value(value));
            ev.done()
        }
        EngineEvent::ConfigurationChange => return simple("ConfigurationChange"),
        EngineEvent::AppState(st) => app_state(*st, o, now),
        EngineEvent::FrameReady(_)
        | EngineEvent::GuideLogLine(_)
        | EngineEvent::Status(_)
        | EngineEvent::Notice(_) => return None,
    };
    Some(text)
}

/// The optional `ErrorCode` and `Status` of `StarLost` and `LoopingExposures`.
fn error_status(ev: &mut Ev, err: i32, status: &str) {
    if err != 0 {
        ev.nv("ErrorCode", err);
    }
    if !status.is_empty() {
        ev.nv("Status", status);
    }
}

fn app_state(st: AppState, o: &Origin, now: f64) -> String {
    let mut ev = Ev::new("AppState", o, now);
    ev.nv("State", st.as_str());
    ev.done()
}

/// `send_catchup_events`: what a client gets on connect, from one snapshot.
/// The calibrations come pMount first — the AO when there is one, with its
/// `Limit` — then pSecondaryMount.
pub fn catch_up(snap: &Snapshot, o: &Origin, now: f64) -> Vec<String> {
    let st = snap.app_state.unwrap_or(AppState::Stopped);
    let mut out = vec![version(o, now)];
    if let Some(p) = snap.lock_position {
        out.push(event(&EngineEvent::LockPositionSet(p), o, now).unwrap_or_default());
    }
    if let Some(p) = snap.star_position {
        out.push(event(&EngineEvent::StarSelected(p), o, now).unwrap_or_default());
    }
    let (primary, secondary) = mounts(snap);
    for m in [primary, secondary].into_iter().flatten() {
        if m.calibrated {
            let limit = std::ptr::eq(m, &snap.ao_calibration).then_some(m.ao_max_pos);
            out.push(calibration_complete(&m.name, limit, o, now));
        }
    }
    match st {
        AppState::Guiding => out.push(Ev::new("StartGuiding", o, now).done()),
        AppState::Calibrating => {
            let m = match snap.guider_state {
                Some(GuiderState::CalibratingSecondary) => secondary,
                _ => primary,
            };
            let mut ev = Ev::new("StartCalibration", o, now);
            ev.nv("Mount", m.map_or("", |m| m.name.as_str()));
            out.push(ev.done());
        }
        AppState::Paused => out.push(Ev::new("Paused", o, now).done()),
        _ => {}
    }
    out.push(app_state(st, o, now));
    out
}

/// pMount and pSecondaryMount: the AO and the scope behind it, or the
/// scope alone.
pub fn mounts(snap: &Snapshot) -> (Option<&CalibrationView>, Option<&CalibrationView>) {
    let (ao, scope) = (&snap.ao_calibration, &snap.mount_calibration);
    match (ao.present, scope.present) {
        (true, true) => (Some(ao), Some(scope)),
        (true, false) => (Some(ao), None),
        (false, true) => (Some(scope), None),
        (false, false) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::guider::FrameDroppedInfo;
    use crate::engine::mount::{AlertKind, CalibrationStepInfo, GuideStepInfo};
    use crate::engine::star::{FindResult, Star};
    use crate::engine::traits::{Dir, MoveOpts};

    fn o() -> Origin {
        Origin { host: "obs".into(), inst: 1 }
    }

    fn text(e: EngineEvent) -> String {
        event(&e, &o(), 1731340000.25).unwrap()
    }

    const HEAD: &str = r#""Timestamp":1731340000.25,"Host":"obs","Inst":1"#;

    #[test]
    fn version_and_header() {
        assert_eq!(
            version(&o(), 12.0),
            r#"{"Event":"Version","Timestamp":12.0,"Host":"obs","Inst":1,"PHDVersion":"2.6.14","PHDSubver":"","OverlapSupport":true,"MsgVersion":1}"#
        );
        assert_eq!(text(EngineEvent::LockPositionLost), format!(r#"{{"Event":"LockPositionLost",{HEAD}}}"#));
        assert!(host_name().find('.').is_none());
    }

    fn step(is_ao: bool) -> GuideStepInfo {
        GuideStepInfo {
            move_options: MoveOpts::default(),
            frame_number: 12,
            time: 30.2504,
            is_ao,
            camera_offset: Point::new(0.12345, -1.5),
            mount_offset: Point::new(0.2, 0.0),
            guide_distance_ra: 0.1,
            guide_distance_dec: 0.0,
            duration_ra: 150,
            direction_ra: Dir::West,
            duration_dec: 0,
            direction_dec: Dir::North,
            ra_limited: true,
            dec_limited: false,
            ao_pos: (3, -2),
            star_mass: 12345.6,
            star_snr: 25.456,
            star_hfd: 2.111,
            avg_dist: 0.3,
            star_error: 0,
        }
    }

    #[test]
    fn guide_step_fields_in_order() {
        let t = text(EngineEvent::GuideStep { mount: "On Camera".into(), step: step(false) });
        assert_eq!(
            t,
            format!(
                r#"{{"Event":"GuideStep",{HEAD},"Frame":12,"Time":30.25,"Mount":"On Camera","dx":0.123,"dy":-1.5,"RADistanceRaw":0.2,"DECDistanceRaw":0.0,"RADistanceGuide":0.1,"DECDistanceGuide":0.0,"RADuration":150,"RADirection":"West","StarMass":12346,"SNR":25.46,"HFD":2.11,"AvgDist":0.3,"RALimited":true}}"#
            )
        );
        // The AO's: its directions, its position.
        let mut s = step(true);
        (s.duration_dec, s.direction_dec, s.star_error) = (2, Dir::South, 1);
        let t = text(EngineEvent::GuideStep { mount: "AO".into(), step: s });
        assert!(t.contains(r#""RADirection":"Left","DECDuration":2,"DECDirection":"Down","Pos":[3,-2],"#), "{t}");
        assert!(t.contains(r#""ErrorCode":1,"RALimited":true}"#), "{t}");
    }

    #[test]
    fn calibration_events() {
        let st = CalibrationStepInfo {
            direction: "West",
            step: 3,
            dx: 1.0,
            dy: -0.25,
            pos: Point::new(100.456, 50.0),
            dist: 1.0307764,
            msg: String::new(),
        };
        let t = text(EngineEvent::Calibrating { mount: "On Camera".into(), step: st.clone() });
        assert!(
            t.ends_with(r#""Mount":"On Camera","dir":"West","dist":1.03078,"dx":1,"dy":-0.25,"pos":[100.46,50.0],"step":3}"#),
            "{t}"
        );
        let st = CalibrationStepInfo { msg: "15 steps".into(), ..st };
        assert!(text(EngineEvent::Calibrating { mount: "m".into(), step: st }).ends_with(r#""step":3,"State":"15 steps"}"#));
        assert_eq!(event(&EngineEvent::CalibrationComplete { mount: "AO".into(), ao: true }, &o(), 0.0), None);
        assert!(
            text(EngineEvent::CalibrationComplete { mount: "On Camera".into(), ao: false })
                .ends_with(r#""Inst":1,"Mount":"On Camera"}"#)
        );
        let t = text(EngineEvent::CalibrationFailed { mount: "m".into(), reason: "Calibration manually stopped".into() });
        assert!(t.ends_with(r#""Mount":"m","Reason":"Calibration manually stopped"}"#), "{t}");
    }

    #[test]
    fn star_events() {
        let lost = FrameDroppedInfo {
            frame_number: 7,
            time: 1.5,
            avg_dist: 0.456,
            star: FrameErrorInfo {
                star_error: FindResult::LowSnr,
                star_mass: 99.6,
                star_snr: 2.5,
                star_hfd: 3.0,
                status: "Star lost - low SNR",
            },
        };
        assert!(text(EngineEvent::StarLost(lost)).ends_with(
            r#""Frame":7,"Time":1.5,"StarMass":100,"SNR":2.5,"HFD":3.0,"AvgDist":0.46,"ErrorCode":2,"Status":"Star lost - low SNR"}"#
        ));
        let p = Point::new(10.12345, 20.0);
        assert!(text(EngineEvent::StarSelected(p)).ends_with(r#""X":10.123,"Y":20.0}"#));
        assert!(text(EngineEvent::LockPositionSet(p)).contains(r#""Event":"LockPositionSet""#));

        // Looping: the star's measures when found; a drop's only when its
        // star was found (saturated), with its error and status.
        let star = Star { mass: 500.4, snr: 10.0, hfd: 2.0, result: FindResult::Ok, valid: true, ..Star::new() };
        let t = text(EngineEvent::LoopingExposures { frame: 4, star: Some(star), dropped: None });
        assert!(t.ends_with(r#""Frame":4,"StarMass":500,"SNR":10.0,"HFD":2.0}"#), "{t}");
        let drop = FrameErrorInfo { star_error: FindResult::Saturated, ..lost.star };
        let t = text(EngineEvent::LoopingExposures { frame: 5, star: None, dropped: Some(drop) });
        assert!(t.ends_with(r#""StarMass":100,"SNR":2.5,"HFD":3.0,"ErrorCode":1,"Status":"Star lost - low SNR"}"#), "{t}");
        let t = text(EngineEvent::LoopingExposures { frame: 6, star: None, dropped: Some(lost.star) });
        assert!(t.ends_with(r#""Frame":6,"ErrorCode":2,"Status":"Star lost - low SNR"}"#), "{t}");
        let t = text(EngineEvent::LoopingExposures { frame: 1, star: None, dropped: None });
        assert!(t.ends_with(r#""Inst":1,"Frame":1}"#), "{t}");
    }

    #[test]
    fn settle_and_other_events() {
        let t = text(EngineEvent::Settling { distance: 1.234, time: 3.05, settle_time: 10.0, star_locked: true });
        assert!(t.ends_with(r#""Distance":1.23,"Time":3.0,"SettleTime":10.0,"StarLocked":true}"#), "{t}");
        let t = text(EngineEvent::SettleDone { error: None, total_frames: 5, dropped_frames: 1 });
        assert!(t.ends_with(r#""Status":0,"TotalFrames":5,"DroppedFrames":1}"#), "{t}");
        let error = Some("timed-out waiting for guider to settle".to_string());
        let t = text(EngineEvent::SettleDone { error, total_frames: 9, dropped_frames: 0 });
        assert!(t.ends_with(r#""Status":1,"Error":"timed-out waiting for guider to settle","TotalFrames":9,"DroppedFrames":0}"#));
        let t = text(EngineEvent::GuidingDithered { dx: 1.23456, dy: -0.5 });
        assert!(t.ends_with(r#""dx":1.235,"dy":-0.5}"#), "{t}");
        let t = text(EngineEvent::Alert { msg: "a \"b\"\nc".into(), kind: AlertKind::Warning, button: None, dont_show: None });
        assert!(t.ends_with(r#""Msg":"a \"b\"\nc","Type":"warning"}"#), "{t}");
        let t = text(EngineEvent::GuideParamChange { name: "Exposure".into(), value: Value::from(1500) });
        assert!(t.ends_with(r#""Name":"Exposure","Value":1500}"#));
        let t = text(EngineEvent::GuideParamChange { name: "x".into(), value: Value::from(0.1234567) });
        assert!(t.ends_with(r#""Value":0.123457}"#), "{t}");
        let t = text(EngineEvent::SingleFrameComplete { success: false, error: Some("e".into()), path: Some("/p".into()) });
        assert!(t.ends_with(r#""Success":false,"Error":"e","Path":"/p"}"#), "{t}");
        assert_eq!(event(&EngineEvent::Status("x".into()), &o(), 0.0), None);
        assert_eq!(event(&EngineEvent::GuideLogLine("x".into()), &o(), 0.0), None);
    }

    #[test]
    fn catch_up_in_order() {
        let names = |v: Vec<String>| -> Vec<String> {
            v.iter()
                .map(|t| serde_json::from_str::<Value>(t).unwrap()["Event"].as_str().unwrap().to_string())
                .collect()
        };
        let mut snap = Snapshot::default();
        assert_eq!(names(catch_up(&snap, &o(), 0.0)), ["Version", "AppState"]);
        assert!(catch_up(&snap, &o(), 0.0)[1].ends_with(r#""State":"Stopped"}"#));

        // An AO in front of a mount, both calibrated, calibrating the mount.
        snap.app_state = Some(AppState::Calibrating);
        snap.guider_state = Some(GuiderState::CalibratingSecondary);
        snap.lock_position = Some(Point::new(1.0, 2.0));
        snap.star_position = Some(Point::new(1.5, 2.5));
        let view = |name: &str| CalibrationView { present: true, name: name.into(), calibrated: true, ..Default::default() };
        snap.ao_calibration = CalibrationView { ao_max_pos: (45, 40), ..view("AO") };
        snap.mount_calibration = view("Mount");
        let out = catch_up(&snap, &o(), 0.0);
        assert_eq!(
            names(out.clone()),
            ["Version", "LockPositionSet", "StarSelected", "CalibrationComplete", "CalibrationComplete", "StartCalibration", "AppState"]
        );
        assert!(out[3].ends_with(r#""Mount":"AO","Limit":[45,40]}"#), "{}", out[3]);
        assert!(out[4].ends_with(r#""Mount":"Mount"}"#));
        assert!(out[5].ends_with(r#""Mount":"Mount"}"#));
        assert!(out[6].ends_with(r#""State":"Calibrating"}"#));

        snap.app_state = Some(AppState::Guiding);
        assert_eq!(names(catch_up(&snap, &o(), 0.0))[5], "StartGuiding");
        snap.app_state = Some(AppState::Paused);
        assert_eq!(names(catch_up(&snap, &o(), 0.0))[5], "Paused");
        // Lost lock: no StartGuiding.
        snap.app_state = Some(AppState::LostLock);
        snap.mount_calibration.calibrated = false;
        assert_eq!(names(catch_up(&snap, &o(), 0.0)), ["Version", "LockPositionSet", "StarSelected", "CalibrationComplete", "AppState"]);
    }
}
