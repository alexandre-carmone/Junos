//! The guiding engine: PHD2's worker-thread logic, ported as pure code.
//!
//! Nothing in here does I/O. Devices come in through the traits of
//! [`traits`], [`camera`], [`mount`] and [`stepguider`]; time through
//! [`traits::Clock`]; files through [`Files`]; everything the engine tells
//! the outside leaves as [`EngineEvent`]s. The daemon owns the thread and
//! the channels.
//!
//! [`Engine`] is PHD2's `MyFrame` and its worker thread, synchronous: the
//! daemon hands it [`Command`]s between frames ([`Engine::handle`]) and
//! calls [`Engine::step`] while it has work, one exposure (or the moves
//! queued between frames) at a time, in `myframe_events.cpp`'s order:
//!
//! 1. the moves queued since the last frame (manual pulses, the backlash
//!    test's), as the worker thread takes them before the next exposure;
//! 2. the delay before the exposure (the time lapse or the variable delay,
//!    through [`traits::Clock::sleep_ms`]; a stop cuts it short and fails
//!    the exposure, `worker_thread.cpp:184`);
//! 3. the capture, with the cancel token and the camera's watchdog, then
//!    the dark or defect map, the noise reduction and the statistics
//!    (`HandleExpose`);
//! 4. `OnExposeComplete`: a full pause drops the frame; a failed capture
//!    stops capture and guiding; otherwise the frame number, the dark-frame
//!    geometry check, the image logger's ring, the guider's
//!    `UpdateGuideState` (its events through the controller, which takes
//!    the auto-exposure step among them), the image logger's verdict, the
//!    frame shown, then the controller's `UpdateControllerState` and the
//!    next exposure scheduled;
//! 5. the moves the frame scheduled, in order, as the worker thread makes
//!    them before the next exposure: the guide step on pMount, an AO's
//!    bump on the scope right after it (PHD2's secondary thread runs it
//!    beside the next exposure; here it is made before), then
//!    `OnMoveComplete` (the step's `GuideStep` and guide-log row, the late
//!    `GuidingStopped`, a move error's alert).
//!
//! The guide log, the debug log and the image logger are the engine's,
//! written through [`Files`] as the events come (see `session.rs` for the
//! event-to-writer map).
//!
//! Ported from PHD2 src/myframe.cpp, src/myframe_events.cpp, src/worker_thread.cpp, src/gear_dialog.cpp (connect and disconnect all), src/camera.cpp (the capture pipeline), src/event_server.cpp (the method handlers) — BSD-3-Clause, see THIRD_PARTY.md

pub mod algo;
pub mod assistant;
pub mod backlash;
pub mod calibration;
pub mod camera;
pub mod command;
pub mod control;
pub mod darks;
pub mod event;
pub mod fits;
pub mod guider;
pub mod image;
pub mod log;
pub mod mount;
pub mod multistar;
pub mod rng;
pub mod scope;
mod session;
pub mod star;
pub mod stats;
pub mod stepguider;
pub mod tools;
pub mod traits;
mod windows;

#[cfg(test)]
mod engine_tests;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Local};
use serde_json::json;

use self::algo::{AlgoCtx, Axis};
use self::assistant::GuidingAssistant;
use self::calibration::{ProfileAccess, ScopePointing};
use self::camera::{CameraSettings, CaptureKind, GuideCamera, OnCamera, capture_frame};
use self::control::{Control, ControlCx, ControlEvent, ExposureRequest, GuideOpts, SingleExposure, can_guide};
use self::darks::{DarkGeometry, DarkLibrary};
use self::guider::{Guider, GuiderCx, GuiderMount, MountRole, PauseType, ScheduledMove};
use self::image::{NoiseReduction, UsImage, apply_noise_reduction};
use self::log::debug_log::{DebugLog, LogFileStamp, log_file_time};
use self::log::guide_log::GuideLog;
use self::log::image_log::{ImageLogger, ImageLoggerSettings};
use self::mount::{GuideOutput, GuiderOffset, PointingSource, StepFrame, stopwatch_seconds};
use self::multistar::FindParams;
use self::scope::{CalibrationEnv, DecGuideMode, Scope, ScopeIo};
use self::star::FindMode;
use self::stepguider::{AoMount, BumpEnv, StepGuider, StepGuiderDevice};
use self::tools::polar_drift::PolarDrift;
use self::traits::{CancelToken, Clock, Dir, MoveOpts, MoveResult, PierSide, Point, Rect};
use self::windows::{DarksJob, GaCommand};
use crate::profile::ProfileStore;

pub use self::command::{
    CmdError, Command, DarksBuildKind, Gear, INVALID_PARAMS, INVALID_REQUEST, ManualPointing, Reply, Rotator,
    ScopeGear, ScopeOutput, SingleFrame, Which,
};
pub use self::event::{
    AlgoView, CalibrationView, DarksView, DeviceView, EngineEvent, GaView, PolarDriftView, SettleView, Snapshot,
};
pub use self::session::{EngineConfig, Files};

/// The thread id the debug log's records carry (PHD2 writes the writing
/// thread's; one thread does everything here).
const THREAD: u64 = 1;

/// The scope as the engine holds it: PHD2's `Scope` and the devices behind
/// it.
struct ScopeDev {
    scope: Scope,
    name: String,
    output: ScopeOutput,
    pointing: Option<Box<dyn PointingSource + Send>>,
}

/// The AO: PHD2's `StepGuider` and its device.
struct AoDev {
    sg: StepGuider,
    dev: Box<dyn StepGuiderDevice + Send>,
}

/// A move waiting for the worker thread.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pending {
    /// What the guider scheduled (`SchedulePrimaryMove`, `ScheduleAxisMove`):
    /// counted in the mount's `IsBusy` until made.
    Move(ScheduledMove),
    /// `ScheduleManualMove`: a `guide_pulse`, never counted.
    Manual { role: MountRole, dir: Dir, amount: i32 },
    /// `ScheduleSecondaryMove(scope, bump, MOVEOPTS_AO_BUMP)`.
    Bump(GuiderOffset),
}

impl Pending {
    fn role(&self) -> MountRole {
        match *self {
            Pending::Move(ScheduledMove::Offset { .. }) => MountRole::Primary,
            Pending::Move(ScheduledMove::Calibration { role, .. }) | Pending::Manual { role, .. } => role,
            Pending::Bump(_) => MountRole::Secondary,
        }
    }
}

/// `stop_capture` while the engine thread is in a frame. PHD2's main thread
/// runs `StopCapturing` beside its worker and interrupts the exposure, so
/// the exposure comes back with the loop already stopped ("Image capture
/// stopped"). Here commands wait for the frame: the daemon calls
/// [`request`](Self::request), which cuts the exposure short; the engine
/// runs `StopCapturing` as soon as it returns, and the `stop_capture`
/// command that follows only answers.
#[derive(Clone)]
pub struct StopRequest {
    flag: Arc<AtomicBool>,
    cancel: CancelToken,
}

impl StopRequest {
    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.cancel.cancel();
    }
}

/// `SingleExposure`'s part outside the capture loop: what to put back and
/// where to save.
#[derive(Clone, Debug)]
struct SingleState {
    prev_binning: u8,
    prev_gain: i32,
    save: bool,
    path: Option<String>,
}

/// PHD2's `MyFrame` with its worker thread, the gear it drives and its
/// logs. See the module doc for a frame's course.
pub struct Engine {
    cfg: EngineConfig,
    store: ProfileStore,
    clock: Box<dyn Clock + Send>,
    files: Box<dyn Files>,

    // The gear (`pCamera`, `pMount`, `pSecondaryMount`, the aux mount,
    // `pRotator`).
    camera: Option<Box<dyn GuideCamera>>,
    cam: CameraSettings,
    scope: Option<ScopeDev>,
    ao: Option<AoDev>,
    aux: Option<(String, Box<dyn PointingSource + Send>)>,
    manual_pointing: Option<ManualPointing>,
    rotator: Option<Box<dyn Rotator>>,

