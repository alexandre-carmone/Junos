//! End-to-end tests of the engine on the gear simulator, in simulated time:
//! the client's sequences (loop, auto-select, guide with a settle, dither,
//! pause, stop) through [`Engine::handle`] and [`Engine::step`], the events
//! they give, and the guide log they leave.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::engine::algo::AlgoKind;
use crate::engine::camera::{CameraSettings, CaptureError};
use crate::engine::control::SettleParams;
use crate::engine::darks::DarkLibrary;
use crate::engine::traits::CancelToken;
use crate::profile::ProfileStore;
use crate::sim::{SimCamera, SimParams, SimTime};

/// The files, in memory.
#[derive(Clone, Default)]
struct MemFiles(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

struct MemWriter {
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    path: String,
}

impl Write for MemWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.files.lock().unwrap().entry(self.path.clone()).or_default().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Files for MemFiles {
    fn append(&mut self, path: &str, tail: u64) -> io::Result<(Box<dyn Write + Send>, Vec<u8>)> {
        let mut f = self.0.lock().unwrap();
        let data = f.entry(path.to_string()).or_default();
        let tail = data[data.len().saturating_sub(tail as usize)..].to_vec();
        Ok((Box::new(MemWriter { files: self.0.clone(), path: path.to_string() }), tail))
    }

    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().get(path).cloned()
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().insert(path.to_string(), bytes.to_vec());
        Ok(())
    }

    fn remove(&mut self, path: &str) {
        self.0.lock().unwrap().remove(path);
    }

    fn exists(&mut self, path: &str) -> bool {
        self.0.lock().unwrap().contains_key(path)
    }

    fn mkdir(&mut self, _path: &str) -> bool {
        true
    }
}

/// 3.75 µm pixels behind 400 mm: 1.93″/px, as the profile says.
const PIXEL_UM: f64 = 3.75;
const FOCAL_MM: i32 = 400;

fn scale() -> f64 {
    calibration::pixel_scale(PIXEL_UM, FOCAL_MM, 1)
}

/// The engine on the simulator's camera, its "On Camera" mount pointing at
/// RA 0 h, Dec 25°, and the events it gave.
struct Rig {
    e: Engine,
    cam: SimCamera,
    files: MemFiles,
    ev: Vec<EngineEvent>,
}

impl Rig {
    /// `profile` sets keys before the engine starts; `ao` adds the AO.
    fn new(params: SimParams, ao: bool, profile: impl FnOnce(&mut crate::profile::Section)) -> Rig {
        let mut store = ProfileStore::open(Path::new("/nonexistent/junos-guider-engine-tests"), 1).unwrap();
        store.initialize_profile();
        {
            let mut p = store.profile();
            p.set_f64("/camera/pixelsize", PIXEL_UM);
            p.set_i32("/frame/focalLength", FOCAL_MM);
            // Only the search box is drawn: the runs stay quick.
            p.set_i32("/camera/UseSubframes", 1);
            // The simulator's stars are about 1.6 px across, at PHD2's
            // 1.5 px floor: one centred on a pixel measures under it and is
            // dropped as too small. A lower floor keeps every frame.
            p.set_f64("/guider/StarMinHFD", 1.0);
            profile(&mut p);
        }
        let cam = SimCamera::new(params, SimTime::Simulated);
        cam.connect();
        cam.mount().slew_to(0.0, Some(25.0));
        let files = MemFiles::default();
        let cfg = EngineConfig {
            instance: 1,
            log_dir: "/logs".into(),
            darks_dir: "/darks".into(),
            data_dir: "/data".into(),
        };
        let e = Engine::new(cfg, store, Box::new(cam.clock()), Box::new(files.clone()));
        let mut r = Rig { e, cam, files, ev: Vec::new() };
        let gear = r.gear(Box::new(r.cam.clone()), ao);
        r.cmd(Command::Attach(gear)).unwrap();
        r
    }

    /// The simulator's rig with `camera`: the "On Camera" mount, the AO
    /// when asked for.
    fn gear(&self, camera: Box<dyn GuideCamera>, ao: bool) -> Gear {
        let ao = ao.then(|| {
            let mut dev = self.cam.ao();
            dev.connect();
            Box::new(dev) as Box<dyn StepGuiderDevice + Send>
        });
        Gear {
            camera: Some(camera),
            scope: Some(ScopeGear {
                name: "On Camera".into(),
                output: ScopeOutput::OnCamera,
                pointing: Some(Box::new(self.cam.mount())),
            }),
            ao,
            ..Gear::default()
        }
    }

    fn cmd(&mut self, c: Command) -> Reply {
        let reply = self.e.handle(c);
        self.ev.extend(self.e.take_events());
        reply
    }

    fn step(&mut self) {
        assert!(self.e.has_work(), "nothing to do");
        let ev = self.e.step();
        self.ev.extend(ev);
    }

    /// Steps until an event matches; the steps taken.
    fn run_until(&mut self, max: usize, f: impl Fn(&EngineEvent) -> bool) -> usize {
        let start = self.ev.len();
        for n in 1..=max {
            self.step();
            if self.ev[start..].iter().any(&f) {
                return n;
            }
        }
        panic!("no matching event in {max} steps");
    }

    fn count(&self, f: impl Fn(&EngineEvent) -> bool) -> usize {
        self.ev.iter().filter(|e| f(e)).count()
    }

    fn position(&self, f: impl Fn(&EngineEvent) -> bool) -> Option<usize> {
        self.ev.iter().position(f)
    }

    fn mono(&self) -> f64 {
        use crate::engine::traits::Clock;
        self.cam.clock().mono()
    }

    /// The guide log's text.
    fn guide_log(&self) -> String {
        let files = self.files.0.lock().unwrap();
        let (_, bytes) = files.iter().find(|(k, _)| k.starts_with("/logs/PHD2_GuideLog_")).expect("guide log");
        String::from_utf8(bytes.clone()).unwrap()
    }

    fn debug_log(&self) -> String {
        let files = self.files.0.lock().unwrap();
        let (_, bytes) = files.iter().find(|(k, _)| k.starts_with("/logs/PHD2_DebugLog_")).expect("debug log");
        String::from_utf8(bytes.clone()).unwrap()
    }

    /// Looping with a star selected, then a guide that calibrates and
    /// settles.
    fn guiding(params: SimParams, ao: bool, profile: impl FnOnce(&mut crate::profile::Section)) -> Rig {
        let mut r = Rig::new(params, ao, profile);
        r.cmd(Command::Loop).unwrap();
        r.step();
        r.cmd(Command::FindStar { roi: None }).unwrap();
        r.cmd(Command::Guide { settle: settle(1.5, 10, 60), recalibrate: false, roi: None }).unwrap();
        r.run_until(500, |e| matches!(e, EngineEvent::SettleDone { .. }));
        r
    }
}

fn settle(px: f64, time: i32, timeout: i32) -> SettleParams {
    SettleParams { tolerance_px: px, settle_time_s: time, timeout_s: timeout, frames: 99999 }
}

fn is_guide_step(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::GuideStep { .. })
}

