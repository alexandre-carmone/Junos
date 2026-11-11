//! The engine's bookkeeping around a frame: the files it reads and writes
//! ([`Files`]), where each internal event goes, the guide-log headers, the
//! frame's settings, the dark library and defect map on the camera, and
//! what connecting and disconnecting the gear sets off.
//!
//! Where the events go (the guide log's writer per event, after
//! `api_r8_fix_logs`): a mount's `GuideStep` → the guide-log row and
//! `GuideStep`; `Calibrating` → the event; `CalibrationRow`,
//! `CalibrationAxisComplete` → the guide log's calibration lines;
//! `CalibrationComplete` / `CalibrationFailed` → their line and event;
//! every parameter change → the guide log's `Guiding parameter change`
//! line and `GuideParamChange`; `Info` → the guide log's `INFO:` line. The
//! guider's `FrameDropped` → the DROP row, `CalibrationFrameDropped` → the
//! `STAR LOST during calibration` line, `StartCalibration` → the
//! calibration header and the event, `GuidingStarted` → the guiding
//! header and `StartGuiding`, `GuidingStopped` → `GuidingStopped` and the
//! guide log's end line, a manually stopped calibration's
//! `CalibrationFailed` → the event only. The controller's settling changes,
//! dithers and server commands → their `INFO:` lines (and events);
//! `CheckLogRollover` → both logs reopened on a new imaging day. Debug
//! lines → the debug log; alerts → `Alert` and the debug log's `Alert:`
//! line; status lines → `Status`. Each guide-log line the relay shows also
//! leaves as [`EngineEvent::GuideLogLine`].
//!
//! The profile store's getter traces are written to the debug log after
//! each command or frame, not in their place among the other lines.
//!
//! Ported from PHD2 src/myframe.cpp (LoadProfileSettings, the dark library, NotifyGuidingStarted/Stopped), src/myframe_events.cpp (the dark and defect-map handlers), src/gear_dialog.cpp (DoConnectCamera, ConnectAll, DisconnectAll, EndModal), src/phd.cpp (OpenLogs, CheckLogRollover), src/guidinglog.cpp (the headers' sources) — BSD-3-Clause, see THIRD_PARTY.md

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::Arc;

use chrono::Utc;
use serde_json::{Value, json};

use super::algo::{Axis, MountClass};
use super::calibration::{self, ProfileAccess, ROTATOR_POSITION_UNKNOWN};
use super::camera::{self, DEFAULT_CAMERA_GAIN};
use super::command::{CmdError, Gear, Reply};
use super::control::{Control, ControlEvent};
use super::darks::{self, DefectMap, FileGeometry};
use super::event::EngineEvent;
use super::fits::{CameraCards, SaveHeader, write_fits};
use super::guider::{GuiderEvent, MountRole};
use super::image::{NoiseReduction, UsImage};
use super::log::debug_log::{PHD2_VERSION, execution_banner};
use super::log::guide_log::{
    CameraSummary, GuideLogHeader, MountKind, MountSummary, PointingSummary, SUMMARY_TAIL_BYTES, ScopeSummary,
};
use super::log::image_log::{ImageLogCall, ImageLogOut, ImageLoggerSettings, SaveCx};
use super::mount::{AlertKind, MountEvent, stopwatch_seconds};
use super::scope::Scope;
use super::stepguider::StepGuider;
use super::{AoDev, Engine, ScopeDev, THREAD};

/// The files the engine reads and writes: the logs, the dark library and
/// defect map, the image logger's frames, saved images. Paths are the
/// engine's folder (from [`EngineConfig`]) joined with PHD2's file names.
/// The daemon implements it on the disk; the tests in memory.
pub trait Files: Send {
    /// Open `path` for appending, creating it, and hand back its last
    /// `tail` bytes (none for a new file).
    fn append(&mut self, path: &str, tail: u64) -> io::Result<(Box<dyn Write + Send>, Vec<u8>)>;
    /// The whole file, `None` when it is not there or unreadable.
    fn read(&mut self, path: &str) -> Option<Vec<u8>>;
    /// Create or replace the file.
    fn write(&mut self, path: &str, bytes: &[u8]) -> io::Result<()>;
    fn remove(&mut self, path: &str);
    fn exists(&mut self, path: &str) -> bool;
    /// `mkdir -p`; true when the folder exists afterwards.
    fn mkdir(&mut self, path: &str) -> bool;
}

/// Where the engine's files go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineConfig {
    /// PHD2's instance number: the `N_` of the log names, the dark files'
    /// `_N`.
    pub instance: u32,
    /// The log folder (`/frame/LogDir`, `~/Documents/PHD2` by default).
    pub log_dir: String,
    /// The dark library's and defect map's folder.
    pub darks_dir: String,
    /// PHD2's default file folder (`GetDefaultFileDir`): `save_image`.
    pub data_dir: String,
}

/// `<dir>/<name>`.
fn join(dir: &str, name: &str) -> String {
    format!("{dir}/{name}")
}

impl Engine {
    // -------------------------------------------------------------- the logs

    /// `OpenLogs(false)` (phd.cpp:405): the debug log with its banner, then
    /// the guide log, whatever `/LoggingMode` says.
    pub(super) fn open_logs(&mut self, now: chrono::DateTime<chrono::Local>) {
        let dir = self.cfg.log_dir.clone();
        let files = RefCell::new(&mut self.files);
        let open_debug = |name: &str| files.borrow_mut().append(&join(&dir, name), 0).map(|f| f.0);
        let res = self.dlog.init(true, false, &self.stamp, open_debug);
        self.dlog.write(now, THREAD, &execution_banner(false));
        if let Err(e) = res {
            self.dlog.add_line(now, THREAD, &format!("cannot open the debug log: {e}"));
        }
        let open_guide = |name: &str| files.borrow_mut().append(&join(&dir, name), SUMMARY_TAIL_BYTES);
        let res = self.glog.enable(&self.stamp, None, open_guide);
        if let Err(e) = res {
            self.dlog.add_line(now, THREAD, &format!("cannot open the guide log: {e}"));
        }
        self.store.global().set_bool("/LoggingMode", self.glog.is_enabled());
    }