    // The frame's state.
    guider: Guider,
    ctrl: Control,
    darks: DarkLibrary,
    dark_geometry: DarkGeometry,
    nr: NoiseReduction,
    focal_length: i32,
    gamma: f64,
    server_mode: bool,
    /// `m_autoLoadCalibration`.
    auto_load_calibration: bool,
    /// `m_frameCounter`.
    frame_counter: u32,
    /// When `m_guidingElapsed` last started, monotonic seconds: at start-up,
    /// then each time guiding starts.
    guiding_started: f64,
    /// The calibration's RA guide speed, deg/s (−1 unknown), for the
    /// algorithms; read again when a calibration is set.
    ra_guide_speed: f64,
    /// The worker thread's queue of moves.
    moves: VecDeque<Pending>,
    /// `WorkerThread::RequestStop` (`INT_STOP`): set by a stop, cleared
    /// when an exposure is scheduled.
    cancel: CancelToken,
    /// [`StopRequest`]'s flag, and the stop it already served.
    stop: Arc<AtomicBool>,
    stop_served: bool,
    /// [`Command::Connecting`].
    connecting: bool,
    // The gear dialog's camera-change state (`m_lastCamera`, `m_camChanged`,
    // `m_camWarningIssued`, `m_imageScaleRatio`). PHD2 resets the last three
    // only when its dialog is shown, which never happens here: as in a PHD2
    // driven through the event server alone, they hold until the end.
    last_camera: String,
    cam_changed: bool,
    cam_warning_issued: bool,
    image_scale_ratio: f64,
    /// A capture timed out and the camera is to be connected again
    /// (`SetSkipExposeComplete`, then `TryReconnect`).
    reconnect: bool,
    /// `m_cameraReconnectAttempts`: when each reconnect of the last minute
    /// was tried, wall seconds.
    reconnect_attempts: Vec<i64>,
    /// The last `Settling` event, for the snapshot.
    settle: Option<SettleView>,
    /// `CreateTempFileName`'s names, apart from the dither's generator.
    temp_rng: rng::Rng,
    /// An exposure was scheduled and the cancel token not yet cleared for it.
    exposure_seen: bool,
    single: Option<SingleState>,

    // The tool windows.
    ga: Option<GuidingAssistant>,
    darks_job: Option<DarksJob>,
    polar_drift: Option<PolarDrift>,

    // The logs.
    glog: GuideLog,
    dlog: DebugLog,
    ilog: ImageLogger,
    stamp: LogFileStamp,

    events: Vec<EngineEvent>,
}

impl Engine {
    /// PHD2 starting up: the logs opened, then the guider and the frame
    /// with the profile's settings. No gear until [`Command::Attach`].
    /// `store` has its current profile selected.
    pub fn new(
        cfg: EngineConfig,
        mut store: ProfileStore,
        clock: Box<dyn Clock + Send>,
        mut files: Box<dyn Files>,
    ) -> Engine {
        let now = clock.wall();
        // `GetLogFileTime`: a restart within the imaging day appends to the
        // night's logs.
        let listing = files.list(&cfg.log_dir);
        let time = log_file_time(listing.iter().map(|(n, _)| n.as_str()), now);
        let stamp = LogFileStamp { instance: cfg.instance, time };
        let mut e = Engine {
            cam: CameraSettings::load(&store.profile()),
            guider: Guider::new(&mut store.profile(), false, &mut Vec::new()),
            ctrl: Control::new(&mut store.profile(), &mut Vec::new()),
            cfg,
            store,
            clock,
            files,
            camera: None,
            scope: None,
            ao: None,
            aux: None,
            manual_pointing: None,
            rotator: None,
            darks: DarkLibrary::default(),
            dark_geometry: DarkGeometry::default(),
            nr: NoiseReduction::None,
            focal_length: 0,
            gamma: 1.0,
            server_mode: true,
            auto_load_calibration: false,
            frame_counter: 0,
            guiding_started: 0.0,
            ra_guide_speed: -1.0,
            moves: VecDeque::new(),
            cancel: CancelToken::new(),
            stop: Arc::default(),
            stop_served: false,
            connecting: false,
            last_camera: String::new(),
            cam_changed: false,
            cam_warning_issued: false,
            image_scale_ratio: 1.0,
            reconnect: false,
            reconnect_attempts: Vec::new(),
            settle: None,
            temp_rng: rng::Rng::new(now.timestamp_micros() as u64),
            exposure_seen: false,
            single: None,
            ga: None,
            darks_job: None,
            polar_drift: None,
            glog: GuideLog::new(),
            dlog: DebugLog::new(now),
            ilog: ImageLogger::new(),
            stamp,
            events: Vec::new(),
        };
        // The first guider and controller only fill the struct: they are
        // made again below once the logs are open, so their profile traces
        // and writes are dropped.
        let _ = e.store.take_debug();
        let _ = e.store.take_change();
        e.open_logs(now, &listing);
        // The guider and the frame again, now that the logs are open: their
        // construction writes to both.
        let sticky = e.store.global().get_bool("/StickyLockPosition", false);
        let mut gev = Vec::new();
        e.guider = Guider::new(&mut e.store.profile(), sticky, &mut gev);
        e.emit_guider_events(gev);
        e.load_frame_settings();
        e.new_control();
        e.load_gear_choices();
        e.guiding_started = e.clock.mono();
        e.flush_profile();
        e
    }

    /// The token the daemon sets to cut a blocking wait short (a stop, the
    /// INDI connection lost); the engine clears it when it schedules an
    /// exposure or a move, as PHD2's worker thread does.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// The handle the daemon uses when `stop_capture` comes in while a
    /// frame may be in progress (see [`StopRequest`]).
    pub fn stop_request(&self) -> StopRequest {
        StopRequest { flag: self.stop.clone(), cancel: self.cancel.clone() }
    }

    /// Whether [`step`](Self::step) has anything to do: an exposure is
    /// scheduled, moves wait or a dark build runs. Otherwise the daemon
    /// waits for a command.
    pub fn has_work(&self) -> bool {
        self.ctrl.exposure_request().is_some() || !self.moves.is_empty() || self.darks_job.is_some()
    }

    /// The events since the last call, in PHD2's order.
    pub fn take_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// The profile store, for the daemon's profile methods (between
    /// commands).
    pub fn store(&mut self) -> &mut ProfileStore {
        &mut self.store
    }

    // ----------------------------------------------------------------- frame

    /// One worker cycle (see the module doc): the queued moves, then the
    /// scheduled exposure and everything that follows from it — or, while
    /// the Darks dialog builds, its next dark frame. Returns the events.
    pub fn step(&mut self) -> Vec<EngineEvent> {
        self.run_moves();
        if self.darks_job.is_some() {
            self.darks_step();
        } else if let Some(req) = self.ctrl.exposure_request() {
            self.expose(req);
        }
        self.finish();
        self.take_events()
    }

    /// `HandleExpose`, then `OnExposeComplete`.
    fn expose(&mut self, req: ExposureRequest) {
        self.debug(&format!("worker thread servicing REQUEST_EXPOSE {}", req.duration_ms));
        let mut cev = Vec::new();
        let guiding_enabled = self.primary_guiding_enabled();
        let delay = self.ctrl.exposure_delay(&self.guider, guiding_enabled, &mut cev);
        self.emit_control(cev);
        let captured = if self.milli_sleep(delay) {
            self.debug("Error thrown from worker_thread.cpp:184->Time lapse interrupted");
            None
        } else {
            self.capture(req)
        };
        // A `stop_capture` that came during the exposure: `StopCapturing`
        // ran on PHD2's main thread meanwhile.
        if self.stop.swap(false, Ordering::SeqCst) {
            self.stop_served = true;
            self.call(|c, cx, ev| c.stop_capturing(cx, ev));
        }
        let reconnect = std::mem::take(&mut self.reconnect);
        if reconnect {
            self.debug("worker thread skipping SendWorkerThreadExposeComplete");
        }
        self.debug("worker thread done servicing request");
        self.exposure_seen = false;
        if reconnect {
            self.try_reconnect();
            return;
        }
        self.expose_complete(captured);
    }

    /// `OnExposeComplete` with the frame, or with the capture error.
    fn expose_complete(&mut self, captured: Option<UsImage>) {
        let keep = self.call(|c, cx, ev| c.on_expose_complete(cx.guider, ev));
        if !keep {
            return;
        }
        match captured {
            Some(img) => self.frame(img),
            None => {
                self.call(|c, cx, ev| c.on_capture_error(cx, ev));
            }
        }
        self.note_schedule();
    }