#[test]
fn loop_auto_select_guide_calibrates_then_settles() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let snap = r.e.snapshot();
    assert!(snap.connected);
    assert_eq!(snap.pixel_scale, scale());
    assert_eq!(snap.app_state.unwrap().as_str(), "Stopped");

    r.cmd(Command::Loop).unwrap();
    assert!(r.e.snapshot().capture_active);
    r.step();
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::LoopingExposures { star: None, .. })));
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::FrameReady(_))));
    assert_eq!(r.e.snapshot().app_state.unwrap().as_str(), "Looping");

    let lock = r.cmd(Command::FindStar { roi: None }).unwrap();
    let snap = r.e.snapshot();
    let p = snap.lock_position.unwrap();
    assert_eq!(lock, json!([p.x, p.y]));
    assert_eq!(snap.app_state.unwrap().as_str(), "Selected");
    assert!(r.position(|e| matches!(e, EngineEvent::StarSelected(_))).is_some());

    r.cmd(Command::Guide { settle: settle(1.5, 10, 60), recalibrate: false, roi: None }).unwrap();
    // A second guide while this one runs is refused, as PHD2's.
    let busy = r.cmd(Command::Guide { settle: settle(1.5, 10, 60), recalibrate: false, roi: None });
    assert_eq!(busy, Err(CmdError::new(1, "Cannot initiate guide while guide is in progress")));
    r.run_until(500, |e| matches!(e, EngineEvent::SettleDone { .. }));

    let at = |f: &dyn Fn(&EngineEvent) -> bool| r.position(f).expect("event");
    let start = at(&|e| matches!(e, EngineEvent::StartCalibration { mount } if mount == "On Camera"));
    let first_step = at(&|e| matches!(e, EngineEvent::Calibrating { .. }));
    let complete = at(&|e| matches!(e, EngineEvent::CalibrationComplete { mount, ao: false } if mount == "On Camera"));
    let guiding = at(&|e| matches!(e, EngineEvent::StartGuiding));
    let begin = at(&|e| matches!(e, EngineEvent::SettleBegin));
    let done = at(&|e| matches!(e, EngineEvent::SettleDone { .. }));
    assert!(start < first_step && first_step < complete && complete < guiding && guiding < begin && begin < done);
    let Some(EngineEvent::SettleDone { error, dropped_frames, .. }) = r.ev.get(done) else { unreachable!() };
    assert_eq!((error.as_deref(), *dropped_frames), (None, 0));

    // The calibration went West, East, cleared the Dec backlash, North and
    // South, in that order.
    let mut legs: Vec<&str> = Vec::new();
    for e in &r.ev {
        if let EngineEvent::Calibrating { step, .. } = e
            && legs.last() != Some(&step.direction)
        {
            legs.push(step.direction);
        }
    }
    assert_eq!(&legs[..5], ["West", "East", "Backlash", "North", "South"]);

    // In range for the last 10 s of the settle.
    let settling: Vec<(f64, f64)> = r
        .ev
        .iter()
        .filter_map(|e| match *e {
            EngineEvent::Settling { distance, time, .. } => Some((distance, time)),
            _ => None,
        })
        .collect();
    assert!(settling.last().is_some_and(|s| s.0 <= 1.5 && s.1 >= 9.0), "{settling:?}");

    let snap = r.e.snapshot();
    assert_eq!(snap.app_state.unwrap().as_str(), "Guiding");
    assert!(snap.calibrated && snap.mount_calibration.calibrated);
    assert!(!snap.settling);
    // Calibrated at 1.93″/px, 15″/s: about 7.5 px/s West, Dec 25° away.
    let x_rate = snap.mount_calibration.x_rate * 1000.0;
    assert!((x_rate / (15.0 / scale() * 25f64.to_radians().cos()) - 1.0).abs() < 0.05, "{x_rate}");
    let log = r.guide_log();
    assert!(log.contains("Calibration Begins at "));
    assert!(log.contains("Calibration complete, mount = On Camera."));
    assert!(log.contains("INFO: SETTLING STATE CHANGE, Settling complete"));
    assert!(r.debug_log().contains("PhdController complete: success"));
}

/// The guiding section of the guide log: its header lines, then its rows.
fn guiding_section(log: &str) -> (Vec<&str>, Vec<Vec<&str>>) {
    let start = log.find("Guiding Begins at ").expect("guiding section");
    let section = &log[start..];
    let end = section.find("Guiding Ends at ").unwrap_or(section.len());
    let mut header = Vec::new();
    let mut rows = Vec::new();
    let mut in_rows = false;
    for line in section[..end].lines() {
        if line.starts_with("Frame,Time,mount,") {
            in_rows = true;
        } else if in_rows && line.starts_with(|c: char| c.is_ascii_digit()) {
            rows.push(line.split(',').collect());
        } else if !in_rows {
            header.push(line);
        }
    }
    (header, rows)
}

#[test]
fn every_guide_step_is_a_guide_log_row() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    for _ in 0..40 {
        r.step();
    }
    r.cmd(Command::StopCapture).unwrap();
    while r.e.has_work() {
        r.step();
    }
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::GuidingStopped)));

    let log = r.guide_log();
    assert!(log.starts_with("PHD2 version 2.6.14 [Linux], Log version 2.5. Log enabled at "));
    let (header, rows) = guiding_section(&log);
    let has = |prefix: &str| header.iter().any(|l| l.starts_with(prefix));
    for prefix in [
        "Guiding Begins at ",
        "Dither = both axes, Dither scale = 1.000, Image noise reduction = none",
        "Pixel scale = 1.93 arc-sec/px, Binning = 1, Focal length = 400 mm",
        "Search region = 15 px, Star mass tolerance = 50.0%",
        "Equipment Profile = My Equipment",
        "Camera = Simulator, gain = 95, full size = 752 x 580, no dark, no defect map, pixel size = 3.8 um",
        "Exposure = 1000 ms",
        "Mount = On Camera, connected, guiding enabled, xAngle = ",
        "X guide algorithm = Hysteresis, Hysteresis = 0.100, Aggression = 0.700, Minimum move = ",
        "Y guide algorithm = Resist Switch, Minimum move = ",
        "Backlash comp = disabled, pulse = 0 ms",
        "Max RA duration = 2500, Max DEC duration = 2500, DEC guide mode = Auto",
        "RA = 0.00 hr, Dec = 25.0 deg, Hour angle = ",
        "Lock position = ",
    ] {
        assert!(has(prefix), "no {prefix:?} in {header:#?}");
    }

    // One GuideStep event per row, every row a frame of PHD2's 18 columns.
    let steps: Vec<&mount::GuideStepInfo> = r
        .ev
        .iter()
        .filter_map(|e| match e {
            EngineEvent::GuideStep { step, .. } => Some(step),
            _ => None,
        })
        .collect();
    let drops = rows.iter().filter(|r| r[2] == "\"DROP\"").count();
    assert_eq!(drops, 0);
    assert_eq!(steps.len(), rows.len());
    for (step, row) in steps.iter().zip(&rows) {
        assert_eq!(row.len(), 18, "{row:?}");
        assert_eq!(row[0].parse::<i32>().unwrap(), step.frame_number);
        assert_eq!(row[2], "\"Mount\"");
        assert_eq!(row[1], log::cfmt::fmt_f(step.time, 3));
        assert_eq!(row[3], log::cfmt::fmt_f(step.camera_offset.x, 3));
        assert_eq!(row[9].parse::<i32>().unwrap(), step.duration_ra);
    }
    // The frames count from 1 when guiding starts.
    assert_eq!(steps[0].frame_number, 1);
    assert!(steps.windows(2).all(|w| w[1].frame_number == w[0].frame_number + 1));
    // Guiding near the lock: under a pixel RMS on each axis.
    let rms = |f: &dyn Fn(&mount::GuideStepInfo) -> f64| {
        (steps.iter().map(|s| f(s).powi(2)).sum::<f64>() / steps.len() as f64).sqrt()
    };
    assert!(rms(&|s| s.mount_offset.x) < 1.0 && rms(&|s| s.mount_offset.y) < 1.0);
    assert!(log.contains("Guiding Ends at "));
}