    /// `CheckLogRollover`: on the first guide start of a new imaging day,
    /// both logs move to files of the new stamp (`OpenLogs(true)`).
    fn check_log_rollover(&mut self) {
        let now = self.wall();
        if !self.stamp.rollover(now) {
            return;
        }
        let header = self.guider.is_guiding().then(|| self.header());
        let dir = self.cfg.log_dir.clone();
        let enabled = self.dlog.is_enabled();
        let files = RefCell::new(&mut self.files);
        let open_debug = |name: &str| files.borrow_mut().append(&join(&dir, name), 0).map(|f| f.0);
        let _ = self.dlog.init(enabled, true, &self.stamp, open_debug);
        self.dlog.write(now, THREAD, &execution_banner(true));
        let _ = self.glog.change_dir_log(
            now,
            &self.stamp,
            header.as_ref(),
            |name| files.borrow_mut().remove(&join(&dir, name)),
            |name| files.borrow_mut().append(&join(&dir, name), SUMMARY_TAIL_BYTES),
        );
    }

    /// A debug-log line.
    pub(super) fn debug(&mut self, line: &str) {
        let now = self.clock.wall();
        self.dlog.add_line(now, THREAD, line);
    }

    /// `pFrame->StatusMsg`.
    pub(super) fn status(&mut self, msg: &str) {
        self.events.push(EngineEvent::Status(msg.to_string()));
    }

    /// `pFrame->Alert`, which PHD2 also writes to the debug log.
    pub(super) fn alert(&mut self, msg: String, kind: AlertKind) {
        self.debug(&format!("Alert: {msg}"));
        self.events.push(EngineEvent::Alert { msg, kind, button: None, dont_show: None });
    }

    /// A guide-log line for the relay, when one was written.
    fn log_line(&mut self, line: Option<String>) {
        if let Some(l) = line {
            self.events.push(EngineEvent::GuideLogLine(l));
        }
    }

    /// The end of a command or a frame: the profile's getter traces, and
    /// one `ConfigurationChange` when the profile changed.
    pub(super) fn finish(&mut self) {
        self.flush_profile();
    }

    pub(super) fn flush_profile(&mut self) {
        for line in self.store.take_debug() {
            self.debug(&line);
        }
        if self.store.take_change() {
            self.events.push(EngineEvent::ConfigurationChange);
        }
    }

    /// The camera's (and the simulator's mount and AO's) debug lines.
    pub(super) fn drain_camera_debug(&mut self) {
        let lines = self.camera.as_deref_mut().map(|c| c.take_debug()).unwrap_or_default();
        for l in lines {
            self.debug(&l);
        }
    }

    // ------------------------------------------------------------ the events

    pub(super) fn emit_guider_events(&mut self, gev: Vec<GuiderEvent>) {
        self.emit_control(gev.into_iter().map(ControlEvent::Guider).collect());
    }

    /// The controller's events (the guider's and the mounts' inside), in
    /// order.
    pub(super) fn emit_control(&mut self, ev: Vec<ControlEvent>) {
        for e in ev {
            match e {
                ControlEvent::Guider(g) => self.emit_guider(g),
                ControlEvent::Mount(role, m) => self.emit_mount(role, m),
                ControlEvent::SettleBegin => self.events.push(EngineEvent::SettleBegin),
                ControlEvent::Settling { distance, time, settle_time, star_locked } => {
                    self.events.push(EngineEvent::Settling { distance, time, settle_time, star_locked })
                }
                ControlEvent::SettleDone { error, total_frames, dropped_frames } => {
                    self.events.push(EngineEvent::SettleDone { error, total_frames, dropped_frames })
                }
                ControlEvent::SettlingStateChange(msg) => {
                    let line = self.glog.settling_state_change(msg);
                    self.log_line(line);
                }
                ControlEvent::GuidingDithered { dx, dy, lock } => {
                    let line = self.glog.guiding_dithered(dx, dy, lock);
                    self.log_line(line);
                    self.events.push(EngineEvent::GuidingDithered { dx, dy });
                }
                ControlEvent::Paused => self.events.push(EngineEvent::Paused),
                ControlEvent::Resumed => self.events.push(EngineEvent::Resumed),
                ControlEvent::ServerCommand(cmd) => {
                    let line = self.glog.server_command(cmd);
                    self.log_line(line);
                }
                ControlEvent::ParamChange { name, value } => self.param_change(name, value, false),
                ControlEvent::LoopingStopped => self.events.push(EngineEvent::LoopingExposuresStopped),
                ControlEvent::SingleFrameComplete { error } => self.single_frame_complete(error),
                ControlEvent::CaptureStarted => {
                    // `StartCapturing`; the camera's `InitCapture` does nothing.
                    self.frame_counter = 0;
                    if self.camera.is_some() {
                        self.check_dark_frame_geometry();
                    }
                }
                ControlEvent::CheckLogRollover => self.check_log_rollover(),
                ControlEvent::SelectDark(ms) => {
                    if self.camera.is_some() {
                        self.darks.select_dark(ms);
                    }
                }
                ControlEvent::SaturationAdu(adu) => {
                    let mut lines = Vec::new();
                    self.cam.set_saturation_by_adu(true, adu, &mut self.store.profile(), &mut lines);
                    for l in lines {
                        self.debug(&l);
                    }
                }
                ControlEvent::Status(s) => self.status(&s),
                ControlEvent::Debug(l) => self.debug(&l),
            }
        }
    }