    /// `MyFrame::DoTryReconnect` (myframe.cpp:1375), which the worker
    /// thread hands the main thread after a timeout: at most 3 tries in a
    /// minute. A camera connected again takes the next exposure, without
    /// `OnExposeComplete` for the lost one, so guiding goes on; otherwise
    /// the lost exposure fails as any other.
    fn try_reconnect(&mut self) {
        const TIME_WINDOW: i64 = 60;
        const MAX_ATTEMPTS: usize = 3;
        let now = self.wall().timestamp();
        self.debug(&format!("Try camera reconnect, now = {now}"));
        while self.reconnect_attempts.first().is_some_and(|&t| now - t > TIME_WINDOW) {
            self.reconnect_attempts.remove(0);
        }
        if self.reconnect_attempts.len() + 1 > MAX_ATTEMPTS {
            self.debug(&format!(
                "More than {MAX_ATTEMPTS} camera reconnect attempts in less than {TIME_WINDOW} seconds, \
                 return without reconnect."
            ));
            self.expose_complete(None);
            return;
        }
        self.reconnect_attempts.push(now);
        self.debug("gear_dialog: ReconnectCamera");
        self.connect_camera(true);
        if self.camera_connected() {
            self.debug("Camera Re-connect succeeded, resume exposures");
            self.call(|c, cx, ev| c.resume_exposures(cx, ev));
        } else {
            self.debug("Camera Re-connect failed");
            self.expose_complete(None);
        }
    }

    /// The exposure itself and the camera's post-processing; `None` when
    /// it failed (the alert raised).
    fn capture(&mut self, req: ExposureRequest) -> Option<UsImage> {
        let r = req.subframe;
        self.debug(&format!(
            "Handling exposure in thread, d={} o={:x} r=({},{},{},{})",
            req.duration_ms, 3, r.x, r.y, r.w, r.h
        ));
        // `DisconnectWithAlert` words a timeout with the exposure setting
        // before the camera disconnects (camera.cpp:1521).
        let requested = self.ctrl.requested_exposure_duration(self.camera_connected());
        let sub = (!r.is_empty()).then_some(r);
        let res = match self.camera.as_deref_mut() {
            Some(camera) => capture_frame(
                camera,
                &self.cam,
                req.duration_ms.max(0) as u32,
                CaptureKind::Light,
                sub,
                &self.cancel,
                &*self.clock,
            ),
            None => Err(camera::CaptureError::Failed(None)),
        };
        self.drain_camera_debug();
        match res {
            Ok(mut img) => {
                self.debug("Exposure complete");
                // The camera's own post-processing (cam_indi.cpp:1130-1137):
                // `SubtractDark` (a size mismatch ignored, as in PHD2), then
                // a colour sensor's luminance at binning 1, then pixels
                // made square.
                let _ = self.darks.subtract_dark(&mut img);
                if let Some(c) = self.camera.as_deref() {
                    if c.has_bayer() && c.binning() == 1 {
                        image::quick_l_recon(&mut img);
                    }
                    if let Some((x, y)) = c.pixel_size_xy()
                        && x != y
                    {
                        image::square_pixels(&mut img, x as f32, y as f32);
                    }
                }
                apply_noise_reduction(&mut img, self.nr);
                img.calc_stats();
                Some(img)
            }
            Err(e) => {
                if let Some((msg, reconnect)) = e.alert(requested, &self.cam) {
                    self.alert(msg, mount::AlertKind::Warning);
                    if reconnect {
                        // `InitiateReconnect` from the worker thread.
                        self.debug("worker thread setting skip send exposure complete");
                        self.debug("worker thread queueing reconnect event to GUI thread");
                        self.reconnect = true;
                    }
                }
                self.debug("Error thrown from worker_thread.cpp:195->Capture failed");
                None
            }
        }
    }

    /// The rest of `OnExposeComplete` for a frame.
    fn frame(&mut self, mut img: UsImage) {
        self.frame_counter += 1;
        img.frame_num = self.frame_counter;
        let size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
        if size != self.dark_geometry.prev_frame_size {
            self.check_dark_frame_geometry();
        }
        let img = Arc::new(img);

        // The image logger hears from the guider (its ImageReplaced and
        // ImageLog events), in PHD2's places.
        let frame = img.clone();
        let upd = self.call(move |c, cx, ev| {
            let stopping = !c.continue_capturing();
            let mut gev = Vec::new();
            let upd = cx.guider.update_guide_state(Some(frame), stopping, &mut cx.gcx, &mut gev);
            c.forward(gev, cx, ev);
            upd
        });
        self.queue_moves(&upd.moves);
        self.pa_tools(upd.pa_tools);
        self.events.push(EngineEvent::FrameReady(img));
        self.call(|c, cx, ev| c.after_frame(cx, ev));
        self.run_moves();
    }

    /// `WorkerThread::MilliSleep(ms, INT_ANY)` on the engine clock: true
    /// when a stop cut it short (or was already asked for).
    fn milli_sleep(&mut self, ms: i32) -> bool {
        const MAX_SLEEP: i64 = 100;
        let ms = ms as i64;
        if ms <= MAX_SLEEP {
            if ms > 0 {
                self.clock.sleep_ms(ms as u64);
            }
            return self.cancel.is_cancelled();
        }
        let start = self.clock.mono();
        let mut elapsed = 0;
        while elapsed < ms {
            self.clock.sleep_ms((ms - elapsed).min(MAX_SLEEP) as u64);
            if self.cancel.is_cancelled() {
                return true;
            }
            elapsed = ((self.clock.mono() - start) * 1000.0) as i64;
        }
        false
    }

    /// After a call that may have scheduled an exposure: the worker
    /// thread's enqueue clears a stop asked for before it
    /// (worker_thread.cpp:88).
    fn note_schedule(&mut self) {
        match self.ctrl.exposure_request() {
            Some(_) if !self.exposure_seen => {
                self.exposure_seen = true;
                self.cancel.reset();
            }
            Some(_) => {}
            None => self.exposure_seen = false,
        }
    }

    // ----------------------------------------------------------------- moves

    /// The worker thread's enqueue, which clears a stop asked for before it
    /// (worker_thread.cpp:260, 281).
    fn queue_moves(&mut self, moves: &[ScheduledMove]) {
        if !moves.is_empty() {
            self.cancel.reset();
        }
        self.moves.extend(moves.iter().map(|&m| Pending::Move(m)));
    }

    /// The worker thread's queue, in order.
    fn run_moves(&mut self) {
        while let Some(mv) = self.moves.pop_front() {
            self.make_move(mv);
        }
    }

    /// `IsBusy` on either mount: a move scheduled and not yet made.
    fn busy(&self) -> bool {
        self.moves.iter().any(|m| !matches!(m, Pending::Manual { .. }))
    }

    /// `IsBusy` on one mount.
    fn busy_role(&self, role: MountRole) -> bool {
        self.moves.iter().any(|m| !matches!(m, Pending::Manual { .. }) && m.role() == role)
    }