#[test]
fn a_dither_moves_the_lock_and_settles() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.ev.clear();
    let lock = r.e.snapshot().lock_position.unwrap();
    r.cmd(Command::Dither { amount: -3.0, ra_only: false, settle: settle(1.5, 5, 60) }).unwrap();
    let dithered = r.ev.iter().find(|e| matches!(e, EngineEvent::GuidingDithered { .. }));
    let Some(&EngineEvent::GuidingDithered { dx, dy }) = dithered else { panic!("no GuidingDithered") };
    // The amount's absolute value, on both axes.
    assert!(dx.abs() <= 3.0 && dy.abs() <= 3.0 && (dx, dy) != (0.0, 0.0));
    assert!(r.e.snapshot().dithering);
    assert_ne!(r.e.snapshot().lock_position.unwrap(), lock);
    let dithered = r.position(|e| matches!(e, EngineEvent::GuidingDithered { .. })).unwrap();
    let begin = r.position(|e| matches!(e, EngineEvent::SettleBegin)).unwrap();
    assert!(dithered < begin);
    // The snapshot follows the settling, as its last `Settling` event.
    r.run_until(100, |e| matches!(e, EngineEvent::Settling { .. }));
    let last = r.ev.iter().rev().find_map(|e| match *e {
        EngineEvent::Settling { distance, time, settle_time, star_locked } => {
            Some(SettleView { distance, time, settle_time, locked: star_locked })
        }
        _ => None,
    });
    assert_eq!(r.e.snapshot().settle, last);
    r.run_until(100, |e| matches!(e, EngineEvent::SettleDone { .. }));
    assert_eq!(r.e.snapshot().settle, None);
    let done = r.ev.iter().rev().find_map(|e| match e {
        EngineEvent::SettleDone { error, .. } => Some(error.clone()),
        _ => None,
    });
    assert_eq!(done, Some(None));
    assert!(!r.e.snapshot().dithering);
    assert!(r.guide_log().contains("INFO: DITHER by "));
}

#[test]
fn a_full_pause_takes_no_frames_until_resumed() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.ev.clear();
    r.cmd(Command::SetPaused { paused: true, full: true }).unwrap();
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::Paused)));
    let snap = r.e.snapshot();
    assert_eq!(snap.app_state.unwrap().as_str(), "Paused");
    assert_eq!(snap.paused, PauseType::Full);
    // The exposure out completes and is dropped; none follows.
    r.step();
    assert!(!r.ev.iter().any(|e| matches!(e, EngineEvent::FrameReady(_)) || is_guide_step(e)));
    assert!(!r.e.has_work());
    // Pausing again changes nothing.
    r.ev.clear();
    r.cmd(Command::SetPaused { paused: true, full: false }).unwrap();
    assert!(r.ev.iter().all(|e| !matches!(e, EngineEvent::Paused)));

    r.cmd(Command::SetPaused { paused: false, full: false }).unwrap();
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::Resumed)));
    assert!(r.e.has_work());
    for _ in 0..3 {
        r.step();
    }
    assert!(r.count(is_guide_step) >= 2);
    assert_eq!(r.e.snapshot().app_state.unwrap().as_str(), "Guiding");
    let log = r.guide_log();
    assert!(log.contains("INFO: Server received PAUSE") && log.contains("INFO: Server received RESUME"));
}

#[test]
fn stop_capture_after_resuming_schedules_no_exposure() {
    // Ekos' order: set_paused false, then stop_capture.
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::SetPaused { paused: true, full: true }).unwrap();
    r.step();
    assert!(!r.e.has_work());
    r.cmd(Command::SetPaused { paused: false, full: false }).unwrap();
    r.cmd(Command::StopCapture).unwrap();
    r.ev.clear();
    // The exposure the resume scheduled is cut short: no frame, guiding and
    // looping stop, nothing more is scheduled.
    r.step();
    assert!(!r.e.has_work());
    assert!(!r.ev.iter().any(|e| matches!(e, EngineEvent::FrameReady(_)) || is_guide_step(e)));
    let stopped = r.position(|e| matches!(e, EngineEvent::GuidingStopped)).expect("GuidingStopped");
    let looping = r.position(|e| matches!(e, EngineEvent::LoopingExposuresStopped)).expect("stopped");
    assert!(stopped < looping);
    let snap = r.e.snapshot();
    assert!(!snap.capture_active);
    assert_eq!(snap.app_state.unwrap().as_str(), "Stopped");
    assert!(r.debug_log().contains("Error thrown from worker_thread.cpp:184->Time lapse interrupted"));
    // A loop starts again from a full frame.
    r.cmd(Command::Loop).unwrap();
    r.step();
    assert!(r.ev.iter().any(|e| matches!(e, EngineEvent::FrameReady(_))));
}

/// The RA RMS (px) of the guide steps between `from` and `to` simulated
/// seconds of guiding, RA guided by `ra`, on the default simulator (seed 2).
fn ra_rms(ra: AlgoKind, from: f64, to: f64) -> f64 {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |p| {
        p.set_i32("/scope/XGuideAlgorithm", ra as i32);
        p.set_i32("/ExposureDurationMs", 2000);
    });
    let start = r.mono();
    let mut sum = 0.0;
    let mut n = 0;
    while r.mono() - start < to {
        r.ev.clear();
        r.step();
        if r.mono() - start < from {
            continue;
        }
        for e in &r.ev {
            if let EngineEvent::GuideStep { step, .. } = e {
                sum += step.mount_offset.x * step.mount_offset.x;
                n += 1;
            }
        }
    }
    assert!(n > 100, "{n} steps");
    (sum / n as f64).sqrt()
}

#[test]
fn predictive_pec_guides_ra_better_than_hysteresis() {
    let gp = ra_rms(AlgoKind::GaussianProcess, 1200.0, 2400.0);
    let hysteresis = ra_rms(AlgoKind::Hysteresis, 1200.0, 2400.0);
    assert!(gp < hysteresis, "Predictive PEC {gp:.3} px vs Hysteresis {hysteresis:.3} px");
}