    /// `NotifyGuidingParam`: the guide log's line and `GuideParamChange`.
    fn param_change(&mut self, name: &str, value: Value, force: bool) {
        let line = self.glog.guiding_param(name, &value, force);
        self.log_line(line);
        self.events.push(EngineEvent::GuideParamChange { name: name.to_string(), value });
    }

    fn emit_guider(&mut self, e: GuiderEvent) {
        let now = self.wall();
        match e {
            GuiderEvent::Mount(role, m) => self.emit_mount(role, m),
            GuiderEvent::PointingAdjusted(_, adj) => {
                for step in adj.steps {
                    match step {
                        calibration::AdjustStep::Log(l) => self.debug(&l),
                        calibration::AdjustStep::Alert(msg) => self.alert(msg, AlertKind::Warning),
                        calibration::AdjustStep::Status(s) => self.status(&s),
                        calibration::AdjustStep::ImageScaleChange => {
                            self.debug("HandleImageScaleChange: image scale changed, calibration cleared")
                        }
                    }
                }
                if let Some(px) = adj.new_pixel_size {
                    let mut lines = Vec::new();
                    self.cam.set_pixel_size(px, &mut self.store.profile(), &mut lines);
                    for l in lines {
                        self.debug(&l);
                    }
                }
            }
            GuiderEvent::LoopingExposures { frame, star, dropped } => {
                self.events.push(EngineEvent::LoopingExposures { frame, star, dropped })
            }
            GuiderEvent::StarSelected(p) => self.events.push(EngineEvent::StarSelected(p)),
            GuiderEvent::StarLost(info) => self.events.push(EngineEvent::StarLost(info)),
            GuiderEvent::FrameDropped(info) => self.glog.frame_dropped(&info),
            GuiderEvent::CalibrationFrameDropped(info) => {
                let line = self.glog.calibration_frame_dropped(&info);
                self.log_line(line);
            }
            GuiderEvent::LockPositionSet(p) => self.events.push(EngineEvent::LockPositionSet(p)),
            GuiderEvent::LockPositionLost => self.events.push(EngineEvent::LockPositionLost),
            GuiderEvent::LockShiftLimitReached => self.events.push(EngineEvent::LockPositionShiftLimitReached),
            GuiderEvent::BacklashStep(p) => self.ga_backlash(Some(p)),
            GuiderEvent::BacklashError => self.ga_backlash(None),
            GuiderEvent::StartCalibration(role) => {
                let header = self.header();
                if let Some(mount) = self.mount_summary(role) {
                    self.glog.start_calibration(now, &header, &mount);
                }
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::StartCalibration { mount });
            }
            GuiderEvent::CalibrationFailed { role, reason } => {
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::CalibrationFailed { mount, reason: reason.to_string() });
            }
            GuiderEvent::GuidingStarted => {
                self.guiding_started = self.clock.mono();
                self.frame_counter = 0;
                self.refresh_ra_guide_speed();
                let header = self.header();
                self.glog.guiding_started(now, &header);
                self.events.push(EngineEvent::StartGuiding);
                self.ga_app_state(true);
            }
            GuiderEvent::GuidingStopped => {
                self.events.push(EngineEvent::GuidingStopped);
                let secs = stopwatch_seconds(self.clock.mono() - self.guiding_started);
                self.glog.guiding_stopped(now, secs);
                self.ga_app_state(false);
            }
            GuiderEvent::ParamChange { name, value, force_log } => self.param_change(name, value, force_log),
            GuiderEvent::SetAutoLoadCalibration(on) => self.set_auto_load_calibration(on),
            // Applied by `Control::forward`, which never passes them on.
            GuiderEvent::AdjustAutoExposure(_) | GuiderEvent::ResetAutoExposure => {}
            GuiderEvent::ImageReplaced(prev) => self.ilog.save_image(prev.0),
            GuiderEvent::ImageLog(call) => self.log_frame_image(call),
            GuiderEvent::Alert { msg, kind, button } => {
                self.debug(&format!("Alert: {msg}"));
                self.events.push(EngineEvent::Alert { msg, kind, button, dont_show: None });
            }
            GuiderEvent::Status(s) => self.status(&s),
            GuiderEvent::Info(l) => {
                let line = self.glog.info(&l);
                self.log_line(line);
            }
            GuiderEvent::Debug(l) => self.debug(&l),
        }
    }

    pub(super) fn emit_mount_events(&mut self, role: MountRole, mev: Vec<MountEvent>) {
        for e in mev {
            self.emit_mount(role, e);
        }
    }

    fn emit_mount(&mut self, role: MountRole, e: MountEvent) {
        match e {
            MountEvent::GuideStep(step) => {
                self.glog.guide_step(&step);
                self.ga_guide_step(&step);
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::GuideStep { mount, step });
            }
            MountEvent::Calibrating(step) => {
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::Calibrating { mount, step });
            }
            MountEvent::CalibrationRow(info) => {
                let line = self.glog.calibration_step(&info);
                self.log_line(line);
            }
            MountEvent::CalibrationAxisComplete { direction, angle, rate, parity } => {
                let line = self.glog.calibration_direct_complete(direction, angle, rate, parity);
                self.log_line(line);
            }
            MountEvent::CalibrationComplete => {
                let mount = self.mount_name(role);
                self.glog.calibration_complete(&mount);
                self.refresh_ra_guide_speed();
                let ao = self.role_is_ao(role);
                self.events.push(EngineEvent::CalibrationComplete { mount, ao });
            }
            MountEvent::CalibrationFailed { reason } => {
                self.glog.calibration_failed(&reason);
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::CalibrationFailed { mount, reason });
            }
            MountEvent::CalibrationDataFlipped => {
                let mount = self.mount_name(role);
                self.events.push(EngineEvent::CalibrationDataFlipped { mount });
            }
            MountEvent::GuideParamChange { name, value } => {
                let line = self.glog.guiding_param(&name, &value, false);
                self.log_line(line);
                self.events.push(EngineEvent::GuideParamChange { name, value });
            }
            MountEvent::Alert { msg, kind, dont_show } => {
                self.debug(&format!("Alert: {msg}"));
                self.events.push(EngineEvent::Alert { msg, kind, button: None, dont_show });
            }
            MountEvent::Status(s) => self.status(&s),
            MountEvent::Info(l) => {
                let line = self.glog.info(&l);
                self.log_line(line);
            }
            MountEvent::Debug(l) => self.debug(&l),
            MountEvent::DebugNoNewline(l) => {
                let now = self.clock.wall();
                self.dlog.write(now, THREAD, &l);
            }
        }
    }

    /// `MyFrame::SetAutoLoadCalibration`.
    fn set_auto_load_calibration(&mut self, on: bool) {
        if self.auto_load_calibration != on {
            self.auto_load_calibration = on;
            self.store.profile().set_bool("/AutoLoadCalibration", on);
        }
    }

    /// `SingleExposure::Complete`: the frame saved when asked (a failure
    /// fails it), the debug line, `SingleFrameComplete`, then the camera's
    /// binning and gain put back.
    fn single_frame_complete(&mut self, error: Option<&'static str>) {
        let Some(st) = self.single.take() else { return };
        let mut succeeded = error.is_none();
        let mut msg = error.unwrap_or_default().to_string();
        let mut path = st.path.clone();
        if succeeded && st.save {
            let p = path.clone().unwrap_or_else(|| self.temp_name("save_image_"));
            let img = self.guider.current_image().clone();
            if self.save_fits(&img, &p).is_err() {
                self.files.remove(&p);
                succeeded = false;
                msg = format!("error saving image file [{p}] -- see debug log for more info");
            }
            path = Some(p);
        }
        self.debug(&format!(
            "SingleExposure::Complete ok={} path=[{}]",
            succeeded as i32,
            path.clone().unwrap_or_default()
        ));
        self.events.push(EngineEvent::SingleFrameComplete {
            success: succeeded,
            error: (!succeeded).then_some(msg),
            path: if st.save { path } else { None },
        });
        let mut lines = Vec::new();
        if let Some(camera) = self.camera.as_deref_mut() {
            let mut prof = self.store.profile();
            if camera.binning() != st.prev_binning {
                camera::set_binning(camera, st.prev_binning as i32, &mut prof, &mut lines);
            }
            if self.cam.gain != st.prev_gain {
                self.cam.set_gain(st.prev_gain, &mut prof, &mut lines);
            }
        }
        for l in lines {
            self.debug(&l);
        }
    }

    /// `wxFileName::CreateTempFileName(<default dir>/<prefix>)`.
    fn temp_name(&mut self, prefix: &str) -> String {
        format!("{}/{prefix}{:06x}", self.cfg.data_dir, self.ctrl.rng.next_u64() & 0xff_ffff)
    }

    /// `usImage::Save`: the frame as FITS with the cards of the moment.
    pub(super) fn save_fits(&mut self, img: &UsImage, path: &str) -> io::Result<()> {
        self.save_fits_note(img, path, "")
    }

    /// `usImage::Save(path, note)`: with a `USERNOTE`.
    pub(super) fn save_fits_note(&mut self, img: &UsImage, path: &str, note: &str) -> io::Result<()> {
        let header = SaveHeader { note: note.to_string(), ..self.save_header() };
        let bytes = write_fits(img, &header).map_err(io::Error::other)?;
        self.files.write(path, &bytes)
    }

    /// The cards `usImage::Save` takes from the application.
    fn save_header(&self) -> SaveHeader {
        let p = self.pointing();
        SaveHeader {
            note: String::new(),
            date: Utc::now(),
            creator: format!("PHD2 {PHD2_VERSION}"),
            profile: self.store.current_profile(),
            camera: self.camera.as_ref().filter(|c| c.is_connected()).map(|c| CameraCards {
                name: c.name(),
                binning: c.binning() as u32,
                pixel_size: self.cam.pixel_size,
                gain: self.cam.gain.max(0) as u32,
                bits_per_pixel: c.bits_per_pixel() as u32,
            }),
            coords: p.and_then(|p| p.coordinates()).map(|c| (c.0, c.1)),
            pier_side: p.map_or(Default::default(), |p| p.pier_side()),
            pixel_scale: self.pixel_scale(),
            lock_pos: self.guider.lock_position(),
        }
    }

    // ------------------------------------------------------- the image logger

    /// One of `UpdateCurrentPosition`'s image logger calls, on the guider's
    /// current image.
    fn log_frame_image(&mut self, call: ImageLogCall) {
        let header = self.save_header();
        let (log_dir, stamp) = (self.cfg.log_dir.clone(), self.stamp);
        let img = self.guider.current_image().clone();
        let Engine { ilog, files, .. } = self;
        let mut mkdir = |p: &str| files.mkdir(p);
        let mut cx = SaveCx { log_dir: &log_dir, stamp: &stamp, header: &header, mkdir: &mut mkdir };
        let out = match call {
            ImageLogCall::Distance { distance, stats } => ilog.log_distance(&img, distance, stats, &mut cx),
            ImageLogCall::Dropped { star_error, active } => ilog.log_dropped(&img, star_error, active, &mut cx),
            ImageLogCall::Deselected => ilog.log_star_deselected(&img, &mut cx),
        };
        self.write_image_log(out);
    }

    /// `ImageLogger::LogAutoSelectImage`.
    pub(super) fn log_auto_select_image(&mut self, img: &Arc<UsImage>, succeeded: bool) {
        let header = self.save_header();
        let (log_dir, stamp) = (self.cfg.log_dir.clone(), self.stamp);
        let Engine { ilog, files, .. } = self;
        let mut mkdir = |p: &str| files.mkdir(p);
        let mut cx = SaveCx { log_dir: &log_dir, stamp: &stamp, header: &header, mkdir: &mut mkdir };
        let out = ilog.log_auto_select(img, succeeded, &mut cx);
        self.write_image_log(out);
    }

    fn write_image_log(&mut self, out: Vec<ImageLogOut>) {
        for o in out {
            match o {
                ImageLogOut::Debug(l) => self.debug(&l),
                ImageLogOut::Save { path, bytes } => {
                    let p = join(&self.cfg.log_dir, &path);
                    // PHD2's line for a file it cannot create; it has none
                    // for a later write error, which the caller cannot tell.
                    if self.files.write(&p, &bytes).is_err() {
                        self.debug("CFITSIO error (105) saving image: couldn't create the named file");
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ the header

    /// What the guide log's headers print, read now.
    pub(super) fn header(&mut self) -> GuideLogHeader {
        let primary = self.mount_summary(MountRole::Primary);
        let secondary = if self.ao.is_some() { self.mount_summary(MountRole::Secondary) } else { None };
        let p = self.pointing();
        let pointing = PointingSummary {
            coords: p.and_then(|p| p.coordinates()),
            pier_side: p.map_or(Default::default(), |p| p.pier_side()),
            latitude: None,
            rotator: self.rotator.as_ref().map(|r| r.position_deg().unwrap_or(ROTATOR_POSITION_UNKNOWN)),
        };
        let camera = self.camera.as_ref().map(|c| {
            let (w, h) = c.frame_size();
            CameraSummary {
                name: c.name(),
                gain: c.has_gain_control().then_some(self.cam.gain),
                read_delay: None,
                port: None,
                frame_width: w,
                frame_height: h,
                dark_exposure_ms: self.darks.current_dark().map_or(0, |d| d.img_exp_dur),
                defect_map: self.darks.defect_map.is_some(),
                pixel_size: self.cam.pixel_size,
            }
        });
        let star = self.guider.primary_star();
        GuideLogHeader {
            profile: self.store.current_profile(),
            dither_ra_only: self.ctrl.dither_ra_only(),
            dither_scale: self.ctrl.dither_scale_factor(),
            noise_reduction: self.nr,
            time_lapse_ms: self.ctrl.time_lapse(),
            server_enabled: self.server_mode,
            pixel_scale: self.pixel_scale(),
            binning: self.binning() as u16,
            focal_length: self.focal_length,
            guider_summary: self.guider.multistar().settings_summary(),
            camera,
            exposure: self.ctrl.exposure_summary(),
            mount: primary,
            secondary_mount: secondary,
            pointing,
            lock_position: self.guider.lock_position().unwrap_or_default(),
            star_position: self.guider.current_position().unwrap_or_default(),
            star_hfd: star.hfd,
        }
    }

    /// The aux mount's name as the mount summary prints it: when it is the
    /// pointing source and reports its position.
    fn aux_summary_name(&self) -> Option<String> {
        let scope_reports =
            self.scope.as_ref().and_then(|s| s.pointing.as_ref()).is_some_and(|p| p.can_report_position());
        match &self.aux {
            Some((name, p)) if !scope_reports && p.can_report_position() => Some(name.clone()),
            _ => None,
        }
    }

    /// `GetSettingsSummary` of a role's mount, `None` without one. A
    /// calibrated scope's stored details are read with their guide-speed
    /// check, as PHD2's `LoadCalibrationDetails` does here.
    pub(super) fn mount_summary(&mut self, role: MountRole) -> Option<MountSummary> {
        let aux = self.aux_summary_name();
        let speeds = self.pointing().filter(|p| p.can_report_position()).and_then(|p| p.guide_rates());
        let mut mev = Vec::new();
        let summary = if self.role_is_ao(role) {
            let Engine { ao, scope, store, .. } = self;
            let a = ao.as_mut()?;
            let validated = scope.as_mut().map(|s| s.scope.cal_details_validated_mut());
            Some(a.sg.settings_summary(aux.as_deref(), &mut store.profile(), validated, &mut mev))
        } else {
            let Engine { scope, store, .. } = self;
            let s = scope.as_mut()?;
            let mut prof = store.profile();
            Some(scope_summary(&mut s.scope, aux, speeds, &mut prof, &mut mev))
        };
        self.emit_mount_events(role, mev);
        summary
    }

    // ------------------------------------------------------ frame settings

    /// The parts of `MyFrame::LoadProfileSettings` that are not the
    /// controller's: noise reduction, auto-restore of the calibration, focal
    /// length, gamma; then the camera's settings, the server switch and the
    /// image logger's.
    pub(super) fn load_frame_settings(&mut self) {
        let mut prof = self.store.profile();
        let nr = prof.get_i32("/NoiseReductionMethod", 0);
        self.nr = NoiseReduction::from_config(nr as i64);
        prof.set_i32("/NoiseReductionMethod", self.nr as i32);
        self.auto_load_calibration = prof.get_i32("/AutoLoadCalibration", -1) == 1;
        let fl = prof.get_i32("/frame/focalLength", 0);
        // `SetFocalLength` refuses a negative length.
        self.focal_length = if fl < 0 { 0 } else { fl };
        prof.set_i32("/frame/focalLength", self.focal_length);
        self.gamma = prof.get_i32("/Gamma", 100) as f64 / 100.0;
        let shutter_closed = self.cam.shutter_closed;
        self.cam = camera::CameraSettings { shutter_closed, ..camera::CameraSettings::load(&prof) };
        let il = ImageLoggerSettings::load(&prof);
        drop(prof);
        self.server_mode = self.store.global().get_bool("/ServerMode", true);
        let line = self.ilog.apply_settings(il);
        self.debug(&line);
    }

    /// The controller's part of `LoadProfileSettings`.
    pub(super) fn load_control_settings(&mut self) {
        let mut cev = Vec::new();
        self.ctrl.load_profile_settings(&mut self.store.profile(), &mut cev);
        self.emit_control(cev);
    }

    /// The controller made anew, its construction logged.
    pub(super) fn new_control(&mut self) {
        let mut cev = Vec::new();
        self.ctrl = Control::new(&mut self.store.profile(), &mut cev);
        self.emit_control(cev);
    }

    // --------------------------------------------------- darks on the camera

    pub(super) fn dark_lib_path(&self) -> String {
        let name = darks::dark_lib_file_name(self.cfg.instance as i32, self.store.current_profile_id());
        join(&self.cfg.darks_dir, &name)
    }

    fn defect_map_path(&self) -> String {
        let name = darks::defect_map_file_name(self.cfg.instance as i32, self.store.current_profile_id());
        join(&self.cfg.darks_dir, &name)
    }

    /// The dark library file's geometry (`DarkLibExists`).
    fn dark_lib_geometry(&mut self) -> FileGeometry {
        let path = self.dark_lib_path();
        self.files.read(&path).map_or(FileGeometry::Missing, |b| FileGeometry::from_fits(&b))
    }

    /// `DarkLibExists(profile, false)`: a dark library file that fits.
    pub(super) fn dark_lib_file_usable(&mut self, frame_size: (i32, i32)) -> bool {
        self.dark_lib_geometry().usable(frame_size)
    }

    /// The defect map's (`DefectMapExists`): its `.txt` decides presence,
    /// its master dark the size.
    fn defect_map_geometry(&mut self) -> FileGeometry {
        let path = self.defect_map_path();
        if !self.files.exists(&path) {
            return FileGeometry::Missing;
        }
        let name = darks::defect_map_master_file_name(self.cfg.instance as i32, self.store.current_profile_id());
        let master = join(&self.cfg.darks_dir, &name);
        self.files.read(&master).map_or(FileGeometry::Unreadable, |b| FileGeometry::from_fits(&b))
    }

    /// `CheckDarkFrameGeometry` (myframe.cpp:2580).
    pub(super) fn check_dark_frame_geometry(&mut self) {
        let size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
        let (lib, map) = (self.dark_lib_geometry(), self.defect_map_geometry());
        let act = self.dark_geometry.check(size, lib, map);
        if act.unload_defect_map {
            self.load_defect_map_handler(false);
        }
        if act.unload_darks {
            self.load_dark_handler(false);
        }
        for a in act.alerts {
            self.alert(a.to_string(), AlertKind::Warning);
        }
    }

    /// `SetDarkMenuState` (myframe.cpp:2624).
    pub(super) fn set_dark_menu_state(&mut self) {
        let size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
        let (lib, map) = (self.dark_lib_geometry(), self.defect_map_geometry());
        for a in self.dark_geometry.refresh(size, lib, map) {
            self.alert(a.to_string(), AlertKind::Warning);
        }
    }

    /// `LoadDarkHandler` (myframe_events.cpp:722).
    pub(super) fn load_dark_handler(&mut self, check: bool) -> bool {
        if !self.camera_connected() {
            self.alert("You must connect a camera before loading a dark library".into(), AlertKind::Warning);
            self.dark_geometry.darks_checked = false;
            return false;
        }
        self.store.profile().set_bool("/camera/AutoLoadDarks", check);
        if check {
            self.dark_geometry.darks_checked = true;
            if self.darks.defect_map.is_some() {
                self.load_defect_map_handler(false);
            }
            if self.load_dark_library() {
                return true;
            }
            self.dark_geometry.darks_checked = false;
            false
        } else {
            if self.darks.current_dark().is_none() {
                self.dark_geometry.darks_checked = false;
                return false;
            }
            self.darks.clear_darks();
            self.dark_geometry.darks_checked = false;
            self.status("Dark library unloaded");
            true
        }
    }

    /// `MyFrame::LoadDarkLibrary` (myframe.cpp:2637).
    fn load_dark_library(&mut self) -> bool {
        let path = self.dark_lib_path();
        let requested = self.ctrl.requested_exposure_duration(self.camera_connected());
        let exposure = self.ctrl.exposure_duration();
        let res = match self.files.read(&path) {
            Some(bytes) => self.darks.load_fits(&bytes, &path, requested, exposure),
            None => Err(String::new()),
        };
        match res {
            Ok(()) => {
                self.debug(&format!("loaded dark library from {path}"));
                self.status("Darks loaded");
                true
            }
            Err(alert) => {
                if !alert.is_empty() {
                    self.alert(alert, AlertKind::Warning);
                }
                self.debug(&format!("failed to load dark frames from {path}"));
                self.status("Darks not loaded");
                false
            }
        }
    }

    /// `LoadDefectMapHandler` (myframe_events.cpp:765).
    pub(super) fn load_defect_map_handler(&mut self, check: bool) {
        if !self.camera_connected() {
            self.alert("You must connect a camera before loading a bad-pixel map".into(), AlertKind::Warning);
            self.dark_geometry.defect_map_checked = false;
            return;
        }
        self.store.profile().set_bool("/camera/AutoLoadDefectMap", check);
        if check {
            let path = self.defect_map_path();
            self.debug(&format!("Loading defect map file {path}"));
            match self.files.read(&path) {
                Some(bytes) => {
                    let map = DefectMap::parse(&String::from_utf8_lossy(&bytes));
                    if self.darks.current_dark().is_some() {
                        self.load_dark_handler(false);
                    }
                    self.darks.defect_map = Some(map);
                    self.dark_geometry.darks_checked = false;
                    self.dark_geometry.defect_map_checked = true;
                    self.status("Defect map loaded");
                }
                None => {
                    self.debug(&format!("Defect map file not found: {path}"));
                    self.status("Defect map not loaded");
                }
            }
        } else {
            if self.darks.defect_map.is_none() {
                self.dark_geometry.defect_map_checked = false;
                return;
            }
            self.darks.defect_map = None;
            self.dark_geometry.defect_map_checked = false;
            self.status("Bad-pixel map unloaded");
        }
    }

    // ---------------------------------------------------------- the gear

    /// `set_connected true` after the daemon connected the devices:
    /// `ConnectAll`'s refusal, then what `DoConnectCamera`, the mounts'
    /// connect buttons and the dialog's `EndModal` do. The gear replaces
    /// what was there; a mount of the same name keeps its object (and its
    /// calibration in memory), as PHD2 keeps it across a reconnect.
    pub(super) fn attach(&mut self, gear: Gear) -> Reply {
        if self.ctrl.capture_active() {
            return Err(CmdError::new(1, "cannot connect equipment when capture is active"));
        }
        self.debug("gear_dialog: ConnectAll calls OnButtonConnectAll");
        self.debug("gear_dialog: OnButtonConnectAll");
        let now = self.clock.mono();
        let Gear { camera, scope, aux_mount, ao, rotator } = gear;
        let mut cam_changed = false;
        match camera {
            Some(c) => cam_changed = self.connect_camera(c),
            None => self.camera = None,
        }

        // The AO, then the scope (OnButtonConnectAll's order).
        let mut mev = Vec::new();
        self.ao = ao.map(|dev| {
            let name = dev.name();
            let mut sg = match self.ao.take() {
                Some(a) if a.sg.mount().name() == name => a.sg,
                _ => StepGuider::new(&mut self.store.profile(), now, &mut mev),
            };
            sg.mount_mut().set_name(&name);
            sg.connect(&*dev, &mut mev);
            AoDev { sg, dev }
        });
        self.emit_mount_events(MountRole::Primary, mev);
        let mut mev = Vec::new();
        self.scope = scope.map(|g| {
            let mut s = match self.scope.take() {
                Some(old) if old.name == g.name => old.scope,
                _ => Scope::new(&mut self.store.profile(), false, now, &mut mev),
            };
            s.mount.set_name(&g.name);
            s.mount.connect();
            // A new pointing source checks the stored guide speeds again.
            *s.cal_details_validated_mut() = false;
            ScopeDev { scope: s, name: g.name, output: g.output, pointing: g.pointing }
        });
        let role = self.scope_role();
        self.emit_mount_events(role, mev);
        if self.scope.is_some() {
            self.status("Mount connected");
        }
        self.aux = aux_mount;
        self.rotator = rotator;

        // `EndModal`: the calibration restored, or cleared for a new camera.
        let primary_ok = match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.mount().is_connected(),
            (None, Some(s)) => s.scope.mount.is_connected(),
            (None, None) => false,
        };
        let secondary_ok = match (&self.ao, &self.scope) {
            (Some(_), Some(s)) => s.scope.mount.is_connected(),
            _ => true,
        };
        if self.auto_load_calibration && !cam_changed {
            if primary_ok && secondary_ok {
                self.debug("Auto-loading calibration data");
                self.load_calibration();
            }
        } else if cam_changed {
            self.debug("Clearing calibration data because camera was changed");
            if let Some(a) = self.ao.as_mut() {
                a.sg.clear_calibration();
            }
            if let Some(s) = self.scope.as_mut() {
                s.scope.clear_calibration();
            }
        }
        self.refresh_ra_guide_speed();
        Ok(json!(0))
    }

    /// `DoConnectCamera` once the camera is connected (gear_dialog.cpp:1077):
    /// its pixel size, the camera-change check, the default gain, a
    /// binning the camera cannot do, the dark library and defect map loaded
    /// when the profile says so. True when the camera changed.
    fn connect_camera(&mut self, mut c: Box<dyn camera::GuideCamera>) -> bool {
        let name = c.name();
        self.debug(&format!("gear_dialog: DoConnectCamera [{name}]"));
        self.status("Connecting to Camera ...");
        let profile_binning = camera::profile_binning(&self.store.profile());
        c.set_binning(profile_binning);
        let mut lines = Vec::new();
        let mut cam_changed = false;
        {
            let mut prof = self.store.profile();
            if let Some(px) = c.pixel_size_um() {
                self.cam.set_pixel_size(px, &mut prof, &mut lines);
            }
            let last = prof.get_str("/camera/LastMenuChoice", "");
            if !last.is_empty() && last != name {
                cam_changed = true;
                lines.push("DoConnectCamera: camera changed".into());
            }
            prof.set_str("/camera/LastMenuChoice", &name);
            if c.has_gain_control() && !prof.has("/camera/gain") {
                lines.push(format!("Initializing camera gain to {DEFAULT_CAMERA_GAIN}%"));
                self.cam.set_gain(DEFAULT_CAMERA_GAIN, &mut prof, &mut lines);
            }
            if profile_binning > c.max_binning() {
                if let Some(s) = self.scope.as_mut() {
                    let d = s.scope.calibration_duration() / profile_binning.max(1) as i32;
                    s.scope.set_calibration_duration(d, &mut prof, &mut Vec::new());
                }
                camera::set_binning(&mut *c, 1, &mut prof, &mut lines);
                lines.push(format!("CamConfigDlg correcting bogus user binning value from {profile_binning} to 1"));
            }
        }
        lines.push(format!("Connected Camera: {name}"));
        let (w, h) = c.frame_size();
        lines.push(format!("FrameSize=({w},{h})"));
        lines.push(format!("PixelSize={}", super::log::cfmt::fmt_f(self.cam.pixel_size, 2)));
        lines.push(format!("BitsPerPixel={}", c.bits_per_pixel()));
        lines.push(format!("HasGainControl={}", c.has_gain_control() as i32));
        if c.has_gain_control() {
            lines.push(format!("GuideCameraGain={}", self.cam.gain));
        }
        lines.push(format!("HasShutter={}", c.has_shutter() as i32));
        lines.push(format!("HasSubFrames={}", c.has_subframes() as i32));
        lines.push(format!("ST4HasGuideOutput={}", c.has_guide_output() as i32));
        for l in lines {
            self.debug(&l);
        }
        self.camera = Some(c);

        // AutoLoadDefectMap, then AutoLoadDarks without a map.
        if self.store.profile().get_bool("/camera/AutoLoadDefectMap", darks::AUTO_LOAD_DEFECT_MAP_DEFAULT) {
            self.debug("auto-loading defect map");
            self.load_defect_map_handler(true);
        }
        if self.darks.defect_map.is_none()
            && self.store.profile().get_bool("/camera/AutoLoadDarks", darks::AUTO_LOAD_DARKS_DEFAULT)
        {
            self.debug("Auto-loading dark library");
            self.load_dark_handler(true);
        }
        self.set_dark_menu_state();
        self.status("Camera Connected");
        cam_changed
    }

    /// `MyFrame::LoadCalibration`: each mount's stored calibration, when
    /// there is one, set again (and stored again with a new time stamp).
    fn load_calibration(&mut self) {
        let ts = super::wx_timestamp(&self.wall());
        let mut mev = Vec::new();
        if let Some(a) = self.ao.as_mut()
            && let Some(cal) = calibration::load_calibration(&self.store.profile(), MountClass::StepGuider)
        {
            a.sg.set_calibration(&cal, &ts, &mut self.store.profile(), &mut mev);
        }
        self.emit_mount_events(MountRole::Primary, mev);
        let mut mev = Vec::new();
        if let Some(s) = self.scope.as_mut()
            && let Some(cal) = calibration::load_calibration(&self.store.profile(), MountClass::Scope)
        {
            s.scope.set_calibration(&cal, &ts, &mut self.store.profile(), &mut mev);
        }
        let role = self.scope_role();
        self.emit_mount_events(role, mev);
    }

    /// `set_connected false` once the daemon disconnected the devices:
    /// `DisconnectAll`'s refusal, then the mounts disconnected.
    pub(super) fn detach(&mut self) -> Reply {
        if self.ctrl.capture_active() {
            return Err(CmdError::new(1, "cannot disconnect equipment while capture active"));
        }
        self.debug("gear_dialog: DisconnectAll calls OnButtonDisconnectAll");
        self.debug("gear_dialog: OnButtonDisconnectAll");
        if let Some(s) = self.scope.as_mut() {
            s.scope.mount.disconnect();
        }
        if let Some(a) = self.ao.as_mut() {
            a.sg.disconnect();
        }
        Ok(json!(0))
    }
}

/// `Scope::GetSettingsSummary`'s values (scope.cpp:1904), the stored
/// details read with their guide-speed check.
fn scope_summary(
    s: &mut Scope,
    aux_mount: Option<String>,
    guide_speeds: Option<(f64, f64)>,
    profile: &mut impl ProfileAccess,
    mev: &mut Vec<MountEvent>,
) -> MountSummary {
    let details = s.load_calibration_details(profile, mev);
    let last_cal_declination = s.mount.get_last_calibration(profile).declination;
    let m = &s.mount;
    let blc = s.backlash_comp();
    MountSummary {
        name: m.name().to_string(),
        connected: m.is_connected(),
        guiding_enabled: m.guiding_enabled(),
        aux_mount,
        calibrated: s.is_calibrated(),
        cal: m.calibration().clone(),
        details,
        x_algo: m.guide_algorithm_selection(Axis::X),
        x_algo_summary: m.algo(Axis::X).settings_summary(),
        y_algo: m.guide_algorithm_selection(Axis::Y),
        y_algo_summary: m.algo(Axis::Y).settings_summary(),
        kind: MountKind::Scope(ScopeSummary {
            backlash_enabled: blc.is_some_and(|b| b.is_enabled()),
            backlash_pulse_ms: blc.map_or(0, |b| b.backlash_pulse_width()),
            max_ra_duration: s.max_ra_duration(),
            max_dec_duration: s.max_dec_duration(),
            dec_guide_mode: s.dec_guide_mode(),
            guide_speeds,
            last_cal_declination,
            calibration_step_ms: s.calibration_duration(),
            calibration_distance_px: s.calibration_distance(),
            assume_orthogonal: s.is_assume_orthogonal(),
        }),
    }
}