    /// `HandleMove` and `OnMoveComplete`.
    fn make_move(&mut self, mv: Pending) {
        let role = mv.role();
        let manual = matches!(mv, Pending::Manual { .. });
        let algo = self.algo_ctx();
        let ra_only = self.ra_only();
        let settling = self.ctrl.is_settling();
        let slow_bump_suppressed = self.suppressed("SlowBumpWarningEnabled");
        let secondary_busy = self.busy_role(MountRole::Secondary);
        let mut mev = Vec::new();
        let mut lines = Vec::new();
        let mut bump = None;
        let result = {
            let Engine { scope, ao, camera, store, guider, clock, cancel, frame_counter, guiding_started, .. } = self;
            let (frame_counter, guiding_started) = (*frame_counter, *guiding_started);
            let (clock, guider) = (&**clock, &*guider);
            // `MoveOffset` reads these after the pulses (mount.cpp:1059).
            let frame = || {
                let s = guider.primary_star();
                StepFrame {
                    frame_number: frame_counter as i32,
                    time: stopwatch_seconds(clock.mono() - guiding_started),
                    star_mass: s.mass,
                    star_snr: s.snr,
                    star_hfd: s.hfd,
                    star_error: s.result.error_code(),
                    avg_dist: guider.current_error(ra_only, clock.wall().timestamp_millis()),
                }
            };
            let mut prof = store.profile();
            // The AO is pMount when there is one; the scope is pSecondaryMount.
            let on_ao = role == MountRole::Primary && ao.is_some();
            if on_ao {
                let a = ao.as_mut().expect("AO");
                match mv {
                    Pending::Move(ScheduledMove::Offset { mut ofs, opts }) => {
                        let env = BumpEnv {
                            secondary_connected: scope.as_ref().is_some_and(|s| s.scope.mount.is_connected()),
                            secondary_busy,
                            max_move_pixels: guider.max_move_pixels(),
                            settling,
                            now: clock.mono(),
                            slow_bump_warning_suppressed: slow_bump_suppressed,
                        };
                        let (r, b) = a.sg.move_offset(&mut ofs, opts, frame, &algo, &env, &mut *a.dev, &prof, &mut mev);
                        bump = b;
                        r
                    }
                    Pending::Move(ScheduledMove::Calibration { dir, amount, .. }) => {
                        a.sg.move_axis(dir, amount, MoveOpts::CALIBRATION_MOVE, &mut *a.dev, &mut mev)
                    }
                    Pending::Manual { dir, amount, .. } => {
                        a.sg.move_axis(dir, amount, MoveOpts::MANUAL, &mut *a.dev, &mut mev)
                    }
                    // A bump is the scope's (`Secondary`): never the AO's.
                    Pending::Bump(_) => MoveResult::Error,
                }
            } else if let Some(s) = scope.as_mut() {
                let now = clock.mono();
                let mut none = NoOutput;
                let mut on_camera;
                let output: &mut dyn GuideOutput = match &mut s.output {
                    ScopeOutput::Mount(o) | ScopeOutput::OnAo(o) => &mut **o,
                    ScopeOutput::OnCamera => match camera.as_deref_mut() {
                        Some(c) => {
                            on_camera = OnCamera(c, &mut lines);
                            &mut on_camera
                        }
                        None => &mut none,
                    },
                };
                let mut io = ScopeIo { output, pointing: None, cancel, now };
                match mv {
                    Pending::Move(ScheduledMove::Offset { mut ofs, opts }) => {
                        s.scope.move_offset(&mut ofs, opts, frame, &algo, &mut io, &mut prof, &mut mev)
                    }
                    Pending::Bump(mut ofs) => {
                        s.scope.move_offset(&mut ofs, MoveOpts::AO_BUMP, frame, &algo, &mut io, &mut prof, &mut mev)
                    }
                    Pending::Move(ScheduledMove::Calibration { dir, amount, .. }) => {
                        s.scope.move_axis(dir, amount, MoveOpts::CALIBRATION_MOVE, &mut io, &mut mev)
                    }
                    Pending::Manual { dir, amount, .. } => {
                        s.scope.move_axis(dir, amount, MoveOpts::MANUAL, &mut io, &mut mev)
                    }
                }
            } else {
                MoveResult::Error
            }
        };
        for l in lines {
            self.debug(&l);
        }
        self.drain_camera_debug();
        self.debug(&format!("move complete, result={}", result as i32));
        if let Some(b) = bump {
            self.debug(&format!(
                "ScheduleSecondaryMove({}, x={}, y={}, opts={})",
                self.mount_name(MountRole::Secondary),
                log::cfmt::fmt_f(b.camera_ofs.x, 2),
                log::cfmt::fmt_f(b.camera_ofs.y, 2),
                MoveOpts::AO_BUMP.0
            ));
            // Made next: PHD2's secondary worker thread starts it at once.
            self.moves.push_front(Pending::Bump(b));
        }

        // `OnMoveComplete`.
        if manual {
            self.emit_mount_events(role, mev);
            self.debug(&format!("Manual Move completed, result = {}", result as i32));
            return;
        }
        self.emit_mount_events(role, mev);
        self.call(|c, cx, ev| {
            let mut gev = Vec::new();
            cx.guider.move_complete(&mut cx.gcx, &mut gev);
            c.forward(gev, cx, ev);
        });
        if result != MoveResult::Ok {
            self.move_failed(role, result);
        }
    }