#[test]
fn an_ao_calibrates_then_bumps_the_mount() {
    // A short travel and PHD2's default Dec drift (5″/min): the AO walks
    // to its bump zone within a minute.
    let params = SimParams { ao_max_position: 20, ..SimParams::new(scale()) };
    let mut r = Rig::new(params, true, |_| {});
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    r.cmd(Command::Guide { settle: settle(1.5, 5, 120), recalibrate: false, roi: None }).unwrap();
    r.run_until(400, |e| matches!(e, EngineEvent::SettleDone { .. }));

    let at = |f: &dyn Fn(&EngineEvent) -> bool| r.position(f).expect("event");
    let ao_start = at(&|e| matches!(e, EngineEvent::StartCalibration { mount } if mount == "AO-Simulator"));
    let ao_done = at(&|e| matches!(e, EngineEvent::CalibrationComplete { ao: true, .. }));
    let scope_start = at(&|e| matches!(e, EngineEvent::StartCalibration { mount } if mount == "On Camera"));
    let scope_done = at(&|e| matches!(e, EngineEvent::CalibrationComplete { ao: false, .. }));
    assert!(ao_start < ao_done && ao_done < scope_start && scope_start < scope_done);
    let snap = r.e.snapshot();
    assert!(snap.calibrated && snap.ao_calibration.calibrated && snap.mount_calibration.calibrated);
    assert_eq!(snap.ao_calibration.ao_max_pos, (20, 20));

    r.ev.clear();
    for _ in 0..240 {
        r.step();
    }
    let ao_steps = r.count(|e| matches!(e, EngineEvent::GuideStep { step, .. } if step.is_ao));
    let bumps = r.count(|e| matches!(e, EngineEvent::GuideStep { mount, step } if !step.is_ao && mount == "On Camera"));
    assert!(ao_steps >= 230, "{ao_steps}");
    assert!(bumps >= 1, "no bump");
    // The AO never reached its end.
    let (x, y) = r.cam.ao().position();
    assert!(x.abs() < 20 && y.abs() < 20, "{x} {y}");
    let log = r.guide_log();
    assert!(log.contains(",\"AO\",") && log.contains(",\"Mount\","));
}


/// The simulator's camera, failing its captures on demand — or timing
/// out, which disconnects it until reconnected — under another name or
/// pixel size when asked.
struct Flaky {
    cam: SimCamera,
    fail: Arc<AtomicBool>,
    timeout: Arc<AtomicBool>,
    down: bool,
    name: String,
    pixel_um: Option<f64>,
    pixel_xy: Option<(f64, f64)>,
}

impl Flaky {
    fn new(cam: &SimCamera) -> Flaky {
        let (fail, timeout) = (Arc::default(), Arc::default());
        Flaky { cam: cam.clone(), fail, timeout, down: false, name: cam.name(), pixel_um: None, pixel_xy: None }
    }
}

impl GuideCamera for Flaky {
    fn name(&self) -> String {
        self.name.clone()
    }
    fn is_connected(&self) -> bool {
        self.cam.is_connected() && !self.down
    }
    fn reconnect(&mut self, _cancel: &CancelToken) -> Result<(), String> {
        self.down = false;
        Ok(())
    }
    fn frame_size(&self) -> (i32, i32) {
        self.cam.frame_size()
    }
    fn binning(&self) -> u8 {
        self.cam.binning()
    }
    fn set_binning(&mut self, binning: u8) {
        self.cam.set_binning(binning)
    }
    fn max_binning(&self) -> u8 {
        self.cam.max_binning()
    }
    fn pixel_size_um(&self) -> Option<f64> {
        self.pixel_um
    }
    fn pixel_size_xy(&self) -> Option<(f64, f64)> {
        self.pixel_xy
    }
    fn bits_per_pixel(&self) -> u8 {
        self.cam.bits_per_pixel()
    }
    fn has_subframes(&self) -> bool {
        true
    }
    fn has_guide_output(&self) -> bool {
        true
    }
    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String> {
        self.cam.st4_pulse(dir, ms, cancel)
    }
    fn capture(
        &mut self,
        dur_ms: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(CaptureError::Failed(Some("Exposure failed".into())));
        }
        if self.timeout.load(Ordering::SeqCst) {
            self.down = true;
            return Err(CaptureError::Timeout);
        }
        self.cam.capture(dur_ms, kind, subframe, settings, cancel)
    }
}

#[test]
fn a_capture_error_stops_guiding_and_looping() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let flaky = Flaky::new(&r.cam);
    let fail = flaky.fail.clone();
    let gear = r.gear(Box::new(flaky), false);
    r.cmd(Command::Attach(gear)).unwrap();
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    r.cmd(Command::Guide { settle: settle(1.5, 0, 60), recalibrate: false, roi: None }).unwrap();
    r.run_until(200, |e| matches!(e, EngineEvent::StartGuiding));
    r.ev.clear();
    fail.store(true, Ordering::SeqCst);
    r.step();
    assert!(!r.e.has_work());
    let alert = r.position(|e| matches!(e, EngineEvent::Alert { msg, .. } if msg == "Exposure failed")).expect("alert");
    let stopped = r.position(|e| matches!(e, EngineEvent::GuidingStopped)).expect("GuidingStopped");
    let looping = r.position(|e| matches!(e, EngineEvent::LoopingExposuresStopped)).expect("stopped");
    // The guide in progress fails: stopping guiding aborts it first, as
    // PHD2's NotifyGuidingStopped does.
    let done = r
        .position(|e| matches!(e, EngineEvent::SettleDone { error: Some(err), .. } if err == "Guiding stopped"))
        .expect("SettleDone");
    assert!(alert < stopped && stopped < done && done < looping);
    let snap = r.e.snapshot();
    assert!(!snap.capture_active);
    // The guider is back to looping without a star (`Reset(false)`).
    assert_eq!(snap.guider_state, Some(guider::GuiderState::Selecting));
    assert!(r.debug_log().contains("Error thrown from myframe_events.cpp:557->Error reported capturing image"));
}

#[test]
fn a_camera_timeout_reconnects_and_guiding_goes_on() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let flaky = Flaky::new(&r.cam);
    let timeout = flaky.timeout.clone();
    let gear = r.gear(Box::new(flaky), false);
    r.cmd(Command::Attach(gear)).unwrap();
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    r.cmd(Command::Guide { settle: settle(1.5, 0, 60), recalibrate: false, roi: None }).unwrap();
    r.run_until(200, |e| matches!(e, EngineEvent::StartGuiding));
    let exposure = r.e.snapshot().exposure_ms;
    r.ev.clear();

    // The camera disconnects itself on a timeout; PHD2 connects it again
    // and takes the next exposure: clients only see the alert.
    timeout.store(true, Ordering::SeqCst);
    r.step();
    timeout.store(false, Ordering::SeqCst);
    let words = format!("has not completed a {} sec exposure", log::cfmt::fmt_f(exposure as f64 / 1000.0, 1));
    let alert = |e: &EngineEvent| {
        matches!(e, EngineEvent::Alert { msg, .. }
            if msg.contains(&words) && msg.ends_with("PHD will make several attempts to re-connect the camera."))
    };
    assert_eq!(r.count(alert), 1);
    let stopped = |e: &EngineEvent| matches!(e, EngineEvent::GuidingStopped | EngineEvent::LoopingExposuresStopped);
    assert_eq!(r.count(stopped), 0);
    let snap = r.e.snapshot();
    assert!(snap.capture_active && snap.connected && r.e.has_work());
    let dbg = r.debug_log();
    for line in [
        "worker thread skipping SendWorkerThreadExposeComplete",
        "gear_dialog: ReconnectCamera",
        "DoConnectCamera: reconnecting=1",
        "Camera Re-connect succeeded, resume exposures",
    ] {
        assert!(dbg.contains(line), "{line}");
    }
    r.run_until(10, |e| matches!(e, EngineEvent::GuideStep { .. }));

    // Three reconnects a minute at most: two more timeouts reconnect, the
    // next fails the exposure, and capture stops.
    r.ev.clear();
    timeout.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        r.step();
    }
    assert_eq!(r.count(stopped), 0);
    r.step();
    assert!(r.debug_log().contains("More than 3 camera reconnect attempts in less than 60 seconds"));
    assert_eq!(r.count(alert), 3);
    assert!(r.position(|e| matches!(e, EngineEvent::GuidingStopped)).is_some());
    assert!(!r.e.has_work() && !r.e.snapshot().capture_active);
}

