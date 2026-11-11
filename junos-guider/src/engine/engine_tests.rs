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
    r.run_until(100, |e| matches!(e, EngineEvent::SettleDone { .. }));
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


/// The simulator's camera, failing its captures on demand.
struct Flaky {
    cam: SimCamera,
    fail: Arc<AtomicBool>,
}

impl GuideCamera for Flaky {
    fn name(&self) -> String {
        self.cam.name()
    }
    fn is_connected(&self) -> bool {
        self.cam.is_connected()
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
        self.cam.pixel_size_um()
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
        self.cam.capture(dur_ms, kind, subframe, settings, cancel)
    }
}

#[test]
fn a_capture_error_stops_guiding_and_looping() {
    let mut r = Rig::new(SimParams::new(scale()), false, |_| {});
    let fail = Arc::new(AtomicBool::new(false));
    let flaky = Flaky { cam: r.cam.clone(), fail: fail.clone() };
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

    // Another camera in the profile: the calibration goes.
    r.e.store().profile().set_str("/camera/LastMenuChoice", "INDI Camera [Other]");
    r.cmd(Command::Detach).unwrap();
    let gear = r.gear(Box::new(r.cam.clone()), false);
    r.cmd(Command::Attach(gear)).unwrap();
    assert!(!r.e.snapshot().calibrated);
    assert!(r.debug_log().contains("Clearing calibration data because camera was changed"));
    // Equipment cannot change while capturing.
    r.cmd(Command::Loop).unwrap();
    let gear = r.gear(Box::new(r.cam.clone()), false);
    assert_eq!(
        r.cmd(Command::Attach(gear)).unwrap_err(),
        CmdError::new(1, "cannot connect equipment when capture is active")
    );
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
    let done = |e: &EngineEvent| matches!(e, EngineEvent::Alert { msg, .. } if msg == complete);
    assert!(r.ev.iter().any(done));
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