    /// `OnMoveComplete` for a failed move.
    fn move_failed(&mut self, role: MountRole, result: MoveResult) {
        match self.role_is_ao(role) {
            true => {
                if let Some(a) = self.ao.as_mut() {
                    a.sg.mount_mut().increment_error_count();
                }
            }
            false => {
                if let Some(s) = self.scope.as_mut() {
                    s.scope.mount.increment_error_count();
                }
            }
        }
        match result {
            MoveResult::Slewing => {
                self.debug("mount move error indicates guiding should stop");
                self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                });
            }
            MoveResult::AoLimit => {
                let calibrating = self.guider.is_calibrating();
                let msg = self.ao.as_ref().map(|a| a.sg.limit_alert(calibrating)).unwrap_or_default();
                self.alert(msg, mount::AlertKind::Warning);
                self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    if calibrating {
                        cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
                    } else {
                        // Recentred: guide again from there, on a lock
                        // found anew.
                        let sticky = cx.guider.lock_pos_is_sticky();
                        cx.guider.set_lock_pos_is_sticky(false);
                        cx.guider.stop_guiding(&mut cx.gcx, &mut gev);
                        cx.guider.start_guiding(&mut cx.gcx, &mut gev);
                        cx.guider.set_lock_pos_is_sticky(sticky);
                    }
                    c.forward(gev, cx, ev);
                });
            }
            _ => {}
        }
        self.debug("Error thrown from myframe_events.cpp:676->Error reported moving");
    }

    // --------------------------------------------------------- the borrows

    /// A controller or guider call: the controller, a [`ControlCx`] lent
    /// from the engine's state, and the events, written once it returns.
    fn call<R>(&mut self, f: impl FnOnce(&mut Control, &mut ControlCx, &mut Vec<ControlEvent>) -> R) -> R {
        let camera_connected = self.camera_connected();
        let frame_size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
        let (exposure_ms, auto_exposure) = self.ctrl.exposure_info(camera_connected);
        let cal_env = self.cal_env();
        let sp = self.scope_pointing();
        let algo = self.algo_ctx();
        let find = self.find_params();
        let ra_only = self.ra_only();
        let settling = self.ctrl.is_settling();
        let guide_time = self.clock.mono() - self.guiding_started;
        let now_ms = self.now_ms();
        let mono_ms = (self.clock.mono() * 1000.0) as i64;
        let busy = self.busy();
        let (gamma, use_subframes) = (self.gamma, self.cam.use_subframes);
        let mut ev = Vec::new();
        let r = {
            let Engine { ctrl, guider, scope, ao, aux, manual_pointing, store, .. } = self;
            let (scope_mount, scope_pointing) = match scope.as_mut() {
                Some(ScopeDev { scope, pointing, .. }) => (Some(scope), pointing.as_deref()),
                None => (None, None),
            };
            let pointing = pointing_source(scope_pointing, aux.as_ref().map(|a| &*a.1), manual_pointing.as_ref());
            let mut ao_mount;
            let (primary, secondary): (Option<&mut dyn GuiderMount>, Option<&mut dyn GuiderMount>) = match ao.as_mut() {
                Some(a) => {
                    ao_mount = AoMount { ao: &mut a.sg, dev: &mut *a.dev };
                    (Some(&mut ao_mount), scope_mount.map(|s| s as &mut dyn GuiderMount))
                }
                None => (scope_mount.map(|s| s as &mut dyn GuiderMount), None),
            };
            let mut prof = store.profile();
            let mut cx = ControlCx {
                guider,
                gcx: GuiderCx {
                    primary,
                    secondary,
                    profile: &mut prof,
                    pointing,
                    cal_env: &cal_env,
                    scope_pointing: &sp,
                    algo: &algo,
                    find,
                    use_subframes,
                    ra_only,
                    settling,
                    exposure_ms,
                    auto_exposure,
                    guide_time,
                    now_ms,
                    busy,
                    gamma,
                },
                camera_connected,
                frame_size,
                mono_ms,
            };
            f(ctrl, &mut cx, &mut ev)
        };
        self.emit_control(ev);
        self.note_schedule();
        r
    }

    // ------------------------------------------------------- what is read

    /// `pCamera && pCamera->Connected`.
    fn camera_connected(&self) -> bool {
        self.camera.as_ref().is_some_and(|c| c.is_connected())
    }

    fn now_ms(&self) -> i64 {
        self.clock.wall().timestamp_millis()
    }

    fn wall(&self) -> DateTime<Local> {
        self.clock.wall()
    }

    /// `pPointingSource`.
    fn pointing(&self) -> Option<&dyn PointingSource> {
        let scope = self.scope.as_ref().and_then(|s| s.pointing.as_deref());
        pointing_source(scope, self.aux.as_ref().map(|a| &*a.1), self.manual_pointing.as_ref())
    }

    /// `MyFrame::GuidingRAOnly`: `TheScope()` guides Dec in no direction.
    fn ra_only(&self) -> bool {
        self.scope.as_ref().is_some_and(|s| s.scope.dec_guide_mode() == DecGuideMode::None)
    }

    /// pMount's guide output (`GetGuidingEnabled`), true without pMount.
    fn primary_guiding_enabled(&self) -> bool {
        match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.mount().guiding_enabled(),
            (None, Some(s)) => s.scope.mount.guiding_enabled(),
            (None, None) => true,
        }
    }

    /// Whether `role` is the AO.
    fn role_is_ao(&self, role: MountRole) -> bool {
        role == MountRole::Primary && self.ao.is_some()
    }

    /// The equipment name of a role's mount, empty without one.
    fn mount_name(&self, role: MountRole) -> String {
        if self.role_is_ao(role) {
            return self.ao.as_ref().map_or(String::new(), |a| a.sg.mount().name().to_string());
        }
        match (role, &self.ao) {
            (MountRole::Secondary, None) => String::new(),
            _ => self.scope.as_ref().map_or(String::new(), |s| s.scope.mount.name().to_string()),
        }
    }

    /// `GetCameraPixelScale`: 1.0 when the pixel size or the focal length
    /// is unknown.
    fn pixel_scale(&self) -> f64 {
        match &self.camera {
            Some(c) if self.cam.pixel_size != 0.0 && self.focal_length != 0 => {
                calibration::pixel_scale(self.cam.pixel_size, self.focal_length, c.binning() as u16)
            }
            _ => 1.0,
        }
    }

    fn binning(&self) -> u8 {
        self.camera.as_ref().map_or(1, |c| c.binning())
    }

    fn rotator_angle(&self) -> Option<f64> {
        self.rotator.as_ref().map(|r| r.position_deg().unwrap_or(calibration::ROTATOR_POSITION_UNKNOWN))
    }

    /// What a calibration reads.
    fn cal_env(&self) -> CalibrationEnv {
        let can_pulse_guide = self.scope.as_ref().is_some_and(|s| match &s.output {
            ScopeOutput::Mount(o) | ScopeOutput::OnAo(o) => o.can_pulse_guide(),
            ScopeOutput::OnCamera => false,
        });
        CalibrationEnv {
            binning: self.binning() as u16,
            focal_length: self.focal_length,
            pixel_size: self.cam.pixel_size,
            image_scale: self.pixel_scale(),
            rotator_angle: self.rotator_angle(),
            fast_recenter: self.guider.is_fast_recenter_enabled(),
            max_move_pixels: self.guider.max_move_pixels(),
            can_pulse_guide,
            timestamp: wx_timestamp(&self.wall()),
        }
    }

    /// Where the scope points, for the adjustment when guiding starts.
    fn scope_pointing(&self) -> ScopePointing {
        let p = self.pointing();
        ScopePointing {
            declination: p.and_then(|p| p.declination_radians()),
            pier_side: p.map_or(PierSide::Unknown, |p| p.pier_side()),
            rotator_angle: self.rotator_angle(),
            binning: self.binning() as u16,
            guide_rates: p.filter(|p| p.can_report_position()).and_then(|p| p.guide_rates()),
            camera_pixel_size: self.cam.pixel_size,
            profile_pixel_size: self.cam.pixel_size,
        }
    }

    /// The algorithms' view of the rig.
    fn algo_ctx(&self) -> AlgoCtx {
        let p = self.pointing();
        let x_rate = match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.mount().x_rate(),
            (None, Some(s)) => s.scope.mount.x_rate(),
            (None, None) => 0.0,
        };
        let connected = self.camera_connected();
        AlgoCtx {
            snr: self.guider.primary_star().snr,
            exposure_s: self.ctrl.requested_exposure_duration(connected) as f64 / 1000.0,
            now: self.clock.mono(),
            x_rate_px_ms: x_rate,
            ra_guide_speed: self.ra_guide_speed,
            ra_h: p.and_then(|p| p.coordinates()).map(|c| c.0),
            pier: p.map_or(PierSide::Unknown, |p| p.pier_side()),
        }
    }

    /// What `Star::Find` reads from the camera; the find mode is the
    /// frame's, centroid (PHD2 keeps no setting for it).
    fn find_params(&self) -> FindParams {
        self.cam.find_params(FindMode::Centroid)
    }

    /// A `/Confirm/<profile>/<key>` alert switched off.
    fn suppressed(&mut self, key: &str) -> bool {
        let id = self.store.current_profile_id();
        !self.store.global().get_bool(&format!("/Confirm/{id}/{key}"), true)
    }

    // --------------------------------------------------------------- commands

    /// One command, answered as PHD2's event server answers it once the
    /// parameters parsed. Its events wait in [`take_events`](Self::take_events).
    pub fn handle(&mut self, cmd: Command) -> Reply {
        let reply = self.dispatch(cmd);
        self.finish();
        reply
    }

    fn dispatch(&mut self, cmd: Command) -> Reply {
        let ok = || Ok(json!(0));
        match cmd {
            Command::Loop | Command::Guide { .. } if self.connecting => {
                Err(CmdError::new(1, "all equipment must be connected first"))
            }
            Command::Loop => {
                if self.call(|c, cx, ev| c.start_looping(cx, ev)) {
                    ok()
                } else {
                    Err(CmdError::new(1, "could not start looping"))
                }
            }
            Command::StopCapture => {
                // Already run when the frame it interrupted came back.
                self.stop.store(false, Ordering::SeqCst);
                if std::mem::take(&mut self.stop_served) {
                    return ok();
                }
                if !self.call(|c, cx, ev| c.stop_capturing(cx, ev)) {
                    // `RequestStop`: the exposure scheduled is cut short.
                    self.cancel.cancel();
                }
                ok()
            }
            Command::Guide { settle, recalibrate, roi } => {
                let mut recal = recalibrate;
                if recal && !self.store.global().get_bool("/server/guide_allow_recalibrate", true) {
                    self.debug("ignoring client recalibration request since guide_allow_recalibrate = false");
                    recal = false;
                }
                let opts = GuideOpts { force_recal: recal, use_sticky_lock: true };
                self.call(|c, cx, ev| {
                    can_guide(cx).map_err(|e| CmdError::new(1, e))?;
                    c.guide(opts, settle, roi, cx, ev).map_err(|e| CmdError::new(1, e))
                })?;
                ok()
            }
            Command::Dither { amount, ra_only, settle } => {
                self.call(|c, cx, ev| c.dither(amount.abs(), ra_only, settle, cx, ev))
                    .map_err(|e| CmdError::new(1, e))?;
                ok()
            }
            Command::SetPaused { paused, full } => {
                let pause = match (paused, full) {
                    (false, _) => PauseType::None,
                    (true, false) => PauseType::Guiding,
                    (true, true) => PauseType::Full,
                };
                self.call(|c, cx, ev| c.set_paused(pause, cx, ev));
                ok()
            }
            Command::FindStar { roi } => {
                let found = self.auto_select_star(roi);
                match self.guider.lock_position() {
                    Some(p) if found => Ok(json!([p.x, p.y])),
                    _ => Err(CmdError::new(1, "could not find star")),
                }
            }
            Command::DeselectStar => {
                self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    cx.guider.reset(true, &mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                });
                ok()
            }
            Command::SetLockPosition { x, y, exact } => {
                let p = Point::new(x, y);
                let set = self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    let set = if exact {
                        cx.guider.set_lock_position(p, &mut cx.gcx, &mut gev)
                    } else {
                        cx.guider.set_lock_pos_to_star_at_position(p, &mut cx.gcx, &mut gev)
                    };
                    c.forward(gev, cx, ev);
                    set
                });
                if set { ok() } else { Err(CmdError::new(INVALID_REQUEST, "could not set lock position")) }
            }
            Command::ClearCalibration(which) => {
                if matches!(which, Which::Mount | Which::Both)
                    && let Some(s) = self.scope.as_mut()
                {
                    s.scope.clear_calibration();
                }
                if matches!(which, Which::Ao | Which::Both)
                    && let Some(a) = self.ao.as_mut()
                {
                    a.sg.clear_calibration();
                }
                ok()
            }
            Command::FlipCalibration => {
                let ts = wx_timestamp(&self.wall());
                let Some(s) = self.scope.as_mut() else { return ok() };
                let mut mev = Vec::new();
                let flipped = s.scope.flip_calibration(&ts, &mut self.store.profile(), &mut mev);
                let role = self.scope_role();
                self.emit_mount_events(role, mev);
                if flipped { ok() } else { Err(CmdError::new(1, "could not flip calibration")) }
            }
            Command::SetExposure(ms) => {
                let mut cev = Vec::new();
                let set = self.ctrl.set_exposure_duration(ms, &mut self.store.profile(), &mut cev);
                self.emit_control(cev);
                if set { ok() } else { Err(CmdError::new(1, "could not set exposure duration")) }
            }
            Command::SetDecGuideMode(mode) => {
                if let Some(s) = self.scope.as_mut() {
                    let mut mev = Vec::new();
                    s.scope.set_dec_guide_mode(mode as i32, &mut self.store.profile(), &mut mev);
                    let role = self.scope_role();
                    self.emit_mount_events(role, mev);
                }
                ok()
            }
            Command::SetGuideOutputEnabled(enable) => {
                if !self.set_primary_guiding_enabled(enable) {
                    return Err(CmdError::new(1, "mount not defined"));
                }
                ok()
            }
            Command::SetAlgoParam { axis, name, value } => {
                let set = match self.primary_mount_mut() {
                    Some(m) => m.algo_mut(axis).set_param(&name, value),
                    None => false,
                };
                if !set {
                    return Err(CmdError::new(1, "could not set param"));
                }
                self.save_primary_algorithm(axis);
                ok()
            }
            Command::GuidePulse { amount, dir, which } => self.guide_pulse(amount, dir, which),
            Command::CaptureSingleFrame(req) => self.capture_single_frame(req),
            Command::SetLockShiftEnabled(enable) => {
                let upd = self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    let upd = cx.guider.enable_lock_shift(enable, &mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                    upd
                });
                self.queue_moves(&upd.moves);
                ok()
            }
            Command::SetLockShiftParams { rate, units, mount_coords } => {
                self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    cx.guider.set_lock_shift_rate(rate, units, mount_coords, &mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                });
                ok()
            }
            Command::SetVariableDelay { enabled, short_ms, long_ms } => {
                let mut cev = Vec::new();
                self.ctrl.set_variable_delay_config(enabled, short_ms, long_ms, &mut self.store.profile(), &mut cev);
                self.emit_control(cev);
                ok()
            }
            Command::SetLimitFrame(_) => match self.camera {
                None => Err(CmdError::new(1, "no guide camera")),
                // Neither the INDI camera nor the simulator limits frames.
                Some(_) => Err(CmdError::new(1, "guide camera does not support frame limiting")),
            },
            Command::SetCoolerState(enable) => self.set_cooler_state(enable),
            Command::SaveImage => self.save_image(),
            Command::Attach(gear) => self.attach(gear),
            Command::Detach => self.detach(),
            Command::ProfileChanged => {
                // `LoadGearChoices` makes the gear anew from the new profile:
                // the camera with its darks and defect map, the mounts with
                // their algorithms and calibration. The daemon connects the
                // new profile's devices at the next `set_connected`.
                self.camera = None;
                self.scope = None;
                self.ao = None;
                self.aux = None;
                self.rotator = None;
                self.darks = DarkLibrary::default();
                self.dark_geometry = DarkGeometry::default();
                self.load_gear_choices();
                self.load_frame_settings();
                self.load_control_settings();
                let upd = self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    let upd = cx.guider.load_profile_settings(&mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                    upd
                });
                self.queue_moves(&upd.moves);
                // `SetProfile` then closes the dialog.
                self.end_modal();
                self.refresh_ra_guide_speed();
                ok()
            }
            Command::DeviceLost(which) => {
                if matches!(which, Which::Mount | Which::Both)
                    && let Some(s) = self.scope.as_mut()
                {
                    s.scope.mount.disconnect();
                }
                if matches!(which, Which::Ao | Which::Both)
                    && let Some(a) = self.ao.as_mut()
                {
                    a.sg.disconnect();
                }
                ok()
            }
            Command::Connecting(on) => {
                if on && self.ctrl.capture_active() {
                    return Err(CmdError::new(1, "cannot connect equipment when capture is active"));
                }
                self.connecting = on;
                ok()
            }
            Command::Tick => {
                self.ga_tick();
                ok()
            }
            Command::SelectStar { x, y } => {
                let set = self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    let set = cx.guider.set_lock_pos_to_star_at_position(Point::new(x, y), &mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                    set
                });
                if set { ok() } else { Err(CmdError::new(1, "could not select star")) }
            }
            Command::SetCalibrationData { which, cal } => self.set_calibration_data(which, cal),
            Command::NudgeLock { dx, dy } => {
                let Some(lock) = self.guider.lock_position() else {
                    return Err(CmdError::new(1, "no lock position"));
                };
                let p = Point::new(lock.x + dx, lock.y + dy);
                let set = self.call(|c, cx, ev| {
                    let mut gev = Vec::new();
                    let set = cx.guider.set_lock_position(p, &mut cx.gcx, &mut gev);
                    c.forward(gev, cx, ev);
                    set
                });
                if set { ok() } else { Err(CmdError::new(INVALID_REQUEST, "could not set lock position")) }
            }
            Command::SetAlgorithm { axis, kind } => {
                let now = self.clock.mono();
                let Engine { scope, ao, store, .. } = self;
                let mut mev = Vec::new();
                let mut prof = store.profile();
                let m = match (ao.as_mut(), scope.as_mut()) {
                    (Some(a), _) => a.sg.mount_mut(),
                    (None, Some(s)) => &mut s.scope.mount,
                    (None, None) => return Err(CmdError::new(1, "mount not defined")),
                };
                if !algo::allowed(kind, axis, m.class()) {
                    return Err(CmdError::new(1, "algorithm not allowed on this axis"));
                }
                m.set_guide_algorithm(axis, kind as i32, &mut prof, now, &mut mev);
                drop(prof);
                self.emit_mount_events(MountRole::Primary, mev);
                ok()
            }
            Command::ResetAlgoParams { axis } => {
                let now = self.clock.mono();
                let smart = algo::smart_default_min_move_current(
                    self.focal_length,
                    self.cam.pixel_size,
                    self.binning() as u32,
                );
                let Engine { scope, ao, store, .. } = self;
                let m = match (ao.as_mut(), scope.as_mut()) {
                    (Some(a), _) => a.sg.mount_mut(),
                    (None, Some(s)) => &mut s.scope.mount,
                    (None, None) => return Err(CmdError::new(1, "mount not defined")),
                };
                m.reset_guide_algorithm_params(axis, smart, &mut store.profile(), now);
                ok()
            }
            Command::SetDitherMode { mode, ra_only, scale } => {
                let mut cev = Vec::new();
                let mut prof = self.store.profile();
                self.ctrl.set_dither_mode(mode, &mut prof, &mut cev);
                self.ctrl.set_dither_ra_only(ra_only, &mut prof);
                let set = self.ctrl.set_dither_scale_factor(scale, &mut prof, &mut cev);
                drop(prof);
                self.emit_control(cev);
                if set { ok() } else { Err(CmdError::new(1, "invalid dither scale factor")) }
            }
            Command::SetAutoExposure { min_ms, max_ms, target_snr } => {
                let mut cev = Vec::new();
                let set =
                    self.ctrl.set_auto_exposure_cfg(min_ms, max_ms, target_snr, &mut self.store.profile(), &mut cev);
                self.emit_control(cev);
                match set {
                    Some(_) => ok(),
                    None => Err(CmdError::new(1, "invalid auto exposure settings")),
                }
            }
            Command::SetImageLogger(ui) => {
                let s = ImageLoggerSettings::from_dialog(self.ilog.settings(), ui);
                let line = self.ilog.apply_settings(s);
                self.debug(&line);
                s.save(&mut self.store.profile());
                ok()
            }
            Command::SetManualPointing(p) => {
                self.manual_pointing = p;
                if let Some(s) = self.scope.as_mut() {
                    *s.scope.cal_details_validated_mut() = false;
                }
                ok()
            }
            Command::GaOpen => self.ga_open(),
            Command::GaStart => self.ga_start(),
            Command::GaStop => self.ga_command(GaCommand::Stop),
            Command::GaEndWait { accept } => self.ga_command(GaCommand::EndWait(accept)),
            Command::GaSetMeasureBacklash(on) => self.ga_command(GaCommand::MeasureBacklash(on)),
            Command::GaApply(kind) => self.ga_command(GaCommand::Apply(kind)),
            Command::GaReview(ts) => self.ga_command(GaCommand::Review(ts)),
            Command::GaClose => self.ga_command(GaCommand::Close),
            Command::DarksBuild(kind) => self.darks_build(kind),
            Command::DarksStop => self.darks_stop(),
            Command::DarksUse(on) => Ok(json!(self.load_dark_handler(on))),
            Command::DefectMapUse(on) => {
                self.load_defect_map_handler(on);
                Ok(json!(self.darks.defect_map.is_some()))
            }
            Command::PolarDriftOpen => self.polar_drift_open(),
            Command::PolarDriftStart => self.polar_drift_start(),
            Command::PolarDriftStop => self.polar_drift_stop(false),
            Command::PolarDriftClose => self.polar_drift_stop(true),
        }
    }

    /// `MyFrame::AutoSelectStar` and the image logger's save of its frame.
    fn auto_select_star(&mut self, roi: Option<Rect>) -> bool {
        let found = self.call(|c, cx, ev| c.auto_select_star(roi, cx, ev));
        if !self.guider.is_calibrating_or_guiding() {
            let img = self.guider.current_image().clone();
            self.log_auto_select_image(&img, found);
        }
        found
    }

    /// `guide_pulse` (event_server.cpp:2100).
    fn guide_pulse(&mut self, amount: i32, dir: Dir, which: Which) -> Reply {
        let role = match which {
            Which::Mount => self.scope_role(),
            Which::Ao => MountRole::Primary,
            Which::Both => return Err(CmdError::new(1, "invalid 'which' param")),
        };
        let connected = match which {
            Which::Mount => self.scope.as_ref().is_some_and(|s| s.scope.mount.is_connected()),
            _ => self.ao.as_ref().is_some_and(|a| a.sg.mount().is_connected()),
        };
        if !connected {
            return Err(CmdError::new(1, "device not connected"));
        }
        if self.guider.is_calibrating_or_guiding() || self.busy_role(role) {
            return Err(CmdError::new(1, "cannot issue guide pulse while calibrating or guiding"));
        }
        let (dir, amount) = if amount < 0 { (dir.opposite(), amount.wrapping_neg()) } else { (dir, amount) };
        let is_ao = which == Which::Ao;
        if let Some(line) = self.glog.manual_guide(is_ao, dir, amount) {
            self.events.push(EngineEvent::GuideLogLine(line));
        }
        self.cancel.reset();
        self.moves.push_back(Pending::Manual { role, dir, amount });
        Ok(json!(0))
    }

    /// `capture_single_frame` (event_server.cpp:1382) and
    /// `MyFrame::StartSingleExposure`. Fix: PHD2 goes on without a camera.
    fn capture_single_frame(&mut self, req: SingleFrame) -> Reply {
        if self.ctrl.capture_active() {
            return Err(CmdError::new(1, "cannot capture single frame when capture is currently active"));
        }
        if !self.camera_connected() {
            return Err(CmdError::new(1, "cannot capture single frame when camera is not connected"));
        }
        let camera = self.camera.as_ref().expect("camera");
        let max_binning = camera.max_binning() as i32;
        // Only the parameters sent are checked; the defaults are taken as
        // they are.
        if req.exposure_ms.is_some_and(|e| !(1..=600_000).contains(&e)) {
            return Err(CmdError::new(INVALID_PARAMS, "expected exposure param"));
        }
        let exposure = req.exposure_ms.unwrap_or(self.ctrl.requested_exposure_duration(true));
        if req.binning.is_some_and(|b| !(1..=max_binning).contains(&b)) {
            return Err(CmdError::new(INVALID_PARAMS, format!("invalid binning value: min=1, max={max_binning}")));
        }
        let binning = req.binning.unwrap_or(camera.binning() as i32);
        if req.gain.is_some_and(|g| !(0..=100).contains(&g)) {
            return Err(CmdError::new(INVALID_PARAMS, "invalid gain value: must be between 0 and 100"));
        }
        let gain = req.gain.unwrap_or(self.cam.gain);
        if let Some(path) = &req.path {
            if !path.starts_with('/') {
                return Err(CmdError::new(INVALID_PARAMS, "path param must be an absolute path"));
            }
            if self.files.exists(path) {
                return Err(CmdError::new(INVALID_PARAMS, "destination file already exists"));
            }
        }
        if !req.save && req.path.is_some() {
            return Err(CmdError::new(INVALID_PARAMS, "path param not allowed when save = false"));
        }
        self.debug(&format!(
            "StartSingleExposure duration={exposure} binning={binning} gain={gain} save={} path=[{}]",
            req.save as i32,
            req.path.clone().unwrap_or_default()
        ));
        self.status("Capturing single exposure");
        // `SingleExposure::Activate`. Quirk kept: the subframe is not clipped
        // to the frame (PHD2 intersects a temporary).
        let camera = self.camera.as_deref_mut().expect("camera");
        let prev_binning = camera.binning();
        let mut lines = Vec::new();
        let mut prof = self.store.profile();
        camera::set_binning(camera, binning, &mut prof, &mut lines);
        let prev_gain = self.cam.gain;
        let gain_ok = self.cam.set_gain(gain, &mut prof, &mut lines);
        drop(prof);
        for l in lines {
            self.debug(&l);
        }
        if !gain_ok {
            self.debug("StartSingleExposure: invalid exposure parameters");
            return Err(CmdError::new(2, "failed to start exposure"));
        }
        self.single = Some(SingleState { prev_binning, prev_gain, save: req.save, path: req.path });
        let single = SingleExposure { duration_ms: exposure, subframe: req.subframe };
        self.call(|c, cx, ev| c.start_single_exposure(single, cx, ev));
        Ok(json!(0))
    }

    /// `set_cooler_state` (event_server.cpp:2213).
    fn set_cooler_state(&mut self, enable: bool) -> Reply {
        if !self.camera_connected() {
            return Err(CmdError::new(1, "camera not connected"));
        }
        let camera = self.camera.as_deref_mut().expect("camera");
        if !camera.has_cooler() {
            return Err(CmdError::new(1, "camera lacks a cooler"));
        }
        if camera.set_cooler_on(enable) {
            return Err(CmdError::new(1, "failed to set cooler state"));
        }
        if enable {
            let setpoint = self.store.profile().get_f64("/camera/CoolerSetpt", 10.0);
            if camera.set_cooler_setpoint(setpoint) {
                return Err(CmdError::new(1, "failed to set cooler setpoint"));
            }
        }
        Ok(json!(0))
    }

    /// `save_image` (event_server.cpp:1358): the current frame into a new
    /// `save_image_*` file of the data folder.
    fn save_image(&mut self) -> Reply {
        let img = self.guider.current_image().clone();
        if img.data.is_empty() {
            return Err(CmdError::new(2, "no image available"));
        }
        let name = self.temp_name("save_image_");
        match self.save_fits(&img, &name) {
            Ok(()) => Ok(json!({ "filename": name })),
            Err(_) => {
                self.files.remove(&name);
                Err(CmdError::new(3, "error saving image"))
            }
        }
    }

    /// `set_calibration_data`: a calibration set on a mount and stored, as
    /// PHD2's manual calibration and the calibration restore do.
    fn set_calibration_data(&mut self, which: Which, cal: calibration::Calibration) -> Reply {
        let ts = wx_timestamp(&self.wall());
        let mut mev = Vec::new();
        let role = match which {
            Which::Mount => {
                let Some(s) = self.scope.as_mut() else { return Err(CmdError::new(1, "mount not defined")) };
                s.scope.set_calibration(&cal, &ts, &mut self.store.profile(), &mut mev);
                self.scope_role()
            }
            Which::Ao => {
                let Some(a) = self.ao.as_mut() else { return Err(CmdError::new(1, "AO not defined")) };
                a.sg.set_calibration(&cal, &ts, &mut self.store.profile(), &mut mev);
                MountRole::Primary
            }
            Which::Both => return Err(CmdError::new(INVALID_PARAMS, "expected param \"mount\" or \"ao\"")),
        };
        self.emit_mount_events(role, mev);
        self.refresh_ra_guide_speed();
        Ok(json!(0))
    }

    /// The role of `TheScope()`: pSecondaryMount behind an AO.
    fn scope_role(&self) -> MountRole {
        if self.ao.is_some() { MountRole::Secondary } else { MountRole::Primary }
    }

    /// pMount's `Mount` half.
    fn primary_mount_mut(&mut self) -> Option<&mut mount::Mount> {
        match (self.ao.as_mut(), self.scope.as_mut()) {
            (Some(a), _) => Some(a.sg.mount_mut()),
            (None, Some(s)) => Some(&mut s.scope.mount),
            (None, None) => None,
        }
    }

    /// pMount's `SetGuidingEnabled`; false without pMount.
    fn set_primary_guiding_enabled(&mut self, enable: bool) -> bool {
        let algo = self.algo_ctx();
        let mut mev = Vec::new();
        match (self.ao.as_mut(), self.scope.as_mut()) {
            (Some(a), _) => {
                a.sg.mount_mut().set_guiding_enabled(enable, &algo, &mut mev);
            }
            (None, Some(s)) => s.scope.set_guiding_enabled(enable, &algo, &mut mev),
            (None, None) => return false,
        }
        self.emit_mount_events(MountRole::Primary, mev);
        true
    }

    /// An algorithm setter's profile write on pMount.
    fn save_primary_algorithm(&mut self, axis: Axis) {
        let Engine { scope, ao, store, .. } = self;
        let mut prof = store.profile();
        match (ao.as_ref(), scope.as_ref()) {
            (Some(a), _) => a.sg.mount().save_algorithm(axis, &mut prof),
            (None, Some(s)) => s.scope.mount.save_algorithm(axis, &mut prof),
            (None, None) => {}
        }
    }

    /// The calibration's RA guide speed, read again (`LoadCalibrationDetails`
    /// without its guide-speed check).
    fn refresh_ra_guide_speed(&mut self) {
        let class = if self.ao.is_some() { algo::MountClass::StepGuider } else { algo::MountClass::Scope };
        self.ra_guide_speed = calibration::load_calibration_details(&self.store.profile(), class).ra_guide_speed;
    }

    // --------------------------------------------------------------- snapshot

    /// What the read-only RPCs read.
    pub fn snapshot(&self) -> Snapshot {
        let camera_connected = self.camera_connected();
        let device = |name: String, connected: bool| DeviceView { name, connected };
        let scope_cal = self.scope.as_ref().map(|s| {
            let m = &s.scope.mount;
            calibration_view(m, s.scope.is_calibrated(), (0, 0))
        });
        let ao_cal = self.ao.as_ref().map(|a| {
            calibration_view(a.sg.mount(), a.sg.is_calibrated(), a.sg.ao_max_pos(&*a.dev))
        });
        let primary = match (&self.ao, &self.scope) {
            (Some(a), _) => Some(a.sg.mount()),
            (None, Some(s)) => Some(&s.scope.mount),
            (None, None) => None,
        };
        let algo_view = |axis: Axis| {
            primary.map(|m| {
                let a = m.algo(axis);
                AlgoView {
                    class_name: a.class_name().to_string(),
                    params: a
                        .param_names()
                        .iter()
                        .filter_map(|n| a.get_param(n).map(|v| (n.to_string(), v)))
                        .collect(),
                }
            })
        };
        let primary_ok = match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.mount().is_connected(),
            (None, Some(s)) => s.scope.mount.is_connected(),
            (None, None) => true,
        };
        let secondary_ok = match (&self.ao, &self.scope) {
            (Some(_), Some(s)) => s.scope.mount.is_connected(),
            _ => true,
        };
        let primary_cal = match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.is_calibrated(),
            (None, Some(s)) => s.scope.is_calibrated(),
            (None, None) => false,
        };
        let secondary_cal = match (&self.ao, &self.scope) {
            (Some(_), Some(s)) => s.scope.is_calibrated(),
            _ => true,
        };
        let a = self.ctrl.auto_exposure();
        let v = self.ctrl.var_delay();
        let img = self.guider.current_image().clone();
        Snapshot {
            app_state: Some(self.guider.app_state(self.ctrl.capture_active())),
            guider_state: Some(self.guider.state()),
            capture_active: self.ctrl.capture_active(),
            connected: camera_connected && primary_ok && secondary_ok,
            camera: self.camera.as_ref().map(|c| device(c.name(), c.is_connected())),
            mount: self.scope.as_ref().map(|s| device(s.name.clone(), s.scope.mount.is_connected())),
            aux_mount: self.aux.as_ref().map(|(n, _)| device(n.clone(), true)),
            ao: self.ao.as_ref().map(|a| device(a.sg.mount().name().to_string(), a.sg.mount().is_connected())),
            rotator: self.rotator.as_ref().map(|r| device(r.name(), r.is_connected())),
            calibrated: primary.is_some() && primary_cal && secondary_cal,
            mount_calibration: scope_cal.unwrap_or_default(),
            ao_calibration: ao_cal.unwrap_or_default(),
            lock_position: self.guider.lock_position(),
            star_position: self.guider.current_position(),
            star: self.guider.current_position().map(|_| *self.guider.primary_star()),
            exposure_ms: self.ctrl.requested_exposure_duration(camera_connected),
            exposure_setting_ms: if a.enabled { -1 } else { self.ctrl.exposure_duration() },
            exposure_durations: self.ctrl.exposure_durations().to_vec(),
            auto_exposure: (a.min_ms, a.max_ms, a.target_snr),
            pixel_scale: self.pixel_scale(),
            dec_guide_mode: Some(self.scope.as_ref().map_or(DecGuideMode::None, |s| s.scope.dec_guide_mode())),
            paused: self.guider.pause_type(),
            settling: self.ctrl.is_settling(),
            dithering: self.ctrl.is_dithering(),
            lock_shift: self.guider.lock_shift_params(),
            lock_shift_rate: self.guider.lock_shift_rate(),
            frame_size: self.camera.as_ref().map_or((0, 0), |c| c.frame_size()),
            binning: self.binning(),
            max_binning: self.camera.as_ref().map_or(1, |c| c.max_binning()),
            use_subframes: self.cam.use_subframes,
            search_region: self.guider.max_move_pixels(),
            subframe: img.subframe.filter(|r| !r.is_empty()),
            image: (!img.data.is_empty()).then_some(img),
            algo_x: algo_view(Axis::X),
            algo_y: algo_view(Axis::Y),
            var_delay: (v.enabled, v.short_ms, v.long_ms),
            cooler: self.camera.as_ref().filter(|c| c.is_connected()).and_then(|c| c.cooler_status()),
            has_cooler: self.camera.as_ref().is_some_and(|c| c.has_cooler()),
            sensor_temperature: self.camera.as_ref().filter(|c| c.is_connected()).and_then(|c| c.sensor_temperature()),
            multistar: self.guider.multistar().cfg.multi_star,
            stars_used: self.guider.multistar().stars_used(),
            dither_mode: self.ctrl.dither_mode(),
            dither_ra_only: self.ctrl.dither_ra_only(),
            dither_scale: self.ctrl.dither_scale_factor(),
            settle: self.settle.filter(|_| self.ctrl.is_settling()),
            gain_supported: self.camera.as_ref().is_some_and(|c| c.has_gain_control()),
            dark_loaded: self.darks.current_dark().is_some(),
            dark_exp_ms: self.darks.current_dark().map(|d| d.img_exp_dur),
            defect_map_loaded: self.darks.defect_map.is_some(),
            profile_id: self.store.current_profile_id(),
            profile_name: self.store.current_profile(),
            frame: self.frame_counter,
            ga: self.ga_view(),
            darks_build: self.darks_view(),
            polar_drift: self.polar_drift_view(),
            connecting: self.connecting,
            port: None,
            port_error: None,
            engine_error: None,
            has_gain_control: self.camera.as_ref().is_some_and(|c| c.has_gain_control()),
            dark_exposures: self.darks.darks.keys().copied().collect(),
            guide_stars: self.guider.multistar().guide_stars().to_vec(),
        }
    }
}