#[test]
fn pixels_twice_as_wide_as_tall_are_made_square() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let mut cam = Flaky::new(&r.cam);
    cam.pixel_xy = Some((2.0 * PIXEL_UM, PIXEL_UM));
    let gear = r.gear(Box::new(cam), false);
    r.cmd(Command::Attach(gear)).unwrap();
    r.cmd(Command::Loop).unwrap();
    r.step();
    let (w, h) = r.cam.frame_size();
    let img = r.e.snapshot().image.expect("frame");
    // `SquarePixels` after the capture (cam_indi.cpp:1137).
    assert_eq!((img.w, img.h), (2 * w, h));
}

#[test]
fn connecting_restores_the_calibration_and_loads_the_darks() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::StopCapture).unwrap();
    while r.e.has_work() {
        r.step();
    }
    // The first guide turned auto-restore on: the mount reports its
    // position.
    assert_eq!(r.e.store().profile().get_i32("/AutoLoadCalibration", -1), 1);
    // A dark library on file, the size of the camera's frames.
    let mut lib = DarkLibrary::default();
    let mut dark = UsImage::new(752, 580);
    dark.img_exp_dur = 1000;
    lib.add_dark(dark);
    let mut files = r.files.clone();
    files.write("/darks/PHD2_dark_lib_1.fit", &lib.to_fits("test")).unwrap();

    r.cmd(Command::ClearCalibration(Which::Both)).unwrap();
    assert!(!r.e.snapshot().calibrated);
    r.cmd(Command::Detach).unwrap();
    assert!(!r.e.snapshot().connected);
    let gear = r.gear(Box::new(r.cam.clone()), false);
    r.cmd(Command::Attach(gear)).unwrap();
    let snap = r.e.snapshot();
    assert!(snap.connected && snap.calibrated);
    assert!(snap.dark_loaded && !snap.defect_map_loaded);
    let dbg = r.debug_log();
    assert!(dbg.contains("Auto-loading calibration data") && dbg.contains("loaded dark library from /darks/"));

    // Another camera than the profile chose when it was loaded
    // (`m_lastCamera`): the calibration goes, and is not restored.
    r.cmd(Command::Detach).unwrap();
    r.e.store().profile().set_str("/camera/LastMenuChoice", "INDI Camera [Other]");
    r.cmd(Command::ProfileChanged).unwrap();
    // The profile's gear went with it: its darks too.
    let snap = r.e.snapshot();
    assert!(snap.camera.is_none() && snap.mount.is_none() && !snap.dark_loaded);
    let gear = r.gear(Box::new(r.cam.clone()), false);
    r.cmd(Command::Attach(gear)).unwrap();
    assert!(!r.e.snapshot().calibrated);
    let dbg = r.debug_log();
    assert!(dbg.contains("DoConnectCamera: reconnecting=0 warningIssued=0 lastCam=[INDI Camera [Other]] scaleRatio=1.000"));
    assert!(dbg.contains("DoConnectCamera: displaying camera-change warning"));
    assert!(dbg.contains("Clearing calibration data because camera was changed"));
    // Equipment cannot change while capturing.
    r.cmd(Command::Loop).unwrap();
    let gear = r.gear(Box::new(r.cam.clone()), false);
    assert_eq!(
        r.cmd(Command::Attach(gear)).unwrap_err(),
        CmdError::new(1, "cannot connect equipment when capture is active")
    );
}

#[test]
fn an_indi_camera_switch_keeps_the_calibration_a_new_pixel_size_does_not() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::StopCapture).unwrap();
    while r.e.has_work() {
        r.step();
    }
    let step = r.e.store().profile().get_i32("/scope/CalibrationDuration", 0);
    // The profile chose an INDI camera when it was loaded; another one
    // matches it (`DeviceSelectionMatches`): the calibration is restored.
    r.cmd(Command::Detach).unwrap();
    r.e.store().profile().set_str("/camera/LastMenuChoice", "INDI Camera [A]");
    r.cmd(Command::ProfileChanged).unwrap();
    let mut cam = Flaky::new(&r.cam);
    cam.name = "INDI Camera [B]".into();
    let gear = r.gear(Box::new(cam), false);
    r.cmd(Command::Attach(gear)).unwrap();
    assert!(r.e.snapshot().calibrated);
    assert!(!r.debug_log().contains("DoConnectCamera: camera changed"));

    // The driver's pixels are 20 % smaller than the profile's: the image
    // scale changed, so the calibration is cleared and the step and the
    // min-moves are worked out again (`MakeImageScaleAdjustments`).
    let mut cam = Flaky::new(&r.cam);
    cam.name = "INDI Camera [B]".into();
    cam.pixel_um = Some(PIXEL_UM * 0.8);
    r.cmd(Command::Detach).unwrap();
    let gear = r.gear(Box::new(cam), false);
    r.cmd(Command::Attach(gear)).unwrap();
    assert!(!r.e.snapshot().calibrated);
    let dbg = r.debug_log();
    for line in [
        "scaleRatio=0.800",
        "DoConnectCamera: camera changed",
        "Clearing calibration data because camera was changed",
        "GearDialog::EndModal: imageScaleRatio changed",
        "New image scale properties:  fl= 400, px= 3.000u, bin= 1",
        "Calibrations cleared because of image scale change",
    ] {
        assert!(dbg.contains(line), "{line}");
    }
    // 25 px at 1.55″/px in 12 steps at the simulator's guide speed (1×
    // sidereal): 214 ms, rounded up to 250.
    let new_step = r.e.store().profile().get_i32("/scope/CalibrationDuration", 0);
    assert_eq!(new_step, 250);
    assert!(dbg.contains(&format!("Cal step-size changed from {step} ms to 250 ms")));
    let min_move = algo::smart_default_min_move_for(FOCAL_MM, PIXEL_UM * 0.8, 1);
    let snap = r.e.snapshot();
    let mm = |a: &Option<AlgoView>| a.as_ref().unwrap().params.iter().find(|(n, _)| n == "minMove").unwrap().1;
    assert_eq!((mm(&snap.algo_x), mm(&snap.algo_y)), (min_move, min_move));
}

#[test]
fn the_guiding_assistant_measures_then_tests_the_backlash() {
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.ev.clear();
    // Open while guiding: it starts measuring at once, guide output off.
    r.cmd(Command::GaOpen).unwrap();
    let snap = r.e.snapshot();
    let ga = snap.ga.as_ref().unwrap();
    assert!(ga.measuring && ga.recommendations.is_empty());
    assert!(!snap.mount_calibration.guiding_enabled);
    let change =
        |e: &EngineEvent| matches!(e, EngineEvent::GuideParamChange { name, .. } if name == "MountGuidingEnabled");
    assert!(r.ev.iter().any(change));

    // Stop before two minutes: the countdown.
    for _ in 0..10 {
        r.step();
    }
    r.cmd(Command::GaStop).unwrap();
    let (left, blt) = r.e.snapshot().ga.unwrap().wait.expect("countdown");
    assert!(left > 100 && blt, "{left}");
    r.cmd(Command::Tick).unwrap();
    assert_eq!(r.e.snapshot().ga.unwrap().wait, Some((left - 1, true)));
    // Its countdown ends on its own once the samples cover two minutes,
    // then the backlash test pulses North and South.
    let start = r.mono();
    while r.e.snapshot().ga.unwrap().wait.is_some() {
        r.step();
        r.cmd(Command::Tick).unwrap();
        assert!(r.mono() - start < 200.0, "no end of the countdown");
    }
    assert!(r.e.snapshot().ga.unwrap().measuring_backlash);
    let mut n = 0;
    while r.e.snapshot().ga.unwrap().measuring_backlash {
        r.step();
        n += 1;
        assert!(n < 600, "no end of the backlash test");
    }
    let snap = r.e.snapshot();
    let ga = snap.ga.as_ref().unwrap();
    assert!(!ga.measuring);
    assert!(!ga.recommendations.is_empty());
    // Guiding resumed.
    assert!(snap.mount_calibration.guiding_enabled);
    assert_eq!(snap.app_state.unwrap().as_str(), "Guiding");
    let log = r.guide_log();
    assert!(log.contains("INFO: GA Result - "), "no GA result");
    let dbg = r.debug_log();
    assert!(dbg.contains("BLT: ") && dbg.contains("GuidingAssistant: Disabling guide output"));
    r.cmd(Command::GaClose).unwrap();
    assert!(r.e.snapshot().ga.is_none());
}

#[test]
fn the_darks_dialog_builds_a_library_and_the_defect_map_darks() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    // A frame first, so the camera knows its size.
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::StopCapture).unwrap();
    while r.e.has_work() {
        r.step();
    }
    let kind =
        DarksBuildKind::Library { exposures_ms: vec![500, 1000], count: 2, new_library: true, note: "n".into() };
    r.cmd(Command::DarksBuild(kind)).unwrap();
    assert!(r.e.snapshot().darks_build.is_some());
    let mut frames = 0;
    while r.e.has_work() {
        r.step();
        frames += 1;
    }
    // Two exposures, two frames each; the last ends the build.
    assert_eq!(frames, 4);
    let snap = r.e.snapshot();
    assert!(snap.darks_build.is_none());
    assert!(snap.dark_loaded);
    assert!(r.files.0.lock().unwrap().contains_key("/darks/PHD2_dark_lib_1.fit"));
    let complete = "Operation complete: dark library built";
    // The dialog's message box: the junos UI's, not a PHD2 `Alert`.
    let done = |e: &EngineEvent| matches!(e, EngineEvent::Notice(msg) if msg == complete);
    assert!(r.ev.iter().any(done));
    assert!(!r.ev.iter().any(|e| matches!(e, EngineEvent::Alert { .. })));
    assert!(r.debug_log().contains("Dark library - new dark lib created from scratch."));
    let status = "Building master dark at 500 mSec: Taking dark frame 1/2";
    let first = |e: &EngineEvent| matches!(e, EngineEvent::Status(s) if s == status);
    assert!(r.ev.iter().any(first));

    let kind = DarksBuildKind::DefectMap { exp_s: 5, count: 5, note: String::new() };
    r.cmd(Command::DarksBuild(kind)).unwrap();
    while r.e.has_work() {
        r.step();
    }
    let files = r.files.0.lock().unwrap();
    assert!(files.contains_key("/darks/PHD2_defect_map_master_1.fit"));
    assert!(files.keys().any(|k| k.starts_with("/darks/PHD2_defect_map_master_filt")), "{:?}", files.keys());
    drop(files);
    // Stopped half-way, the library is loaded back from its file.
    let kind =
        DarksBuildKind::Library { exposures_ms: vec![1000], count: 5, new_library: true, note: String::new() };
    r.cmd(Command::DarksBuild(kind)).unwrap();
    r.step();
    r.cmd(Command::DarksStop).unwrap();
    assert!(!r.e.has_work());
    assert!(r.e.snapshot().dark_loaded);
    assert!(r.debug_log().contains("Dark library abort, dark library restored."));
}

#[test]
fn the_polar_drift_tool_measures_the_dec_drift() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::PolarDriftOpen).unwrap();
    // Opening it starts looping.
    assert!(r.e.snapshot().capture_active);
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    r.cmd(Command::PolarDriftStart).unwrap();
    assert!(!r.e.snapshot().mount_calibration.guiding_enabled);
    for _ in 0..60 {
        r.step();
    }
    let pd = r.e.snapshot().polar_drift.unwrap();
    assert!(pd.drifting && pd.status[0].starts_with("Time "), "{:?}", pd.status);
    r.cmd(Command::PolarDriftStop).unwrap();
    let pd = r.e.snapshot().polar_drift.unwrap();
    let result = pd.result.expect("result");
    // PHD2's default 5″/min Dec drift, at Dec 25°: the error that drift
    // gives, `3.8197 × drift(″/min) / cos(dec)`, is ~21′; the sim's
    // seeing and PE blur it.
    assert!(result.pa_error_arcmin > 5.0, "{result:?}");
    assert!(r.e.snapshot().mount_calibration.guiding_enabled);
    r.cmd(Command::PolarDriftClose).unwrap();
    assert!(r.e.snapshot().polar_drift.is_none());
    // Refused while guiding.
    let mut g = Rig::guiding(SimParams::new(scale()), false, |_| {});
    assert_eq!(
        g.cmd(Command::PolarDriftOpen).unwrap_err(),
        CmdError::new(1, "Please wait till Calibration is done and stop guiding")
    );
}

#[test]
fn the_star_cross_test_pulses_the_cross_then_loops() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::StarCrossStart { pulse_count: Some(5), pulse_size: Some(500) }).unwrap();
    // It loops so something shows; the pulses come first.
    assert!(r.e.snapshot().capture_active);
    assert_eq!(
        r.cmd(Command::StarCrossStart { pulse_count: None, pulse_size: None }).unwrap_err(),
        CmdError::new(1, "Star-cross test running")
    );
    let t0 = r.mono();
    for _ in 0..41 {
        r.step();
    }
    // 40 pulses of 0.5 s, no exposure between them.
    assert!((r.mono() - t0 - 20.0).abs() < 1.0, "{}", r.mono() - t0);
    let view = r.e.snapshot().tools["starcross"].clone();
    assert_eq!((view["completed"].as_bool(), view["status"].as_str()), (Some(true), Some("Test completed")));
    assert_eq!(view["progress"], serde_json::json!([40, 40]));
    let log = r.debug_log();
    assert!(log.contains("Star-cross move 1/40, West for 500 ms"), "{log}");
    assert!(log.contains("Star-cross test completed"));
    assert_eq!(r.e.store().profile().get_f64("/SCT/PulseSize", 0.0), 500.0);
    // Stop between two pulses.
    r.cmd(Command::StarCrossStart { pulse_count: None, pulse_size: None }).unwrap();
    r.step();
    r.cmd(Command::StarCrossStop).unwrap();
    assert_eq!(r.e.snapshot().tools["starcross"]["status"], "Test cancelled");
}