/// One mount's calibration for the snapshot.
fn calibration_view(m: &mount::Mount, calibrated: bool, ao_max_pos: (i32, i32)) -> CalibrationView {
    let c = m.mount_cal();
    CalibrationView {
        present: true,
        name: m.name().to_string(),
        connected: m.is_connected(),
        calibrated,
        x_angle: c.x_angle,
        x_rate: c.x_rate,
        x_parity: c.ra_guide_parity,
        y_angle: c.y_angle,
        y_rate: c.y_rate,
        y_parity: c.dec_guide_parity,
        declination: c.declination,
        ao_max_pos,
        guiding_enabled: m.guiding_enabled(),
    }
}

/// `pPointingSource` (gear_dialog.cpp:1301): the scope when it reports its
/// position or there is no aux mount, else the aux mount; the manual
/// pointing when neither reports one.
fn pointing_source<'a>(
    scope: Option<&'a (dyn PointingSource + Send)>,
    aux: Option<&'a (dyn PointingSource + Send)>,
    manual: Option<&'a ManualPointing>,
) -> Option<&'a dyn PointingSource> {
    let reports = |p: &&(dyn PointingSource + Send)| p.can_report_position();
    if let Some(s) = scope.filter(reports) {
        return Some(s);
    }
    if let Some(a) = aux.filter(reports) {
        return Some(a);
    }
    if let Some(m) = manual {
        return Some(m);
    }
    match (scope, aux) {
        (Some(s), _) => Some(s),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// `wxDateTime::Now().Format()` in the C locale (`%c`), the time stamp a
/// calibration is stored with.
fn wx_timestamp(t: &DateTime<Local>) -> String {
    t.format("%a %b %e %H:%M:%S %Y").to_string()
}

/// A guide output that refuses every pulse: the "On Camera" mount without
/// a camera.
struct NoOutput;

impl GuideOutput for NoOutput {
    fn guide(&mut self, _dir: Dir, _ms: u32, _cancel: &CancelToken) -> MoveResult {
        MoveResult::Error
    }
}