#[test]
fn the_lock_is_nudged_saved_restored_and_bookmarked() {
    use crate::engine::BookmarkOp;
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    assert_eq!(r.cmd(Command::SaveLock).unwrap_err(), CmdError::new(1, "no lock position"));
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    let lock = r.e.snapshot().lock_position.unwrap();
    assert_eq!(r.cmd(Command::RestoreLock).unwrap_err(), CmdError::new(1, "no saved lock position"));
    r.cmd(Command::SaveLock).unwrap();
    r.cmd(Command::NudgeLock { dx: 3.0, dy: -1.0 }).unwrap();
    assert_eq!(r.e.snapshot().lock_position, Some(Point::new(lock.x + 3.0, lock.y - 1.0)));
    // The profile keeps doubles as PHD2's config file does, to 6 digits.
    let back = r.cmd(Command::RestoreLock).unwrap();
    let restored = r.e.snapshot().lock_position.unwrap();
    assert_eq!(back, serde_json::json!([restored.x, restored.y]));
    assert!((restored.x - lock.x).abs() < 1e-3 && (restored.y - lock.y).abs() < 1e-3);
    let lock = restored;
    assert!(r.cmd(Command::NudgeLock { dx: -1e6, dy: 0.0 }).is_err());

    r.cmd(Command::Bookmark(BookmarkOp::Lock)).unwrap();
    r.cmd(Command::Bookmark(BookmarkOp::Toggle(Point::new(10.0, 10.0)))).unwrap();
    assert_eq!(r.e.snapshot().tools["bookmarks"].as_array().unwrap().len(), 2);
    let saved = r.e.store().profile().get_str("/guider/bookmarks", "");
    assert_eq!(saved, format!("{:.5} {:.5} 10.00000 10.00000 ", lock.x, lock.y));
    r.cmd(Command::Bookmark(BookmarkOp::Delete(Point::new(12.0, 8.0)))).unwrap();
    assert!(r.cmd(Command::Bookmark(BookmarkOp::Delete(Point::new(12.0, 8.0)))).is_err());
    r.cmd(Command::Bookmark(BookmarkOp::Clear)).unwrap();
    assert_eq!(r.e.snapshot().tools["bookmarks"], serde_json::json!([]));
    assert_eq!(r.e.store().profile().get_str("/guider/bookmarks", "x"), "");
}

#[test]
fn the_static_pa_tool_reads_positions_on_selected_frames() {
    use crate::engine::StaticPaSet;
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    assert_eq!(r.cmd(Command::StaticPaRotate).unwrap_err(), CmdError::new(1, "Static PA tool not open"));
    r.cmd(Command::StaticPaOpen).unwrap();
    assert!(r.e.snapshot().capture_active);
    let v = r.e.snapshot().static_pa.clone();
    // The simulator's mount cannot slew: manual mode, hour angle 18 h.
    assert_eq!((v["auto"].as_bool(), v["can_slew"].as_bool()), (Some(false), Some(false)));
    assert_eq!(v["hour_angle"], 18.0);
    assert_eq!(v["stars"].as_array().unwrap().len(), 8);
    // No star yet.
    r.cmd(Command::StaticPaRotate).unwrap();
    assert_eq!(r.e.snapshot().static_pa["status"], "Please select a star");
    r.step();
    r.cmd(Command::FindStar { roi: None }).unwrap();
    r.cmd(Command::StaticPaRotate).unwrap();
    r.step();
    let v = r.e.snapshot().static_pa.clone();
    assert!(v["positions"][0].is_array() && v["positions"][1].is_null(), "{v}");
    r.cmd(Command::StaticPaSet(StaticPaSet { ref_star: Some(3), ..StaticPaSet::default() })).unwrap();
    assert_eq!(r.e.snapshot().static_pa["ref_star"], 3);
    r.cmd(Command::StaticPaMark(2)).unwrap();
    r.step();
    assert!(r.e.snapshot().static_pa["positions"][1].is_array());
    r.cmd(Command::StaticPaClose).unwrap();
    assert!(r.e.snapshot().static_pa.is_null());
    assert!(r.debug_log().contains("Close StaticPaTool"));
}

#[test]
fn comet_training_takes_the_rate_from_the_lock_moves() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    assert_eq!(r.cmd(Command::CometTrainStart).unwrap_err(), CmdError::new(1, "not guiding"));
    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::CometTrainStart).unwrap();
    assert!(r.e.snapshot().lock_shift.enabled);
    let t0 = r.mono();
    while r.mono() - t0 < 36.0 {
        r.step();
    }
    r.cmd(Command::NudgeLock { dx: 1.0, dy: 0.0 }).unwrap();
    // 1 px in ~36 s: ~100 px/h along x, in camera px.
    let rate = r.e.snapshot().lock_shift.rate;
    assert!((rate.x - 3600.0 / (r.mono() - t0)).abs() < 2.0 && rate.y.abs() < 1e-9, "{rate:?}");
    assert_eq!(r.e.snapshot().tools["comet"]["training"], true);
    // Training stops with guiding.
    r.cmd(Command::StopCapture).unwrap();
    for _ in 0..3 {
        if r.e.has_work() {
            r.step();
        }
    }
    assert!(r.e.snapshot().guider_state != Some(crate::engine::guider::GuiderState::Guiding));
    assert_eq!(r.e.snapshot().tools["comet"]["training"], false);
}

#[test]
fn the_calibration_assistant_calibrates_and_grades_it() {
    use crate::engine::CalAssistCmd as C;
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::CalAssistant(C::Open)).unwrap();
    let v = r.e.snapshot().tools["cal_assistant"].clone();
    // RA 0 h, Dec 25°; the simulator's mount cannot slew.
    assert_eq!(v["current"]["dec"], "+25.0");
    assert_eq!((v["target"]["offset"].as_i64(), v["target"]["dec"].as_i64()), (Some(5), Some(0)));
    assert_eq!(v["status"], "Manually move the telescope to a Dec location near 0");
    assert!(v["explanation"].as_str().unwrap().starts_with("Calibration will be more accurate"));
    r.cmd(Command::CalAssistant(C::Set { offset: Some(70), dec: Some(-3), east: None })).unwrap();
    assert_eq!(r.e.snapshot().tools["cal_assistant"]["target"]["offset"], 50);
    r.cmd(Command::CalAssistant(C::SaveCustom { single_side: true })).unwrap();
    assert_eq!(r.e.store().profile().get_i32("/scope/CalSlew/TgtDec", 0), -3);
    // No slew to make: the calibration starts at once.
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::CalAssistant(C::Calibrate)).unwrap();
    assert_eq!(r.e.snapshot().tools["cal_assistant"]["status"], "Waiting for calibration to complete");
    r.run_until(400, |e| matches!(e, EngineEvent::StartCalibration { .. }));
    r.cmd(Command::Tick).unwrap();
    r.run_until(800, |e| matches!(e, EngineEvent::CalibrationComplete { .. }));
    r.step();
    r.cmd(Command::Tick).unwrap();
    let v = r.e.snapshot().tools["cal_assistant"].clone();
    // Dec 25°: graded down for the sky location.
    assert_eq!(v["verdict"], "Acceptable", "{v}");
    assert!(v["status"].as_str().unwrap().ends_with("Sky location)"), "{v}");
    assert!(r.debug_log().contains("CalAsst: acceptable result, ("));
    // The step size the simulator's profile keeps is far from the
    // calculator's: the sanity check says so.
    assert!(v["sanity"].as_str().unwrap().starts_with("Your current calibration parameters"));
    r.cmd(Command::CalAssistant(C::Close)).unwrap();
    assert!(r.e.snapshot().tools["cal_assistant"].is_null());
}

#[test]
fn the_pier_flip_tool_calibrates_on_the_first_side() {
    let mut a = Rig::new(SimParams::new(scale()), true, |_| {});
    let e = a.cmd(Command::PierFlipOpen).unwrap_err();
    assert_eq!(e.message, "The meridian flip calibration tool requires an equipment profile without an AO");
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::PierFlipOpen).unwrap();
    assert_eq!(r.e.snapshot().tools["pierflip"]["state"], "Intro");
    r.cmd(Command::PierFlipNext).unwrap();
    let v = r.e.snapshot().tools["pierflip"].clone();
    assert_eq!((v["state"].as_str(), v["dec"].as_str()), (Some("Slew1"), Some("+25.0°")));
    r.cmd(Command::Loop).unwrap();
    r.step();
    r.cmd(Command::PierFlipNext).unwrap();
    assert_eq!(r.e.snapshot().tools["pierflip"]["state"], "Calibrate1");
    for _ in 0..1200 {
        r.step();
        if r.e.snapshot().tools["pierflip"]["state"] == "Slew2" {
            break;
        }
    }
    let v = r.e.snapshot().tools["pierflip"].clone();
    assert_eq!(v["state"], "Slew2", "{v}");
    // Looping again, guiding stopped, for the slew.
    assert!(r.e.snapshot().capture_active);
    assert!(r.debug_log().contains("PFT: calibrate1 done, start looping"));
    r.cmd(Command::PierFlipClose).unwrap();
    assert!(r.e.snapshot().tools["pierflip"].is_null());
}

#[test]
fn drift_align_drifts_in_ra_only_then_adjusts() {
    use crate::engine::DriftAlignCmd as D;
    use crate::engine::scope::DecGuideMode;
    let mut n = Rig::new(SimParams::new(scale()), false, |_| {});
    n.cmd(Command::DriftAlign(D::Open)).unwrap();
    n.cmd(Command::DriftAlign(D::Drift)).unwrap();
    let v = n.e.snapshot().drift_align.clone();
    let idle = (v["mode"].as_str(), v["status"].as_str());
    assert_eq!(idle, (Some("Idle"), Some("Please calibrate before starting drift alignment")));

    let mut r = Rig::guiding(SimParams::new(scale()), false, |_| {});
    r.cmd(Command::DriftAlign(D::Open)).unwrap();
    assert_eq!(r.e.snapshot().drift_align["slew"], serde_json::json!({"ra": 0.0, "dec": 0.0}));
    // Opening it stopped guiding (its idle mode); Drift guides again.
    r.cmd(Command::DriftAlign(D::Drift)).unwrap();
    assert_eq!(r.e.snapshot().drift_align["status"], "Start guiding...");
    for _ in 0..10 {
        if r.e.snapshot().drift_align["drifting"] == true {
            break;
        }
        r.step();
    }
    let v = r.e.snapshot().drift_align.clone();
    let drifting = (v["drifting"].as_bool(), v["status"].as_str());
    assert_eq!(drifting, (Some(true), Some("Drifting... click Adjust when done drifting")));
    assert_eq!(r.e.snapshot().dec_guide_mode, Some(DecGuideMode::None));
    for _ in 0..20 {
        r.step();
    }
    let v = r.e.snapshot().drift_align.clone();
    assert!(v["trend"]["points"].as_u64().unwrap() >= 5, "{v}");
    assert!(v["trend"]["error_arcmin"].as_f64().is_some(), "{v}");
    assert!(v["circle"]["radius"].as_f64().is_some(), "{v}");
    r.cmd(Command::DriftAlign(D::Adjust)).unwrap();
    r.step();
    let v = r.e.snapshot().drift_align.clone();
    let adjust = (v["mode"].as_str(), v["status"].as_str());
    assert_eq!(adjust, (Some("Adjust"), Some("Adjust azimuth, click Drift when done")));
    assert!(r.e.snapshot().guider_state != Some(crate::engine::guider::GuiderState::Guiding));
    r.cmd(Command::DriftAlign(D::Phase)).unwrap();
    assert_eq!(r.e.snapshot().drift_align["slew"]["ra"], -65.0);
    r.cmd(Command::DriftAlign(D::Close)).unwrap();
    assert!(r.e.snapshot().drift_align.is_null());
    assert_eq!(r.e.snapshot().dec_guide_mode, Some(DecGuideMode::Auto));
    assert!(r.debug_log().contains("EndDecDrift: DecGuideMode set to Auto (1)"));
}

#[test]
fn the_bad_pixel_map_is_refined_from_its_master_dark() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let e = r.cmd(Command::BpmSet { cold: 75, hot: 75 }).unwrap_err();
    assert_eq!(e.message, "no master dark frames: build the bad-pixel map first");
    let note = String::new();
    r.cmd(Command::DarksBuild(DarksBuildKind::DefectMap { exp_s: 1, count: 2, note })).unwrap();
    while r.e.has_work() {
        r.step();
    }
    let counts = r.cmd(Command::BpmSet { cold: 75, hot: 75 }).unwrap();
    let wide = r.cmd(Command::BpmSet { cold: 100, hot: 100 }).unwrap();
    assert!(wide["hot"].as_u64() >= counts["hot"].as_u64(), "{counts} {wide}");
    // The Darks dialog's 5 s floor for the defect map's darks.
    assert_eq!(counts["exposure_ms"], 5000);
    let built = r.cmd(Command::BpmSave { cold: 60, hot: 80 }).unwrap();
    assert!(r.e.snapshot().defect_map_loaded);
    assert_eq!(r.e.store().profile().get_i32("/camera/dmap_hot_factor", 0), 80);
    let files = r.files.0.lock().unwrap();
    let (_, text) = files.iter().find(|(k, _)| k.ends_with(".txt") && k.contains("defect_map")).expect("map");
    let text = String::from_utf8_lossy(text);
    assert!(text.contains("# Aggressiveness, hot: 80"), "{text}");
    let pixels = text.lines().filter(|l| !l.starts_with('#')).count() as u64;
    assert_eq!(built["hot"].as_u64().unwrap() + built["cold"].as_u64().unwrap(), pixels);
}
