//! The guider: PHD2's `Guider` state machine, without its window. Each frame
//! it finds the star again (through [`MultiStar`]), keeps the error averages,
//! walks UNINITIALIZED → SELECTING → SELECTED → CALIBRATING_PRIMARY →
//! CALIBRATING_SECONDARY → CALIBRATED → GUIDING, and hands back the moves
//! PHD2 schedules (`SchedulePrimaryMove`, `ScheduleAxisMove`) for the engine
//! to make before the next exposure. It owns the lock position (moved by a
//! dither with the fast recenter after it, or shifted at a set rate for a
//! comet), the measurement mode of the Guiding Assistant's backlash test,
//! and the pause state (PHD2's `Guider::m_paused`, which `MyFrame::SetPaused`
//! wraps with the events and the exposure restart).
//!
//! The mounts come in through [`GuiderMount`] — the primary (`pMount`: the
//! AO when there is one, else the scope) and the secondary (`pSecondaryMount`:
//! the scope behind an AO) — and everything else PHD2 reads from `pFrame`,
//! `pCamera` and the pointing source through a [`GuiderCx`]. What PHD2 tells
//! the outside (events, guide-log lines, alerts, status and debug lines, the
//! mounts' own) comes back as [`GuiderEvent`]s, in PHD2's order. The
//! tracker's debug lines ([`MultiStar::take_log`]) are moved in right after
//! each call that writes them; the `ERROR_INFO` / `THROW_INFO` lines of
//! `GuiderMultiStar`'s throws, and its status messages, are the guider's.
//!
//! Ported from PHD2 src/guider.h, src/guider.cpp, src/guider_multistar.cpp (AutoSelect, SetMultiStarMode, SetLockPosition, the UpdateCurrentPosition throws and status) — BSD-3-Clause, see THIRD_PARTY.md

use std::sync::Arc;

use serde_json::Value;

use super::algo::AlgoCtx;
use super::calibration::{Adjustment, ProfileAccess, ScopePointing, degrees};
use super::image::UsImage;
use super::log::cfmt::{fmt_f, fmt_gp};
use super::log::image_log::{ErrorStats, ImageLogCall};
use super::mount::{AlertKind, GuiderOffset, Mount, MountEvent, PointingSource, stopwatch_seconds};
use super::multistar::{
    BoxState, FindParams, FrameErrorInfo, GuidingView, LossReason, MAX_SEARCH_REGION, MIN_SEARCH_REGION, MultiStar,
    MultiStarConfig, PositionLost, PositionUpdate, autoselect_edge_allowance, keys,
};
use super::scope::{CalibrationEnv, CalibrationNext, Scope};
use super::star::Star;
use super::traits::{Dir, MoveOpts, Parity, Point, Rect};

/// The profile keys the guider itself reads (the star settings, AutoSelect's
/// downsampling included, are [`multistar::keys`](super::multistar::keys)).
pub const FAST_RECENTER_KEY: &str = "/guider/FastRecenter";
/// Whether the frame is shown scaled to the window (`DefaultScaleImage` on).
pub const SCALE_IMAGE_KEY: &str = "/guider/ScaleImage";
/// The slit overlay and its defaults (`Guider::Guider`): centre, size, angle.
const SLIT_KEYS: [(&str, i32); 5] = [
    ("/overlay/slit/center.x", 752 / 2),
    ("/overlay/slit/center.y", 580 / 2),
    ("/overlay/slit/width", 8),
    ("/overlay/slit/height", 100),
    ("/overlay/slit/angle", 0),
];

/// `CurrentError` past this long without a star (`THRESHOLD_SECONDS`).
const STAR_LOST_SECONDS: i64 = 20;
/// What `CurrentError` reports then (`LARGE_DISTANCE`).
const LARGE_DISTANCE: f64 = 100.0;

/// PHD2's `GUIDER_STATE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GuiderState {
    Uninitialized = 0,
    Selecting,
    Selected,
    CalibratingPrimary,
    CalibratingSecondary,
    Calibrated,
    Guiding,
    /// The pseudo-state stopping asks for, mapped to a real one by
    /// `SetState`; never the current state.
    Stop,
}

impl GuiderState {
    /// `StateStr`, the debug log's name.
    pub fn name(self) -> &'static str {
        match self {
            GuiderState::Uninitialized => "UNINITIALIZED",
            GuiderState::Selecting => "SELECTING",
            GuiderState::Selected => "SELECTED",
            GuiderState::CalibratingPrimary => "CALIBRATING_PRIMARY",
            GuiderState::CalibratingSecondary => "CALIBRATING_SECONDARY",
            GuiderState::Calibrated => "CALIBRATED",
            GuiderState::Guiding => "GUIDING",
            GuiderState::Stop => "STOP",
        }
    }
}

/// PHD2's `PauseType`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PauseType {
    /// Not paused.
    #[default]
    None = 0,
    /// No guide corrections, exposures still looping.
    Guiding = 1,
    /// No corrections and no exposures.
    Full = 2,
}

/// PHD2's `EXPOSED_STATE`, what `get_app_state` and the `AppState` event
/// report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppState {
    Stopped = 0,
    Selected = 1,
    Calibrating = 2,
    Guiding = 3,
    LostLock = 4,
    Paused = 100,
    Looping = 101,
}

impl AppState {
    /// The wire string (`state_name`, event_server.cpp:60).
    pub fn as_str(self) -> &'static str {
        match self {
            AppState::Stopped => "Stopped",
            AppState::Selected => "Selected",
            AppState::Calibrating => "Calibrating",
            AppState::Guiding => "Guiding",
            AppState::LostLock => "LostLock",
            AppState::Paused => "Paused",
            AppState::Looping => "Looping",
        }
    }
}

/// PHD2's `GRAPH_UNITS`, the unit of a lock shift rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftUnits {
    Pixels = 0,
    Arcsec = 1,
}

/// PHD2's `LockPosShiftParams`: the lock shift as set (comet tracking).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LockShiftParams {
    pub enabled: bool,
    /// Per hour, in `units`.
    pub rate: Point,
    pub units: ShiftUnits,
    /// The rate is along RA and Dec rather than the camera's X and Y.
    pub mount_coords: bool,
}

impl Default for LockShiftParams {
    /// `Guider::Guider`'s: off, no rate, ″/hr along RA and Dec.
    fn default() -> Self {
        LockShiftParams { enabled: false, rate: Point::default(), units: ShiftUnits::Arcsec, mount_coords: true }
    }
}

/// Which mount: PHD2's `pMount` or `pSecondaryMount`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountRole {
    Primary,
    Secondary,
}

/// A move the guider schedules; the engine makes it before the next
/// exposure, then calls [`Guider::move_complete`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScheduledMove {
    /// `SchedulePrimaryMove(pMount, ofs, opts)`: `move_offset` on the
    /// primary mount.
    Offset { ofs: GuiderOffset, opts: MoveOpts },
    /// `ScheduleAxisMove(mount, dir, amount, MOVEOPTS_CALIBRATION_MOVE)`: a
    /// calibration step, `move_axis` on that mount (ms on a scope, steps on
    /// an AO).
    Calibration { role: MountRole, dir: Dir, amount: i32 },
}

/// What the polar tools are due after a frame. PHD2 calls them from inside
/// `UpdateGuideState`; nothing in the guider follows them, so the engine
/// runs them right after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaTools {
    #[default]
    None,
    /// SELECTED with the star found: `StaticPaTool::UpdateState` then
    /// `PolarDriftTool::UpdateState`. A failure goes to
    /// [`Guider::pa_tool_failed`].
    UpdateState,
    /// SELECTED and the star lost: `StaticPaTool::NotifyStarLost`.
    StarLost,
}

/// The polar tool whose `UpdateState` failed, for [`Guider::pa_tool_failed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaFailure {
    StaticPa,
    PolarDrift,
}

impl PaFailure {
    /// The status message and the line of its throw.
    fn message(self) -> (&'static str, u32) {
        match self {
            PaFailure::StaticPa => ("Static PA rotation failed", 1413),
            PaFailure::PolarDrift => ("Polar Drift PA drift failed", 1419),
        }
    }
}

/// What a call did besides its events: what PHD2 does there outside the
/// guider, left to the caller.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuideUpdate {
    /// The moves to make, in order.
    pub moves: Vec<ScheduledMove>,
    /// The star tracker's verdict, `None` when no position was looked for.
    /// It carries what PHD2 does inside `UpdateCurrentPosition` outside the
    /// guider: the image logger's distance or error. Its auto-exposure step
    /// comes as an event, in its place among the debug lines.
    pub position: Option<Result<PositionUpdate, PositionLost>>,
    pub pa_tools: PaTools,
    /// AutoFind set the camera's saturation level
    /// (`SetSaturationByADU(true, level)`), even when no star was found:
    /// the caller saves it to `/camera/SaturationADU`.
    pub saturation_adu: Option<u16>,
}

/// PHD2's `FrameDroppedInfo`: a frame whose star was not accepted, for the
/// `StarLost` event, the guide log's DROP row and the graphs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameDroppedInfo {
    /// `pImage->FrameNum`.
    pub frame_number: u32,
    /// `TimeSinceGuidingStarted`, s, cut to whole ms.
    pub time: f64,
    /// `CurrentGuideError`.
    pub avg_dist: f64,
    pub star: FrameErrorInfo,
}

/// What the guider tells the outside, in PHD2's order.
#[derive(Clone, Debug, PartialEq)]
pub enum GuiderEvent {
    /// What one of the mounts said while the guider drove it.
    Mount(MountRole, MountEvent),
    /// `AdjustCalibrationForScopePointing` ran on that mount when guiding
    /// started: its steps (debug lines, alerts, the status line, an
    /// image-scale change) and the new pixel size are the caller's; what it
    /// set is already stored.
    PointingAdjusted(MountRole, Adjustment),
    /// The `LoopingExposures` event: the star when found, else the drop.
    LoopingExposures { frame: u32, star: Option<Star>, dropped: Option<FrameErrorInfo> },
    /// `StarSelected`.
    StarSelected(Point),
    /// `StarLost`.
    StarLost(FrameDroppedInfo),
    /// A frame dropped while guiding: the guide log's DROP row, the Guiding
    /// Assistant's and the graph's notice.
    FrameDropped(FrameDroppedInfo),
    /// The guide log's `INFO: STAR LOST during calibration` line.
    CalibrationFrameDropped(FrameDroppedInfo),
    /// `LockPositionSet`.
    LockPositionSet(Point),
    /// `LockPositionLost`.
    LockPositionLost,
    /// `LockPositionShiftLimitReached`: the shifted lock left the frame's
    /// valid area, and the shift was turned off.
    LockShiftLimitReached,
    /// `GuidingAssistant::NotifyBacklashStep`: a guiding frame of the
    /// backlash measurement, the star there; no guide move was made.
    BacklashStep(Point),
    /// `GuidingAssistant::NotifyBacklashError`: guiding ended during the
    /// measurement, which ends it.
    BacklashError,
    /// A mount began calibrating: the guide log's calibration header and the
    /// `StartCalibration` event.
    StartCalibration(MountRole),
    /// The `CalibrationFailed` event of a calibration stopped by hand. Only
    /// the event: PHD2 writes nothing to the guide log here.
    CalibrationFailed { role: MountRole, reason: &'static str },
    /// The rest of `MyFrame::NotifyGuidingStarted`, after its "Guiding"
    /// status and the mounts' notices: the guiding clock and frame counter
    /// restarted, the guide log's guiding header, the `StartGuiding` event.
    GuidingStarted,
    /// `MyFrame::NotifyGuidingStopped` after the mounts' notices: the
    /// `GuidingStopped` event, the guide log's end line, the controller
    /// aborted ("Guiding stopped").
    GuidingStopped,
    /// `NotifyGuidingParam`: the `GuideParamChange` event and the guide
    /// log's parameter-change line, written even when not guiding if
    /// `force_log`.
    ParamChange { name: &'static str, value: Value, force_log: bool },
    /// `MyFrame::SetAutoLoadCalibration`: the frame's flag, saved to
    /// `/AutoLoadCalibration` when it changes.
    SetAutoLoadCalibration(bool),
    /// `pFrame->AdjustAutoExposure(snr)`: the star was found, with this
    /// SNR. [`Control::forward`](super::control::Control::forward) applies
    /// it, so its debug line lands here.
    AdjustAutoExposure(f64),
    /// `pFrame->ResetAutoExposure()`: the star was lost (not when none was
    /// selected). Applied by `Control::forward` too.
    ResetAutoExposure,
    /// `pFrame->Alert`. The event server's `Alert` carries `msg` and `kind`
    /// only; `button` is for the junos UI.
    Alert { msg: String, kind: AlertKind, button: Option<AlertButton> },
    /// `pFrame->StatusMsg`.
    Status(String),
    /// A guide-log `INFO:` line, without the prefix; the log writes it only
    /// between the start of a calibration or of guiding and its end.
    Info(String),
    /// A debug-log line.
    Debug(String),
    /// `ImageLogger::SaveImage(prev)`: a frame replaced the current image
    /// (`UpdateGuideState` with a frame, `DisplayImage`).
    ImageReplaced(PrevImage),
    /// One of `UpdateCurrentPosition`'s `ImageLogger::LogImage*` calls, on
    /// the current image, in its place among the debug lines.
    ImageLog(ImageLogCall),
}

/// The image the guider let go of. Two are equal when they are the same frame.
#[derive(Clone)]
pub struct PrevImage(pub Arc<UsImage>);

impl std::fmt::Debug for PrevImage {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "PrevImage(frame {})", self.0.frame_num)
    }
}

impl PartialEq for PrevImage {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// The action button of an alert (`pFrame->Alert`'s `buttonLabel` and
/// `SpecialFn`). Pressing it runs the action, which also dismisses the alert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertButton {
    /// `SetAutoLoad` (guider.cpp:1196): `MyFrame::SetAutoLoadCalibration(true)`,
    /// as [`GuiderEvent::SetAutoLoadCalibration`] does.
    AutoRestoreCalibration,
}

impl AlertButton {
    /// The button's label.
    pub fn label(self) -> &'static str {
        match self {
            AlertButton::AutoRestoreCalibration => "Auto-restore",
        }
    }
}

fn debug(ev: &mut Vec<GuiderEvent>, line: impl Into<String>) {
    ev.push(GuiderEvent::Debug(line.into()));
}

/// `ERROR_INFO` inside `Debug.Write`: the macro writes the line, and the
/// `Write` around it writes it again.
fn debug_twice(ev: &mut Vec<GuiderEvent>, line: &str) {
    debug(ev, line);
    debug(ev, line);
}

fn wrap(ev: &mut Vec<GuiderEvent>, role: MountRole, mev: Vec<MountEvent>) {
    ev.extend(mev.into_iter().map(|e| GuiderEvent::Mount(role, e)));
}

/// `UpdateImageDisplay`'s debug line. PHD2's gamma is the display stretch's.
fn image_display_line(img: &UsImage, gamma: f64) -> String {
    format!(
        "UpdateImageDisplay: Size=({},{}) min={}, max={}, med={}, FiltMin={}, FiltMax={}, Gamma={}",
        img.w,
        img.h,
        img.min_adu,
        img.max_adu,
        img.median_adu,
        img.filt_min,
        img.filt_max,
        fmt_f(gamma, 3)
    )
}

/// The virtual `Mount` interface the guider drives: the [`Mount`] half, and
/// what the scope and the AO each do their own way. The scope implements it
/// itself; the AO through [`AoMount`](super::stepguider::AoMount), which
/// lends it its device.
pub trait GuiderMount {
    fn mount(&self) -> &Mount;
    fn mount_mut(&mut self) -> &mut Mount;
    /// `IsCalibrated`: the scope also wants its Dec leg.
    fn is_calibrated(&self) -> bool;
    /// `CalibrationTotDistance`: the room a calibration needs around the
    /// star, px.
    fn calibration_tot_distance(&self) -> i32;
    /// `BeginCalibration` from the star at `location` (`None`: no star);
    /// true when it began.
    fn begin_calibration(
        &mut self,
        location: Option<Point>,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool;
    /// `UpdateCalibrationState` with the star at `cur`: the calibration
    /// moves to schedule (two at most, on an AO), or the failure.
    fn update_calibration_state(
        &mut self,
        cur: Point,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> CalibrationNext;
    /// `AdjustCalibrationForScopePointing`, when guiding starts. What it set
    /// is stored with `timestamp`.
    fn adjust_calibration_for_scope_pointing(
        &mut self,
        pointing: &ScopePointing,
        timestamp: &str,
        profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> Adjustment;
    /// `NotifyGuidingStopped`: the scope also resets its backlash
    /// compensation; the AO forgets its bump and steps back to the centre.
    fn notify_guiding_stopped(&mut self, ctx: &AlgoCtx, profile: &mut dyn ProfileAccess, events: &mut Vec<MountEvent>);
    /// `NotifyGuidingDithered`: the mount's, which the AO overrides.
    fn notify_guiding_dithered(
        &mut self,
        dx: f64,
        dy: f64,
        mount_coords: bool,
        ctx: &AlgoCtx,
        events: &mut Vec<MountEvent>,
    ) {
        self.mount_mut().notify_guiding_dithered(dx, dy, mount_coords, ctx, events);
    }
    /// `ClearCalibration`: no longer calibrated. The scope and the AO
    /// override it to send their calibration state machine back to the
    /// start, which a forced recalibration relies on.
    fn clear_calibration(&mut self) {
        self.mount_mut().clear_calibration();
    }
    /// `NotifyGuidingResumed`, when a pause ends: the mount's, which the AO
    /// overrides.
    fn notify_guiding_resumed(&mut self, ctx: &AlgoCtx, events: &mut Vec<MountEvent>) {
        self.mount_mut().notify_guiding_resumed(ctx, events);
    }
    /// `StepGuider::IsBumpInProgress`: a settle is not done while an AO
    /// bump runs. Never on a scope.
    fn is_bump_in_progress(&self) -> bool {
        false
    }
    /// The AO's part of `MyFrame::Dither`: with `BumpOnDither`, start a bump
    /// after the lock moved. Nothing on a scope.
    fn dither_bump(&mut self, _events: &mut Vec<MountEvent>) {}
    /// The scope behind the mount, for what PHD2 reads through `TheScope()`
    /// and `static_cast<Scope *>(pMount)` (the Dec guide mode); `None` on an
    /// AO.
    fn scope_mut(&mut self) -> Option<&mut Scope> {
        None
    }
}

impl GuiderMount for Scope {
    fn mount(&self) -> &Mount {
        &self.mount
    }

    fn mount_mut(&mut self) -> &mut Mount {
        &mut self.mount
    }

    fn is_calibrated(&self) -> bool {
        Scope::is_calibrated(self)
    }

    fn calibration_tot_distance(&self) -> i32 {
        self.calibration_distance()
    }

    fn begin_calibration(
        &mut self,
        location: Option<Point>,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        mut profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> bool {
        Scope::begin_calibration(self, location, env, pointing, &mut profile, events)
    }

    fn update_calibration_state(
        &mut self,
        cur: Point,
        env: &CalibrationEnv,
        pointing: Option<&dyn PointingSource>,
        mut profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> CalibrationNext {
        Scope::update_calibration_state(self, cur, env, pointing, &mut profile, events)
    }

    fn adjust_calibration_for_scope_pointing(
        &mut self,
        pointing: &ScopePointing,
        timestamp: &str,
        mut profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) -> Adjustment {
        Scope::adjust_calibration_for_scope_pointing(self, pointing, timestamp, &mut profile, events)
    }

    fn notify_guiding_stopped(
        &mut self,
        ctx: &AlgoCtx,
        mut profile: &mut dyn ProfileAccess,
        events: &mut Vec<MountEvent>,
    ) {
        Scope::notify_guiding_stopped(self, ctx, &mut profile, events)
    }

    fn clear_calibration(&mut self) {
        Scope::clear_calibration(self)
    }

    fn scope_mut(&mut self) -> Option<&mut Scope> {
        Some(self)
    }
}

/// What the guider reads from the rest of PHD2 on a call: the mounts, the
/// profile, the pointing source, and the frame's and the camera's settings.
pub struct GuiderCx<'a> {
    /// `pMount`: the AO when there is one, else the scope; `None` without a
    /// mount.
    pub primary: Option<&'a mut dyn GuiderMount>,
    /// `pSecondaryMount`: the scope behind an AO.
    pub secondary: Option<&'a mut dyn GuiderMount>,
    pub profile: &'a mut dyn ProfileAccess,
    /// `pPointingSource`.
    pub pointing: Option<&'a dyn PointingSource>,
    /// What a calibration reads (binning, optics, rotator, the guider's fast
    /// recenter and search region, the time stamp it stores).
    pub cal_env: &'a CalibrationEnv,
    /// Where the scope points, for the adjustment when guiding starts.
    pub scope_pointing: &'a ScopePointing,
    /// For the algorithms' notifications.
    pub algo: &'a AlgoCtx,
    /// What each `Star::Find` and `AutoFind` pass besides the star settings;
    /// AutoSelect's "Auto" downsampling reads `cal_env.image_scale`
    /// (`pFrame->GetCameraPixelScale()`).
    pub find: FindParams,
    /// `pCamera->UseSubframes`.
    pub use_subframes: bool,
    /// `MyFrame::GuidingRAOnly`: the scope's Dec guide mode is Off.
    pub ra_only: bool,
    /// `PhdController::IsSettling`.
    pub settling: bool,
    /// `MyFrame::GetExposureInfo`: the exposure, ms, and auto exposure.
    pub exposure_ms: i32,
    pub auto_exposure: bool,
    /// `TimeSinceGuidingStarted`, s, as the clock reads it: PHD2's
    /// `m_guidingElapsed` is a stopwatch running from the frame's creation
    /// and restarted only when guiding starts (myframe.cpp:2263), so before
    /// the first guiding it counts from startup, e.g. a `StarLost.Time`
    /// while looping.
    pub guide_time: f64,
    /// UTC milliseconds (`wxGetUTCTimeMillis`); `CurrentError` reads whole
    /// seconds of it (`wxDateTime::GetTimeNow`).
    pub now_ms: i64,
    /// `IsBusy` on either mount: a move scheduled (not manual) and not yet
    /// completed. The engine keeps the count.
    pub busy: bool,
    /// `pFrame->Stretch_gamma`, for the `UpdateImageDisplay` debug line.
    pub gamma: f64,
}

impl<'a> GuiderCx<'a> {
    /// A mount by role, next to the profile.
    fn split(&mut self, role: MountRole) -> (Option<&mut (dyn GuiderMount + 'a)>, &mut (dyn ProfileAccess + 'a)) {
        let m = match role {
            MountRole::Primary => self.primary.as_deref_mut(),
            MountRole::Secondary => self.secondary.as_deref_mut(),
        };
        (m, &mut *self.profile)
    }
}

/// `IsLoopingState`: looping, not calibrating or guiding.
fn is_looping_state(state: GuiderState) -> bool {
    matches!(state, GuiderState::Uninitialized | GuiderState::Selecting | GuiderState::Selected)
}

/// What a calibration frame came to for one mount.
enum CalStep {
    Done,
    Pending,
    Failed,
}

/// The lock position: PHD2's `ShiftPoint`, whose coordinates outlive an
/// invalidation and can drift at a set rate from where they were last set.
#[derive(Clone, Copy, Debug, Default)]
struct Lock {
    x: f64,
    y: f64,
    valid: bool,
    /// Where and when (UTC ms) the shift started.
    x0: f64,
    y0: f64,
    t0: i64,
    /// Camera px per second; `None` when not shifting.
    rate: Option<Point>,
}

impl Lock {
    fn point(&self) -> Point {
        Point::new(self.x, self.y)
    }

    /// `SetXY`: valid there, and any shift starts over from there.
    fn set_xy(&mut self, x: f64, y: f64, now_ms: i64) {
        (self.x, self.y, self.valid) = (x, y, true);
        self.begin_shift(now_ms);
    }

    /// `BeginShift`.
    fn begin_shift(&mut self, now_ms: i64) {
        if self.valid {
            (self.x0, self.y0, self.t0) = (self.x, self.y, now_ms);
        }
    }

    /// `SetShiftRate`.
    fn set_shift_rate(&mut self, rate: Point, now_ms: i64) {
        self.rate = Some(rate);
        self.begin_shift(now_ms);
    }

    /// `UpdateShift`: where the shift has taken it by now.
    fn update_shift(&mut self, now_ms: i64) {
        if let (true, Some(r)) = (self.valid, self.rate) {
            let dt = (now_ms - self.t0) as f64 / 1000.0;
            self.x = self.x0 + r.x * dt;
            self.y = self.y0 + r.y * dt;
        }
    }
}

/// The fast recenter after a lock move (`m_ditherRecenterRemaining`,
/// `m_ditherRecenterStep`, `m_ditherRecenterDir`): mount px still to go
/// on each axis, the most per frame, and the way to go.
#[derive(Clone, Copy, Debug)]
struct Recenter {
    remaining: Point,
    step: Point,
    dir: (i32, i32),
}

/// `edgeDist`: the distance to the nearest frame edge.
fn edge_dist(pt: Point, w: f64, h: f64) -> f64 {
    let min = |a: f64, b: f64| if a < b { a } else { b }; // wxMin
    min(pt.x, min(w - pt.x, min(pt.y, h - pt.y)))
}

/// PHD2's `Guider` with `GuiderMultiStar`'s star handling.
pub struct Guider {
    state: GuiderState,
    paused: PauseType,
    lock: Lock,
    /// `m_starFoundTimestamp`, whole seconds; `None` before the first star.
    star_found_at: Option<i64>,
    avg_distance: f64,
    avg_distance_ra: f64,
    avg_distance_long: f64,
    avg_distance_long_ra: f64,
    avg_distance_cnt: u32,
    avg_distance_need_reset: bool,
    lock_pos_is_sticky: bool,
    ignore_lost_star_looping: bool,
    fast_recenter_enabled: bool,
    /// `None` when not recentering (`m_ditherRecenterRemaining` invalid).
    recenter: Option<Recenter>,
    lock_shift: LockShiftParams,
    /// `m_userMultiStarMode`: the multi-star mode the lock shift put aside.
    user_multi_star: bool,
    /// `m_measurementMode`: guiding frames go to the Guiding Assistant.
    measurement_mode: bool,
    force_full_frame: bool,
    /// `m_pCurrentImage`: the last frame, an empty one at first.
    current_image: Arc<UsImage>,
    multistar: MultiStar,
}

impl Guider {
    /// `Guider::Guider` and `GuiderMultiStar::GuiderMultiStar`, which leave
    /// it SELECTING (they set UNINITIALIZED, which moves on at once), then
    /// the profile settings as [`Guider::load_profile_settings`] reads them.
    /// `sticky_lock` is the global `/StickyLockPosition`. The events go to a
    /// guide log already open, as PHD2 opens it before the guider: their
    /// forced `MultiStar` parameter change is its second line, and keeps the
    /// log file (guidinglog.cpp:736-741).
    pub fn new(profile: &mut dyn ProfileAccess, sticky_lock: bool, ev: &mut Vec<GuiderEvent>) -> Guider {
        debug(ev, "guider state => UNINITIALIZED");
        // `SetOverlaySlitCoords` writes the slit overlay back, so a profile
        // export has its keys; the overlay itself is the UI's.
        for (key, default) in SLIT_KEYS {
            let v = profile.get_i32(key, default);
            profile.set_i32(key, v);
        }
        debug(ev, "Changing from state UNINITIALIZED to UNINITIALIZED");
        debug(ev, "guider state => SELECTING");
        let mut g = Guider {
            state: GuiderState::Selecting,
            paused: PauseType::None,
            lock: Lock::default(),
            star_found_at: None,
            avg_distance: 0.0,
            avg_distance_ra: 0.0,
            avg_distance_long: 0.0,
            avg_distance_long_ra: 0.0,
            avg_distance_cnt: 0,
            avg_distance_need_reset: false,
            lock_pos_is_sticky: sticky_lock,
            ignore_lost_star_looping: false,
            fast_recenter_enabled: true,
            recenter: None,
            lock_shift: LockShiftParams::default(),
            // Read in `Guider::Guider`, where the virtual call still answers
            // the base class' false.
            user_multi_star: false,
            measurement_mode: false,
            force_full_frame: false,
            current_image: Arc::new(UsImage::default()),
            multistar: MultiStar::new(MultiStarConfig::default()),
        };
        // No image yet, so no display to clear (the gamma is unused), and
        // SELECTING, so the multi-star mode never re-selects here.
        let on = g.load_settings(profile, 0.0, ev);
        g.multistar.set_multi_star_mode(on);
        announce_multi_star(on, false, profile, ev);
        g
    }

    /// `Guider::LoadProfileSettings` and `GuiderMultiStar::LoadProfileSettings`
    /// (a profile switch): the settings, their effective values written
    /// back, the display cleared (when not calibrating or guiding), then the
    /// multi-star mode set as [`Guider::set_multi_star_mode`] does, whose
    /// update is returned. Its re-selection finds no image after the clear,
    /// unless a calibration or guiding was running (`DisplayImage` refuses
    /// then): on that frame it can set a saturation level and, after a
    /// calibration (stopped back to SELECTING), run the frame again. The
    /// display scaling (`/guider/ScaleImage`, written back) and the
    /// bookmarks are the UI's.
    pub fn load_profile_settings(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> GuideUpdate {
        let on = self.load_settings(cx.profile, cx.gamma, ev);
        self.set_multi_star_mode(on, cx, ev)
    }

    /// The settings part of `LoadProfileSettings`; the multi-star flag read.
    fn load_settings(&mut self, profile: &mut dyn ProfileAccess, gamma: f64, ev: &mut Vec<GuiderEvent>) -> bool {
        let fast = profile.get_bool(FAST_RECENTER_KEY, true);
        self.enable_fast_recenter(fast, profile);
        // `SetScaleImage` writes it back; the display scaling is the UI's.
        let scale = profile.get_bool(SCALE_IMAGE_KEY, true);
        profile.set_bool(SCALE_IMAGE_KEY, scale);

        let (cfg, raw_threshold, raw_region) = {
            let p = &*profile;
            let get = |k: &str| p.has(k).then(|| p.get_f64(k, 0.0));
            (MultiStarConfig::from_profile(&get), get(keys::MASS_CHANGE_THRESHOLD), get(keys::SEARCH_REGION))
        };
        debug(ev, format!("Setting StarMinHFD = {}", fmt_f(cfg.min_star_hfd, 2)));
        debug(ev, format!("Setting MaxHFD = {}", fmt_f(cfg.max_star_hfd, 1)));
        debug(ev, format!("Setting StarMinSNR = {}", fmt_f(cfg.min_star_snr, 1)));
        debug(ev, format!("Setting AutoSelDownsample = {}", cfg.auto_sel_downsample));
        if !self.current_image.data.is_empty() {
            self.display_image(Arc::new(UsImage::default()), gamma, ev);
        }

        // The setters' refusals, which MultiStarConfig already applied.
        if raw_threshold.is_some_and(|v| v < 0.0) {
            debug(ev, "Error thrown from guider_multistar.cpp:304->massChangeThreshold < 0");
        }
        match raw_region.map(|v| v as i32) {
            Some(v) if v < MIN_SEARCH_REGION => {
                debug(ev, "Error thrown from guider_multistar.cpp:342->searchRegion < MIN_SEARCH_REGION");
            }
            Some(v) if v > MAX_SEARCH_REGION => {
                debug(ev, "Error thrown from guider_multistar.cpp:347->searchRegion > MAX_SEARCH_REGION");
            }
            _ => {}
        }
        for (key, value) in cfg.profile_values() {
            match key {
                keys::SEARCH_REGION | keys::AUTO_SEL_DOWNSAMPLE => profile.set_i32(key, value as i32),
                keys::MASS_CHANGE_THRESHOLD_ENABLED | keys::TOLERATE_JUMPS_ENABLED | keys::MULTISTAR_ENABLED => {
                    profile.set_bool(key, value != 0.0)
                }
                _ => profile.set_f64(key, value),
            }
        }
        self.multistar.cfg = MultiStarConfig { multi_star: self.multistar.cfg.multi_star, ..cfg };
        cfg.multi_star
    }

    // ---------------------------------------------------------------- state

    /// `GetState`.
    pub fn state(&self) -> GuiderState {
        self.state
    }

    /// `IsGuiding`.
    pub fn is_guiding(&self) -> bool {
        self.state == GuiderState::Guiding
    }

    /// `IsCalibrating`.
    pub fn is_calibrating(&self) -> bool {
        self.state >= GuiderState::CalibratingPrimary && self.state < GuiderState::Calibrated
    }

    /// `IsCalibratingOrGuiding`.
    pub fn is_calibrating_or_guiding(&self) -> bool {
        self.state >= GuiderState::CalibratingPrimary && self.state <= GuiderState::Guiding
    }

    /// `GetPauseType`.
    pub fn pause_type(&self) -> PauseType {
        self.paused
    }

    /// `IsPaused`.
    pub fn is_paused(&self) -> bool {
        self.paused != PauseType::None
    }

    /// `Guider::SetPaused`: the previous pause. Leaving a full pause resets
    /// the error averages on the next star. The `Paused` / `Resumed` events,
    /// the guide-log lines and the algorithms' notice are `MyFrame`'s.
    pub fn set_paused(&mut self, pause: PauseType, ev: &mut Vec<GuiderEvent>) -> PauseType {
        debug(ev, format!("Guider::SetPaused({})", pause as i32));
        let prev = self.paused;
        self.paused = pause;
        if prev == PauseType::Full && pause != prev {
            debug(ev, "Guider::SetPaused: resetting avg dist filter");
            self.avg_distance_need_reset = true;
        }
        prev
    }

    /// `GetExposedState`: the app state clients see; the pause wins over the
    /// capture state. PHD2 writes `case statement mapped state <state> to
    /// <app state>` (the enums' integers) to the debug log when capture is
    /// active; that is the caller's, as this is read off the engine thread.
    pub fn app_state(&self, capture_active: bool) -> AppState {
        if self.is_paused() {
            return AppState::Paused;
        }
        if !capture_active {
            return AppState::Stopped;
        }
        match self.state {
            GuiderState::Uninitialized | GuiderState::Stop => AppState::Stopped,
            // "Looping" only with no star selected.
            GuiderState::Selecting if self.multistar.primary().is_valid() => AppState::Selected,
            GuiderState::Selecting => AppState::Looping,
            GuiderState::Selected | GuiderState::Calibrated => AppState::Selected,
            GuiderState::CalibratingPrimary | GuiderState::CalibratingSecondary => AppState::Calibrating,
            GuiderState::Guiding if self.multistar.is_locked() => AppState::Guiding,
            GuiderState::Guiding => AppState::LostLock,
        }
    }

    /// `SetState` (guider.cpp:935). `Stop` maps to where looping can start
    /// again: UNINITIALIZED after a calibration (the mount moved), SELECTED
    /// after guiding. A jump of more than one state up is refused.
    /// UNINITIALIZED drops the lock and the star and moves on to SELECTING;
    /// CALIBRATING_PRIMARY begins the primary's calibration unless it is
    /// calibrated, then the secondary's, and ends at CALIBRATED when there
    /// is no secondary to calibrate (the AO short-cut); GUIDING drops the
    /// fast recenter, adjusts both calibrations to the scope's pointing and
    /// locks on the star, unless a sticky lock is kept. When a step lands
    /// below the state asked for, the lower state is set the same way.
    fn set_state(&mut self, new: GuiderState, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        use GuiderState::*;
        debug(ev, format!("Changing from state {} to {}", self.state.name(), new.name()));
        let mut new = if new == Stop {
            match self.state {
                Uninitialized | Selecting | Selected | Stop => self.state,
                CalibratingPrimary | CalibratingSecondary => Uninitialized,
                Calibrated | Guiding => Selected,
            }
        } else {
            new
        };
        if new as i32 > self.state as i32 + 1 {
            debug(ev, format!("Cannot transition from {} to newState={}", self.state.name(), new.name()));
            debug(ev, "Error thrown from guider.cpp:972->Illegal state transition");
            return;
        }

        let requested = new;
        match requested {
            Uninitialized => {
                self.invalidate_lock_position(ev);
                self.multistar.invalidate_current_position(false);
                new = Selecting;
            }
            CalibratingPrimary | CalibratingSecondary => {
                let mut secondary_due = requested == CalibratingSecondary;
                if requested == CalibratingPrimary {
                    if cx.primary.as_deref().is_some_and(|m| m.is_calibrated()) {
                        secondary_due = true;
                    } else if !self.begin_mount_calibration(MountRole::Primary, cx, ev) {
                        new = Uninitialized;
                    }
                }
                if secondary_due {
                    // Quirk kept: asked for CALIBRATING_PRIMARY with the
                    // primary calibrated, this begins the secondary's
                    // calibration but the state stays CALIBRATING_PRIMARY;
                    // the next frame's CALIBRATING_SECONDARY begins it
                    // again, so `StartCalibration` and the guide log's
                    // calibration header come twice.
                    match cx.secondary.as_deref().map(|m| (m.mount().is_connected(), m.is_calibrated())) {
                        None | Some((false, _)) => new = Calibrated,
                        Some((true, false)) => {
                            if !self.begin_mount_calibration(MountRole::Secondary, cx, ev) {
                                new = Uninitialized;
                            }
                        }
                        Some((true, true)) => {}
                    }
                }
            }
            Guiding => {
                self.recenter = None;
                let (sp, ts) = (cx.scope_pointing, &cx.cal_env.timestamp);
                for role in [MountRole::Primary, MountRole::Secondary] {
                    if let (Some(m), profile) = cx.split(role) {
                        let mut mev = Vec::new();
                        let adj = m.adjust_calibration_for_scope_pointing(sp, ts, profile, &mut mev);
                        wrap(ev, role, mev);
                        ev.push(GuiderEvent::PointingAdjusted(role, adj));
                    }
                }
                if self.lock.valid && self.lock_pos_is_sticky {
                    debug(ev, "keeping sticky lock position");
                } else {
                    match self.current_position() {
                        Some(p) => {
                            self.set_lock_position(p, cx, ev);
                        }
                        None => debug(ev, "Error thrown from guider.cpp:780->Point is not valid"),
                    }
                }
            }
            Selecting | Selected | Calibrated | Stop => {}
        }

        if new >= requested {
            self.state = new;
            debug(ev, format!("guider state => {}", self.state.name()));
        } else {
            debug(ev, format!("SetState recurses newState {}", new.name()));
            self.set_state(new, cx, ev);
        }
    }

    /// `ResetErrorCount` then `BeginCalibration` on a mount, from the star;
    /// the guide-log header and `StartCalibration` when it began. Without a
    /// primary mount (PHD2 would dereference null; its controller never asks)
    /// the calibration fails.
    fn begin_mount_calibration(&mut self, role: MountRole, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> bool {
        let location = self.current_position();
        let (env, pointing) = (cx.cal_env, cx.pointing);
        let began = match cx.split(role) {
            (Some(m), profile) => {
                m.mount_mut().reset_error_count();
                let mut mev = Vec::new();
                let began = m.begin_calibration(location, env, pointing, profile, &mut mev);
                wrap(ev, role, mev);
                began
            }
            (None, _) => false,
        };
        if began {
            ev.push(GuiderEvent::StartCalibration(role));
        } else {
            debug_twice(
                ev,
                match role {
                    MountRole::Primary => "Error thrown from guider.cpp:993->pMount->BeginCalibration failed",
                    MountRole::Secondary => {
                        "Error thrown from guider.cpp:1014->pSecondaryMount->BeginCalibration failed"
                    }
                },
            );
        }
        began
    }

    /// One calibration frame on a mount not yet calibrated:
    /// `UpdateCalibrationState`, its moves scheduled in order.
    fn calibration_step(
        &mut self,
        role: MountRole,
        out: &mut GuideUpdate,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) -> CalStep {
        let cur = self.multistar.primary().position();
        let (env, pointing) = (cx.cal_env, cx.pointing);
        let (Some(m), profile) = cx.split(role) else {
            return CalStep::Done;
        };
        if m.is_calibrated() {
            return CalStep::Done;
        }
        let mut mev = Vec::new();
        let next = m.update_calibration_state(cur, env, pointing, profile, &mut mev);
        let calibrated = m.is_calibrated();
        wrap(ev, role, mev);
        let CalibrationNext::Moves(moves) = next else {
            return CalStep::Failed;
        };
        for (dir, amount) in moves {
            schedule(out, ScheduledMove::Calibration { role, dir, amount }, cx, ev);
        }
        if calibrated { CalStep::Done } else { CalStep::Pending }
    }

    /// `StartGuiding`: ask for CALIBRATING_PRIMARY; the frames move it on to
    /// GUIDING (from SELECTED only, the jump rule refuses it otherwise).
    pub fn start_guiding(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        self.set_state(GuiderState::CalibratingPrimary, cx, ev);
    }

    /// `StopGuiding`: a calibration in progress (or just done) is reported
    /// failed, "Calibration manually stopped"; guiding stopped is announced
    /// at once unless a move is still out ([`GuiderCx::busy`]), then by
    /// [`Guider::move_complete`], so the last `GuideStep` comes first. Then
    /// the `Stop` state.
    pub fn stop_guiding(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        use GuiderState::*;
        match self.state {
            CalibratingPrimary | CalibratingSecondary | Calibrated | Guiding => {
                if self.state != Guiding {
                    let role =
                        if self.state == CalibratingSecondary { MountRole::Secondary } else { MountRole::Primary };
                    ev.push(GuiderEvent::CalibrationFailed { role, reason: "Calibration manually stopped" });
                }
                if !cx.busy {
                    self.notify_guiding_stopped(cx, ev);
                }
            }
            Uninitialized | Selecting | Selected | Stop => {}
        }
        self.set_state(Stop, cx, ev);
    }

    /// The guider's part of `OnMoveComplete` for a move that was not manual,
    /// called once the engine has counted it done (and after the mount's
    /// step was logged): out of calibration and guiding with no move left,
    /// the `GuidingStopped` that [`Guider::stop_guiding`] held back.
    /// Quirk kept: PHD2 sends it after any such late move, held back or not.
    pub fn move_complete(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        if !self.is_calibrating_or_guiding() && !cx.busy {
            self.notify_guiding_stopped(cx, ev);
        }
    }

    /// `Reset`: back to looping without a star; a full reset also forgets
    /// where the star was, so the next frame does not look for it.
    pub fn reset(&mut self, full_reset: bool, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        self.set_state(GuiderState::Uninitialized, cx, ev);
        if full_reset {
            self.multistar.invalidate_current_position(true);
        }
    }

    /// A polar tool's `UpdateState` failed on a SELECTED frame
    /// ([`PaTools::UpdateState`]): back to looping, with its message. PHD2
    /// does this inside `UpdateGuideState`, so its `UpdateGuideState exits`
    /// debug line reads the message; here that line was written with the
    /// star's status before.
    pub fn pa_tool_failed(&mut self, failure: PaFailure, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        self.set_state(GuiderState::Uninitialized, cx, ev);
        let (msg, line) = failure.message();
        debug(ev, format!("Error thrown from guider.cpp:{line}->{msg}"));
        ev.push(GuiderEvent::Status(msg.into()));
    }

    /// `MyFrame::NotifyGuidingStarted`: the "Guiding" status, both mounts'
    /// algorithms, then the rest of the frame's part.
    fn notify_guiding_started(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        ev.push(GuiderEvent::Status("Guiding".into()));
        let algo = cx.algo;
        for role in [MountRole::Primary, MountRole::Secondary] {
            if let (Some(m), _) = cx.split(role) {
                let mut mev = Vec::new();
                m.mount_mut().notify_guiding_started(algo, &mut mev);
                wrap(ev, role, mev);
            }
        }
        ev.push(GuiderEvent::GuidingStarted);
    }

    /// `MyFrame::NotifyGuidingStopped`: both mounts, then the frame's part.
    fn notify_guiding_stopped(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        let algo = cx.algo;
        for role in [MountRole::Primary, MountRole::Secondary] {
            if let (Some(m), profile) = cx.split(role) {
                let mut mev = Vec::new();
                m.notify_guiding_stopped(algo, profile, &mut mev);
                wrap(ev, role, mev);
            }
        }
        ev.push(GuiderEvent::GuidingStopped);
    }

    // ------------------------------------------------------------- settings

    /// `SetLockPosIsSticky`: keep the lock position when guiding starts.
    pub fn set_lock_pos_is_sticky(&mut self, sticky: bool) {
        self.lock_pos_is_sticky = sticky;
    }

    pub fn lock_pos_is_sticky(&self) -> bool {
        self.lock_pos_is_sticky
    }

    /// `SetIgnoreLostStarLooping`: losing the star while SELECTED keeps the
    /// selection (the drift tool's choice).
    pub fn set_ignore_lost_star_looping(&mut self, ignore: bool, ev: &mut Vec<GuiderEvent>) {
        if self.ignore_lost_star_looping != ignore {
            debug(ev, format!("setting ignore lost star looping = {ignore}"));
            self.ignore_lost_star_looping = ignore;
        }
    }

    /// `IsRecentering`: a lock move is still being caught up with.
    pub fn is_recentering(&self) -> bool {
        self.recenter.is_some()
    }

    /// `IsFastRecenterEnabled`.
    pub fn is_fast_recenter_enabled(&self) -> bool {
        self.fast_recenter_enabled
    }

    /// `EnableFastRecenter`, saved to `/guider/FastRecenter`.
    pub fn enable_fast_recenter(&mut self, enable: bool, profile: &mut dyn ProfileAccess) {
        self.fast_recenter_enabled = enable;
        profile.set_i32(FAST_RECENTER_KEY, enable as i32);
    }

    /// `GetAutoSelDownsample`, 0 for Auto.
    pub fn auto_sel_downsample(&self) -> u32 {
        self.multistar.cfg.auto_sel_downsample
    }

    /// `ForceFullFrame`: no subframe until a star is found again.
    pub fn force_full_frame(&mut self, ev: &mut Vec<GuiderEvent>) {
        if !self.force_full_frame {
            debug(ev, "setting force full frames = true");
            self.force_full_frame = true;
        }
    }

    /// `GetMaxMovePixels`: the search region.
    pub fn max_move_pixels(&self) -> i32 {
        self.multistar.cfg.search_region
    }

    /// The star tracker.
    pub fn multistar(&self) -> &MultiStar {
        &self.multistar
    }

    pub fn multistar_mut(&mut self) -> &mut MultiStar {
        &mut self.multistar
    }

    /// `GuiderMultiStar::SetMultiStarMode`. Turning it on from SELECTED on
    /// stops guiding and auto-selects again so the secondaries are found,
    /// restarting a calibration or guiding that was running when that
    /// succeeds. Saved, logged and announced (`MultiStar`, a string,
    /// logged even when not guiding). The re-selection's update is returned.
    pub fn set_multi_star_mode(&mut self, on: bool, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> GuideUpdate {
        let mut forced = false;
        let mut out = GuideUpdate::default();
        if self.multistar.set_multi_star_mode(on) && self.state >= GuiderState::Selected {
            let active = self.is_calibrating_or_guiding();
            self.stop_guiding(cx, ev);
            self.multistar.invalidate_current_position(true);
            let (star, upd) = self.auto_select(None, cx, ev);
            out = upd;
            if star.is_some() {
                if active {
                    self.start_guiding(cx, ev);
                }
                forced = true;
            }
        }
        announce_multi_star(on, forced, cx.profile, ev);
        out
    }

    // ------------------------------------------------------- star and lock

    /// `CurrentPosition`: the star, `None` when invalid.
    pub fn current_position(&self) -> Option<Point> {
        let star = self.multistar.primary();
        star.is_valid().then(|| star.position())
    }

    /// `PrimaryStar`.
    pub fn primary_star(&self) -> &Star {
        self.multistar.primary()
    }

    /// `LockPosition`, `None` when invalid.
    pub fn lock_position(&self) -> Option<Point> {
        self.lock.valid.then(|| self.lock.point())
    }

    /// `CurrentImage`.
    pub fn current_image(&self) -> &Arc<UsImage> {
        &self.current_image
    }

    /// `DisplayImage`: a frame shown while not calibrating or guiding (a
    /// single exposure) becomes the current image, the previous one going
    /// to the image logger.
    pub fn display_image(&mut self, img: Arc<UsImage>, gamma: f64, ev: &mut Vec<GuiderEvent>) {
        if self.is_calibrating_or_guiding() {
            return;
        }
        let prev = std::mem::replace(&mut self.current_image, img);
        ev.push(GuiderEvent::ImageReplaced(PrevImage(prev)));
        debug(ev, image_display_line(&self.current_image, gamma));
    }

    /// `IsValidLockPosition`: far enough inside the current frame.
    pub fn is_valid_lock_position(&self, pt: Point) -> bool {
        self.multistar.is_valid_lock_position(pt, &self.current_image)
    }

    /// `GetBoundingBox` on a camera of `full_size`: the subframe to ask for,
    /// empty for the full frame.
    pub fn bounding_box(&self, full_size: (i32, i32)) -> Rect {
        let state = match self.state {
            GuiderState::Selected | GuiderState::CalibratingPrimary | GuiderState::CalibratingSecondary => {
                BoxState::Tracking
            }
            GuiderState::Guiding => BoxState::Guiding(self.lock.point()),
            _ => BoxState::Other,
        };
        self.multistar.bounding_box(state, self.force_full_frame, full_size)
    }

    /// `InvalidateLockPosition`: `LockPositionLost` when there was one.
    fn invalidate_lock_position(&mut self, ev: &mut Vec<GuiderEvent>) {
        if self.lock.valid {
            ev.push(GuiderEvent::LockPositionLost);
        }
        self.lock.valid = false;
    }

    /// `GuiderMultiStar::SetLockPosition` (`Guider::SetLockPosition` and the
    /// multi-star stabilisation): true when set. A position off the current
    /// frame is refused. A new position is announced (`LockPositionSet`);
    /// while guiding the primary's algorithms are told it moved, as a
    /// dither, and the guide log notes it.
    pub fn set_lock_position(&mut self, pos: Point, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> bool {
        debug(ev, format!("setting lock position to ({}, {})", fmt_f(pos.x, 2), fmt_f(pos.y, 2)));
        // Written as PHD2's tests, so a NaN gets through as it does there.
        if pos.x < 0.0 || pos.x >= self.current_image.w as f64 {
            debug(ev, "Error thrown from guider.cpp:789->invalid x value");
            return false;
        }
        if pos.y < 0.0 || pos.y >= self.current_image.h as f64 {
            debug(ev, "Error thrown from guider.cpp:794->invalid y value");
            return false;
        }
        if !self.lock.valid || pos.x != self.lock.x || pos.y != self.lock.y {
            ev.push(GuiderEvent::LockPositionSet(pos));
            if self.state == GuiderState::Guiding {
                let algo = cx.algo;
                if let Some(m) = cx.primary.as_deref_mut() {
                    let mut mev = Vec::new();
                    m.notify_guiding_dithered(pos.x - self.lock.x, pos.y - self.lock.y, false, algo, &mut mev);
                    wrap(ev, MountRole::Primary, mev);
                }
                // Quirk kept: the guide log reads the lock before it moves,
                // so its "new lock pos" is the old one.
                ev.push(GuiderEvent::Info(format!(
                    "SET LOCK POSITION, new lock pos = {}, {}",
                    fmt_f(self.lock.x, 3),
                    fmt_f(self.lock.y, 3)
                )));
            }
        }
        self.lock.set_xy(pos.x, pos.y, cx.now_ms);
        self.multistar.lock_position_set();
        self.drain_multistar_log(ev);
        true
    }

    /// `SetLockPosToStarAtPosition`: find the star near `hint` on the
    /// current frame (`SetCurrentPosition`) and lock on it. True when a star
    /// was found (PHD2 returns the error).
    pub fn set_lock_pos_to_star_at_position(
        &mut self,
        hint: Point,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) -> bool {
        let found = self.multistar.set_current_position(&self.current_image, hint, &cx.find);
        self.drain_multistar_log(ev);
        // The tracker refuses a hint off the frame without saying which axis.
        let (w, h) = (self.current_image.w as f64, self.current_image.h as f64);
        if hint.x <= 0.0 || hint.x >= w {
            debug(ev, "Error thrown from guider_multistar.cpp:380->invalid x value");
            return false;
        }
        if hint.y <= 0.0 || hint.y >= h {
            debug(ev, "Error thrown from guider_multistar.cpp:385->invalid y value");
            return false;
        }
        if found && let Some(p) = self.current_position() {
            self.set_lock_position(p, cx, ev);
        }
        found
    }

    /// `MoveLockPosition` (guider.cpp:838): move the lock by `mount_delta`
    /// (mount axes, px), as a dither does. Near the frame edge the move is
    /// reflected in RA, Dec or both — the first of the four that leaves a
    /// valid lock position, else the one farthest from the edge. The error
    /// averages grow by the move at once. With fast recenter on and a move
    /// of some length, the next guiding frames move the mount there
    /// themselves, in steps of 0.7 search regions that bypass the
    /// algorithms. False when it failed: no calibrated mount, or the lock
    /// refused.
    pub fn move_lock_position(&mut self, mount_delta: Point, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> bool {
        let Some(m) = cx.primary.as_deref().filter(|m| m.is_calibrated()) else {
            debug(ev, "Error thrown from guider.cpp:851->No mount");
            return false;
        };
        let lock = self.lock.point();
        let (w, h) = (self.current_image.w as f64, self.current_image.h as f64);
        let (mut camera_delta, mut mount_used, mut best) = (Point::default(), Point::default(), 0.0);
        for q in 0..4 {
            // (+, +), (−, +), (+, −), (−, −)
            let sx = (1 - ((q & 1) << 1)) as f64;
            let sy = (1 - (q & 2)) as f64;
            let tmp_mount = Point::new(mount_delta.x * sx, mount_delta.y * sy);
            let mut mev = Vec::new();
            let tmp_camera = m.mount().mount_to_camera(tmp_mount, true, &mut mev);
            wrap(ev, MountRole::Primary, mev);
            let tmp_lock = lock + tmp_camera;
            if self.is_valid_lock_position(tmp_lock) {
                (camera_delta, mount_used) = (tmp_camera, tmp_mount);
                break;
            }
            debug(ev, "dither produces an invalid lock position, try a variation");
            let d = edge_dist(tmp_lock, w, h);
            if q == 0 || d > best {
                (camera_delta, mount_used, best) = (tmp_camera, tmp_mount, d);
            }
        }
        if !self.set_lock_position(lock + camera_delta, cx, ev) {
            debug(ev, "Error thrown from guider.cpp:904->SetLockPosition failed");
            return false;
        }

        // So the reported error shows the dither at once.
        let (dist, dist_ra) = (camera_delta.x.hypot(camera_delta.y), mount_used.x.abs());
        self.avg_distance += dist;
        self.avg_distance_long += dist;
        self.avg_distance_ra += dist_ra;
        self.avg_distance_long_ra += dist_ra;

        // A zero-length move (a settle without a dither) would divide by 0.
        if self.fast_recenter_enabled && dist != 0.0 {
            let remaining = Point::new(mount_used.x.abs(), mount_used.y.abs());
            let dir = (if mount_used.x < 0.0 { 1 } else { -1 }, if mount_used.y < 0.0 { 1 } else { -1 });
            // A bit under the search region, so the star is not lost.
            let f = (self.max_move_pixels() as f64 * 0.7) / remaining.x.hypot(remaining.y);
            self.recenter = Some(Recenter { remaining, step: Point::new(f * remaining.x, f * remaining.y), dir });
        }
        true
    }

    /// `GuiderMultiStar::AutoSelect`: pick the best star on the current
    /// frame (`roi` limits the search), lock on it and, from SELECTING,
    /// run the frame again at once so the state is SELECTED — Ekos starts
    /// guiding right after. The star is returned only when all of that
    /// worked: the primary as that frame left it, which the status shows
    /// (`find_star` answers with [`Guider::lock_position`] instead). The
    /// update holds that frame's and AutoFind's saturation level. When the
    /// current image has data, the caller then hands it to the image logger
    /// (`ImageLogger::LogAutoSelectImage(image, success)`), on a failure too.
    pub fn auto_select(
        &mut self,
        roi: Option<Rect>,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) -> (Option<Star>, GuideUpdate) {
        // Leave room for the calibration of a mount that needs one.
        let room = |m: Option<&dyn GuiderMount>| {
            m.filter(|m| m.mount().is_connected() && !m.is_calibrated()).map(|m| m.calibration_tot_distance())
        };
        let edge = autoselect_edge_allowance(room(cx.primary.as_deref()), room(cx.secondary.as_deref()));
        let img = self.current_image.clone();
        let scale = cx.cal_env.image_scale;
        let res = self.multistar.auto_select(&img, roi, edge, cx.use_subframes, scale, &cx.find);
        // AutoFind set the camera's level (`SetSaturationByADU(true, level)`):
        // the frame run below finds the star with it.
        if let Some(level) = res.saturation_adu_set {
            cx.find.saturation_adu = level;
        }
        let mut log = self.multistar.take_log();
        if img.data.is_empty() {
            ev.extend(log.into_iter().map(GuiderEvent::Debug));
            debug(ev, "Error thrown from guider_multistar.cpp:457->No Current Image");
            return (None, GuideUpdate::default());
        }
        if res.star.is_none() {
            // The tracker's last line is its "failed.", which PHD2 writes
            // after the throw's line; it is written again below.
            log.pop();
        }
        ev.extend(log.into_iter().map(GuiderEvent::Debug));
        let mut out = GuideUpdate::default();
        let mut selected = None;
        match res.star {
            None => {
                let line = if res.find_failed { "482->Unable to find" } else { "474->Unable to AutoFind" };
                debug(ev, format!("Error thrown from guider_multistar.cpp:{line}"));
            }
            Some(star) => {
                if !self.set_lock_position(star.position(), cx, ev) {
                    debug(ev, "Error thrown from guider_multistar.cpp:497->Unable to set Lock Position");
                } else {
                    if self.state == GuiderState::Selecting {
                        debug(ev, format!("AutoSelect: state = {}, call UpdateGuideState", self.state as i32));
                        out = self.update_guide_state(None, false, cx, ev);
                    }
                    // That frame found the star again, from its truncated
                    // position and in the star find mode, and locked on it:
                    // PHD2 reports `m_primaryStar` as it is now.
                    let star = *self.multistar.primary();
                    debug(ev, image_display_line(&self.current_image, cx.gamma));
                    ev.push(GuiderEvent::Status(format!(
                        "Auto-selected star at ({}, {})",
                        fmt_f(star.x, 1),
                        fmt_f(star.y, 1)
                    )));
                    selected = Some(star);
                }
            }
        }
        if selected.is_none() {
            debug(ev, "GuiderMultiStar::AutoSelect failed.");
        }
        out.saturation_adu = res.saturation_adu_set;
        (selected, out)
    }

    // ----------------------------------------------------------- lock shift

    /// `GetLockPosShiftParams`.
    pub fn lock_shift_params(&self) -> LockShiftParams {
        self.lock_shift
    }

    /// `LockPosition().ShiftRate()`: the shift in camera px per second, as
    /// last worked out; `None` before that.
    pub fn lock_shift_rate(&self) -> Option<Point> {
        self.lock.rate
    }

    /// `SetLockPosShiftRate`: a rate per hour in `units`, along RA and Dec
    /// when `mount_coords`. Once the camera angle is known (CALIBRATED or
    /// GUIDING) it is turned into camera px per second at once, and the
    /// guide log notes it when the shift is on.
    pub fn set_lock_shift_rate(
        &mut self,
        rate: Point,
        units: ShiftUnits,
        mount_coords: bool,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) {
        debug(
            ev,
            format!(
                "SetLockPosShiftRate: rate = {},{} units = {} isMountCoords = {}",
                fmt_f(rate.x, 2),
                fmt_f(rate.y, 2),
                units as i32,
                mount_coords as i32
            ),
        );
        (self.lock_shift.rate, self.lock_shift.units, self.lock_shift.mount_coords) = (rate, units, mount_coords);
        if matches!(self.state, GuiderState::Calibrated | GuiderState::Guiding) {
            self.update_lock_shift_camera_coords(cx, ev);
            if self.lock_shift.enabled {
                self.log_lock_shift(ev);
            }
        }
    }

    /// `EnableLockPosShift`. Turning it on puts multi-star guiding aside
    /// (the secondaries would hold the lock back) and starts the shift from
    /// the lock as it is; turning it off restores the user's mode, which may
    /// re-select the star ([`Guider::set_multi_star_mode`], whose update is
    /// returned). The guide log notes it once the camera angle is known.
    pub fn enable_lock_shift(&mut self, enable: bool, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) -> GuideUpdate {
        let mut out = GuideUpdate::default();
        if enable == self.lock_shift.enabled {
            return out;
        }
        debug(ev, format!("EnableLockPosShift: enable = {}", enable as i32));
        self.lock_shift.enabled = enable;
        if enable {
            self.user_multi_star = self.multistar.cfg.multi_star;
            out = self.set_multi_star_mode(false, cx, ev);
            debug(ev, "Multi-star mode disabled for comet tracking");
            self.lock.begin_shift(cx.now_ms);
        } else {
            out = self.set_multi_star_mode(self.user_multi_star, cx, ev);
            debug(ev, "Comet tracking disabled, user's multi-star mode restored");
        }
        if matches!(self.state, GuiderState::Calibrated | GuiderState::Guiding) {
            self.log_lock_shift(ev);
        }
        out
    }

    /// `ShiftLockPosition`: move the lock along the shift; true when that
    /// left the valid lock area.
    fn shift_lock_position(&mut self, now_ms: i64, ev: &mut Vec<GuiderEvent>) -> bool {
        self.lock.update_shift(now_ms);
        let valid = self.is_valid_lock_position(self.lock.point());
        debug(
            ev,
            format!(
                "ShiftLockPos: new pos = {}, {} valid={}",
                fmt_f(self.lock.x, 2),
                fmt_f(self.lock.y, 2),
                valid as i32
            ),
        );
        !valid
    }

    /// `UpdateLockPosShiftCameraCoords`: the rate in camera px per second.
    /// Along RA and Dec it goes through the scope's calibration (the scope
    /// behind an AO); in ″ it is taken as an ephemeris rate, so the axes'
    /// parities apply when known and RA is scaled by cos(Dec) when the
    /// pointing source knows it, then divided by the image scale. PHD2's
    /// "no shift rate set" branch is left out: its rate is always set.
    fn update_lock_shift_camera_coords(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        let p = self.lock_shift;
        let arcsec = p.units == ShiftUnits::Arcsec;
        let mut rate = Point::default();
        if p.mount_coords {
            let mut radec = p.rate;
            debug(
                ev,
                format!(
                    "UpdateLockPosShiftCameraCoords: shift rate mount coords = {},{}",
                    fmt_f(radec.x, 2),
                    fmt_f(radec.y, 2)
                ),
            );
            // TheScope(): the secondary when the primary is an AO.
            let ao = cx.primary.as_deref().is_some_and(|m| m.mount().is_step_guider());
            let pointing = cx.pointing;
            let (scope, _) = cx.split(if ao { MountRole::Secondary } else { MountRole::Primary });
            if let Some(scope) = scope {
                let scope = scope.mount();
                if arcsec {
                    let (ra_parity, dec_parity) = (scope.ra_parity(), scope.dec_parity());
                    if ra_parity != Parity::Unknown || dec_parity != Parity::Unknown {
                        if ra_parity == Parity::Odd {
                            radec.x = -radec.x;
                        }
                        if dec_parity == Parity::Odd {
                            radec.y = -radec.y;
                        }
                        debug(
                            ev,
                            format!(
                                "UpdateLockPosShiftCameraCoords: after parity adjustment: {},{}",
                                fmt_f(radec.x, 2),
                                fmt_f(radec.y, 2)
                            ),
                        );
                    }
                    if let Some(dec) = pointing.and_then(|p| p.declination_radians()) {
                        radec.x *= dec.cos();
                        debug(
                            ev,
                            format!(
                                "UpdateLockPosShiftCameraCoords: RA shift rate adjusted for declination {}",
                                fmt_f(degrees(dec), 1)
                            ),
                        );
                    }
                }
                let role = if ao { MountRole::Secondary } else { MountRole::Primary };
                let mut mev = Vec::new();
                rate = scope.mount_to_camera(radec, true, &mut mev);
                wrap(ev, role, mev);
            }
        } else {
            rate = p.rate;
        }
        debug(
            ev,
            format!(
                "UpdateLockPosShiftCameraCoords: shift rate camera coords = {},{} {}/hr",
                fmt_f(rate.x, 2),
                fmt_f(rate.y, 2),
                if arcsec { "arcsec" } else { "pixels" }
            ),
        );
        if arcsec {
            rate = Point::new(rate.x / cx.cal_env.image_scale, rate.y / cx.cal_env.image_scale);
        }
        rate = Point::new(rate.x / 3600.0, rate.y / 3600.0);
        debug(
            ev,
            format!("UpdateLockPosShiftCameraCoords: shift rate {},{} px/sec", fmt_gp(rate.x, 2), fmt_gp(rate.y, 2)),
        );
        self.lock.set_shift_rate(rate, cx.now_ms);
    }

    /// `GuidingLog::NotifyLockShiftParams`: the `LOCK SHIFT` line, the rate
    /// as set and in camera px per hour.
    fn log_lock_shift(&self, ev: &mut Vec<GuiderEvent>) {
        let p = &self.lock_shift;
        let mut line = format!("LOCK SHIFT, enabled = {} ", p.enabled as i32);
        if p.enabled {
            let cam = self.lock.rate.unwrap_or_default();
            line += &format!(
                "{} rate ({},{}) {}/hr ({},{}) px/hr",
                if p.mount_coords { "RA,Dec" } else { "X,Y" },
                fmt_f(p.rate.x, 2),
                fmt_f(p.rate.y, 2),
                if p.units == ShiftUnits::Arcsec { "arc-sec" } else { "pixels" },
                fmt_f(cam.x * 3600.0, 2),
                fmt_f(cam.y * 3600.0, 2)
            );
        }
        ev.push(GuiderEvent::Info(line));
    }

    /// `EnableMeasurementMode`: from GUIDING, the frames go to the Guiding
    /// Assistant's backlash measurement ([`GuiderEvent::BacklashStep`])
    /// instead of the mount, until it is turned off or guiding ends
    /// ([`GuiderEvent::BacklashError`]). Turning it on in another state
    /// changes nothing.
    pub fn enable_measurement_mode(&mut self, enable: bool) {
        if !enable {
            self.measurement_mode = false;
        } else if self.state == GuiderState::Guiding {
            self.measurement_mode = true;
        }
    }

    // ------------------------------------------------------- error averages

    /// `UpdateCurrentDistance`: the star was found at `distance` px from the
    /// lock (`distance_ra` along RA). While guiding, an EMA with α 0.3 and a
    /// smoother one — the mean of the first samples (the count starts at 1,
    /// so of the first nine), then α 0.045; otherwise both restart from this
    /// sample, as they do after a full pause or a fast recenter.
    fn update_current_distance(&mut self, distance: f64, distance_ra: f64, now_ms: i64) {
        self.star_found_at = Some(now_ms.div_euclid(1000));
        if self.is_guiding() {
            self.avg_distance += 0.3 * (distance - self.avg_distance);
            self.avg_distance_ra += 0.3 * (distance_ra - self.avg_distance_ra);
            self.avg_distance_cnt += 1;
            if self.avg_distance_cnt < 10 {
                let n = self.avg_distance_cnt as f64;
                self.avg_distance_long += (distance - self.avg_distance_long) / n;
                self.avg_distance_long_ra += (distance_ra - self.avg_distance_long_ra) / n;
            } else {
                self.avg_distance_long += 0.045 * (distance - self.avg_distance_long);
                self.avg_distance_long_ra += 0.045 * (distance_ra - self.avg_distance_long_ra);
            }
        } else {
            self.restart_averages(distance, distance_ra);
        }
        if self.avg_distance_need_reset {
            self.restart_averages(distance, distance_ra);
            self.avg_distance_need_reset = false;
        }
    }

    fn restart_averages(&mut self, distance: f64, distance_ra: f64) {
        self.avg_distance = distance;
        self.avg_distance_long = distance;
        self.avg_distance_ra = distance_ra;
        self.avg_distance_long_ra = distance_ra;
        self.avg_distance_cnt = 1;
    }

    /// `::CurrentError`: 100 px when no star was found in the last 20 s.
    fn error_or_lost(&self, avg: f64, now_ms: i64) -> f64 {
        match self.star_found_at {
            Some(t) if now_ms.div_euclid(1000) - t <= STAR_LOST_SECONDS => avg,
            _ => LARGE_DISTANCE,
        }
    }

    /// `CurrentError`: the fast average, RA only or total.
    pub fn current_error(&self, ra_only: bool, now_ms: i64) -> f64 {
        self.error_or_lost(if ra_only { self.avg_distance_ra } else { self.avg_distance }, now_ms)
    }

    /// `CurrentErrorSmoothed`: the slow average.
    pub fn current_error_smoothed(&self, ra_only: bool, now_ms: i64) -> f64 {
        self.error_or_lost(if ra_only { self.avg_distance_long_ra } else { self.avg_distance_long }, now_ms)
    }

    /// `CurrentErrorFrameCount`.
    pub fn current_error_frame_count(&self) -> u32 {
        self.avg_distance_cnt
    }

    // ---------------------------------------------------------------- frame

    /// `GuiderMultiStar::UpdateCurrentPosition` around the tracker: what it
    /// reads from the guider and the primary mount, its debug lines (the
    /// transform's among them), the status and throw lines of a loss, and
    /// the error averages.
    fn update_current_position(
        &mut self,
        cx: &GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) -> Result<PositionUpdate, PositionLost> {
        let primary = cx.primary.as_deref();
        let view = GuidingView {
            guiding: self.is_guiding(),
            paused: self.is_paused(),
            settling: cx.settling,
            ra_only: cx.ra_only,
            guiding_enabled: primary.is_some_and(|m| m.mount().guiding_enabled()),
            error_frame_count: self.avg_distance_cnt,
            error_smoothed: self.current_error_smoothed(cx.ra_only, cx.now_ms),
            exposure_ms: cx.exposure_ms,
            auto_exposure: cx.auto_exposure,
        };
        let lock = self.lock_position();
        let cal = primary.filter(|m| m.is_calibrated()).map(|m| &m.mount().calibration().cal);
        let res = self.multistar.update_current_position(&self.current_image, &cx.find, lock, &view, cx.now_ms, cal);
        match &res {
            Ok(u) => {
                // LogImage comes before RefineOffset and the transform.
                let mut lines = self.multistar.take_log();
                let after = lines.split_off(u.image_log_at.min(lines.len()));
                ev.extend(lines.into_iter().map(GuiderEvent::Debug));
                let stats = ErrorStats::from(&view);
                ev.push(GuiderEvent::ImageLog(ImageLogCall::Distance { distance: u.raw_distance, stats }));
                ev.extend(after.into_iter().map(GuiderEvent::Debug));
                if let Some((d, d_ra)) = u.current_distance {
                    self.update_current_distance(d, d_ra, cx.now_ms);
                }
                ev.push(GuiderEvent::AdjustAutoExposure(u.auto_exposure_snr));
            }
            Err(lost) => {
                self.log_position_lost(lost, ev);
                // The catch's, after the throw's line.
                if lost.reset_auto_exposure() {
                    ev.push(GuiderEvent::ResetAutoExposure);
                }
            }
        }
        res
    }

    /// The tracker's lines of a frame whose star was not accepted, with the
    /// status PHD2 shows and the line of its throw in their places: the
    /// mass status comes before the tracker's mass line, "Recovering" after
    /// the distance checker's lines. "No star selected" is a plain return.
    fn log_position_lost(&mut self, lost: &PositionLost, ev: &mut Vec<GuiderEvent>) {
        if let LossReason::MassChange(limits) = lost.reason {
            let mass = lost.info.star_mass;
            ev.push(GuiderEvent::Status(format!("Mass: {} vs {}", fmt_f(mass, 0), fmt_f(limits[1], 0))));
        }
        self.drain_multistar_log(ev);
        if lost.reason == LossReason::Jump {
            ev.push(GuiderEvent::Status("Recovering".into()));
        }
        // The image logger, right before the throw.
        let active = self.is_calibrating_or_guiding() && !self.is_paused();
        ev.push(GuiderEvent::ImageLog(match lost.reason {
            LossReason::NoStar => ImageLogCall::Deselected,
            _ => ImageLogCall::Dropped { star_error: lost.info.star_error, active },
        }));
        match lost.reason {
            LossReason::NoStar | LossReason::Jump => {}
            LossReason::NotFound => {
                debug(ev, "Error thrown from guider_multistar.cpp:958->UpdateCurrentPosition():newStar not found");
            }
            LossReason::MassChange(_) => debug(ev, "Throw from guider_multistar.cpp:989->massChangeThreshold error"),
        }
        if lost.reason == LossReason::Jump {
            debug(ev, "Throw from guider_multistar.cpp:1020->CheckDistance error");
        }
    }

    /// The tracker's debug lines since its last call, written now.
    fn drain_multistar_log(&mut self, ev: &mut Vec<GuiderEvent>) {
        ev.extend(self.multistar.take_log().into_iter().map(GuiderEvent::Debug));
    }

    /// `UpdateGuideState` (guider.cpp:1265): one frame. `img` becomes the
    /// current image (`None` runs the state machine on the current one, as
    /// AutoSelect does); `stopping` stops guiding instead (looping ended).
    ///
    /// The star is looked for first. Lost, the frame is reported per state —
    /// looping goes on (SELECTED drops the star unless told to ignore it),
    /// a calibration blunders on, guiding logs a DROP and lets the
    /// algorithms deduce a move. Found, a paused guider only deduces;
    /// otherwise SELECTING locks on it, a calibrating mount takes its step,
    /// CALIBRATED starts guiding, and GUIDING schedules the guide step.
    pub fn update_guide_state(
        &mut self,
        img: Option<Arc<UsImage>>,
        stopping: bool,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) -> GuideUpdate {
        use GuiderState::*;
        debug(ev, format!("UpdateGuideState(): m_state={}", self.state as i32));
        if let Some(img) = img {
            let prev = std::mem::replace(&mut self.current_image, img);
            ev.push(GuiderEvent::ImageReplaced(PrevImage(prev)));
        }
        let mut out = GuideUpdate::default();
        let mut status: String;

        let failed = 'frame: {
            if stopping {
                self.stop_guiding(cx, ev);
                status = "Stopped Guiding".into();
                debug(ev, "Throw from guider.cpp:1295->Stopped Guiding");
                break 'frame true;
            }

            if self.lock_shift.enabled && self.is_guiding() && self.shift_lock_position(cx.now_ms, ev) {
                ev.push(GuiderEvent::LockShiftLimitReached);
                ev.push(GuiderEvent::Alert {
                    msg: "Shifted lock position outside allowable area. Lock Position Shift disabled.".into(),
                    kind: AlertKind::Warning,
                    button: None,
                });
                // From GUIDING a multi-star re-selection stops at SELECTED,
                // so nothing of its own frame comes back here.
                out = self.enable_lock_shift(false, cx, ev);
            }

            let frame = self.current_image.frame_num;
            let u = match self.update_current_position(cx, ev) {
                Ok(u) => u,
                Err(lost) => {
                    let info = FrameDroppedInfo {
                        frame_number: frame,
                        time: stopwatch_seconds(cx.guide_time),
                        avg_dist: self.current_error(cx.ra_only, cx.now_ms),
                        star: lost.info,
                    };
                    let looping = GuiderEvent::LoopingExposures { frame, star: None, dropped: Some(lost.info) };
                    match self.state {
                        Uninitialized | Selecting => ev.push(looping),
                        Selected => {
                            ev.push(looping);
                            if !self.ignore_lost_star_looping {
                                self.set_state(Uninitialized, cx, ev);
                                ev.push(GuiderEvent::StarLost(info));
                            }
                            out.pa_tools = PaTools::StarLost;
                        }
                        CalibratingPrimary | CalibratingSecondary => {
                            ev.push(GuiderEvent::CalibrationFrameDropped(info));
                            debug(ev, "Star lost during calibration... blundering on");
                            ev.push(GuiderEvent::StarLost(info));
                            ev.push(GuiderEvent::Status("star lost".into()));
                        }
                        Guiding => {
                            ev.push(GuiderEvent::FrameDropped(info));
                            ev.push(GuiderEvent::StarLost(info));
                            // Let the algorithms dead-reckon.
                            schedule(&mut out, deduced_move(), cx, ev);
                        }
                        Calibrated | Stop => {}
                    }
                    status = lost.info.status.into();
                    debug(ev, "Throw from guider.cpp:1371->unable to update current position");
                    out.position = Some(Err(lost));
                    break 'frame true;
                }
            };

            status = u.status.clone();
            if is_looping_state(self.state) {
                ev.push(GuiderEvent::LoopingExposures { frame, star: Some(*self.multistar.primary()), dropped: None });
            }
            // A star again, so subframes again.
            if self.force_full_frame {
                debug(ev, "setting force full frames = false");
                self.force_full_frame = false;
            }
            let ofs = GuiderOffset { camera_ofs: u.camera_ofs.unwrap_or_default(), mount_ofs: u.mount_ofs };
            out.position = Some(Ok(u));

            if self.is_paused() {
                if self.state == Guiding {
                    schedule(&mut out, deduced_move(), cx, ev);
                }
                status = format!("Paused{}", if self.paused == PauseType::Full { "/full" } else { "/looping" });
                debug(ev, "Throw from guider.cpp:1396->Skipping frame - guider is paused");
                break 'frame true;
            }

            match self.state {
                Selecting => {
                    let pos = self.multistar.primary().position();
                    self.set_lock_position(pos, cx, ev);
                    debug(ev, "CurrentPosition() valid, moving to STATE_SELECTED");
                    ev.push(GuiderEvent::StarSelected(pos));
                    self.set_state(Selected, cx, ev);
                }
                Selected => out.pa_tools = PaTools::UpdateState,
                CalibratingPrimary | CalibratingSecondary | Calibrated => {
                    let mut stage = self.state;
                    if stage == CalibratingPrimary {
                        match self.calibration_step(MountRole::Primary, &mut out, cx, ev) {
                            CalStep::Failed => {
                                self.set_state(Uninitialized, cx, ev);
                                let ao = cx.primary.as_deref().is_some_and(|m| m.mount().is_step_guider());
                                status = if ao { "AO calibration failed" } else { "calibration failed" }.into();
                                debug(ev, "Error thrown from guider.cpp:1429->Calibration failed");
                                break 'frame true;
                            }
                            CalStep::Pending => break 'frame false,
                            CalStep::Done => {}
                        }
                        self.set_state(CalibratingSecondary, cx, ev);
                        // A secondary still to calibrate takes the next
                        // frame, so this one is not shown with the lock in
                        // the wrong place.
                        if self.state == CalibratingSecondary {
                            break 'frame false;
                        }
                        stage = CalibratingSecondary;
                    }
                    if stage == CalibratingSecondary {
                        if cx.secondary.as_deref().is_some_and(|m| m.mount().is_connected()) {
                            match self.calibration_step(MountRole::Secondary, &mut out, cx, ev) {
                                CalStep::Failed => {
                                    self.set_state(Uninitialized, cx, ev);
                                    status = "calibration failed".into();
                                    debug(ev, "Error thrown from guider.cpp:1462->Calibration failed");
                                    break 'frame true;
                                }
                                CalStep::Pending => break 'frame false,
                                CalStep::Done => {}
                            }
                        }
                        self.set_state(Calibrated, cx, ev);
                    }
                    self.set_state(Guiding, cx, ev);
                    self.notify_guiding_started(cx, ev);
                    // The camera angle is known now.
                    self.update_lock_shift_camera_coords(cx, ev);
                    if self.lock_shift.enabled {
                        self.log_lock_shift(ev);
                    }
                    self.check_calibration_auto_load(cx, ev);
                }
                Guiding => match self.recenter {
                    Some(rc) => self.recenter_step(rc, &mut out, cx, ev),
                    None if self.measurement_mode => {
                        ev.push(GuiderEvent::BacklashStep(self.multistar.primary().position()));
                    }
                    // PHD2's star deflection logger is compiled out.
                    None => schedule(&mut out, ScheduledMove::Offset { ofs, opts: MoveOpts::GUIDE_STEP }, cx, ev),
                },
                Uninitialized | Stop => {}
            }
            false
        };

        // While calibrating the mount keeps the status line.
        if failed && self.state != CalibratingPrimary && self.state != CalibratingSecondary {
            ev.push(GuiderEvent::Status(status.clone()));
        }
        if self.measurement_mode && self.state != Guiding {
            ev.push(GuiderEvent::BacklashError);
            self.measurement_mode = false;
        }
        debug(ev, image_display_line(&self.current_image, cx.gamma));
        debug(ev, format!("UpdateGuideState exits: {status}"));
        out
    }

    /// A guiding frame of the fast recenter: a step towards the moved lock,
    /// made straight (`MOVEOPTS_RECOVERY_MOVE`) and told to the algorithms
    /// as a direct move; under half a pixel left on both axes it is done,
    /// and the error averages restart on the next star.
    fn recenter_step(
        &mut self,
        mut rc: Recenter,
        out: &mut GuideUpdate,
        cx: &mut GuiderCx,
        ev: &mut Vec<GuiderEvent>,
    ) {
        let min = |a: f64, b: f64| if a < b { a } else { b }; // wxMin
        let step = Point::new(min(rc.remaining.x, rc.step.x), min(rc.remaining.y, rc.step.y));
        let (dx, dy) = (rc.dir.0 as f64, rc.dir.1 as f64);
        debug(
            ev,
            format!(
                "dither recenter: remaining=({},{}) step=({},{})",
                fmt_f(rc.remaining.x * dx, 1),
                fmt_f(rc.remaining.y * dy, 1),
                fmt_f(step.x * dx, 1),
                fmt_f(step.y * dy, 1)
            ),
        );
        rc.remaining = rc.remaining - step;
        if rc.remaining.x < 0.5 && rc.remaining.y < 0.5 {
            self.recenter = None;
            self.avg_distance_need_reset = true;
        } else {
            self.recenter = Some(rc);
        }
        let mount_ofs = Point::new(step.x * dx, step.y * dy);
        let mut mev = Vec::new();
        let camera_ofs =
            cx.primary.as_deref().map_or(Point::default(), |m| m.mount().mount_to_camera(mount_ofs, true, &mut mev));
        wrap(ev, MountRole::Primary, mev);
        let ofs = GuiderOffset { camera_ofs, mount_ofs: Some(mount_ofs) };
        schedule(out, ScheduledMove::Offset { ofs, opts: MoveOpts::RECOVERY_MOVE }, cx, ev);
        let algo = cx.algo;
        if let Some(m) = cx.primary.as_deref_mut() {
            let mut mev = Vec::new();
            m.mount_mut().notify_direct_move(mount_ofs, algo, &mut mev);
            wrap(ev, MountRole::Primary, mev);
        }
    }

    /// `CheckCalibrationAutoLoad`, each time guiding starts from CALIBRATED
    /// (guider.cpp:1481), calibrated just now or not: turn calibration
    /// auto-restore on for a new profile when the mount reports where it
    /// points, or offer it once, with an "Auto-restore" button, when the
    /// profile has it off.
    fn check_calibration_auto_load(&mut self, cx: &mut GuiderCx, ev: &mut Vec<GuiderEvent>) {
        let stored = cx.profile.get_i32("/AutoLoadCalibration", -1);
        let should = cx.pointing.is_some_and(|p| p.can_report_position());
        if stored == -1 {
            ev.push(GuiderEvent::SetAutoLoadCalibration(should));
        } else if stored == 0 && should && cx.profile.get_i32("/AlreadyAskedCalibAutoload", 0) == 0 {
            // Flags 0: an "info" alert.
            ev.push(GuiderEvent::Alert {
                msg: "Do you want to automatically restore this calibration whenever the profile is used?".into(),
                kind: AlertKind::Info,
                button: Some(AlertButton::AutoRestoreCalibration),
            });
            cx.profile.set_i32("/AlreadyAskedCalibAutoload", 1);
        }
    }
}

/// The dead-reckoning move of a frame without a correction: `ZERO_OFS`
/// with `MOVEOPTS_DEDUCED_MOVE`.
fn deduced_move() -> ScheduledMove {
    ScheduledMove::Offset { ofs: GuiderOffset::default(), opts: MoveOpts::DEDUCED_MOVE }
}

/// `SchedulePrimaryMove` (its debug line; PHD2 prints the mount's address,
/// the mount's name stands in) or `ScheduleAxisMove` (none).
fn schedule(out: &mut GuideUpdate, mv: ScheduledMove, cx: &GuiderCx, ev: &mut Vec<GuiderEvent>) {
    if let ScheduledMove::Offset { ofs, opts } = mv {
        let name = cx.primary.as_deref().map_or("", |m| m.mount().name());
        debug(
            ev,
            format!(
                "SchedulePrimaryMove({name}, x={}, y={}, opts={})",
                fmt_f(ofs.camera_ofs.x, 2),
                fmt_f(ofs.camera_ofs.y, 2),
                opts.0
            ),
        );
    }
    out.moves.push(mv);
}

/// The end of `SetMultiStarMode`: saved, logged, announced as a string
/// parameter logged even when not guiding.
fn announce_multi_star(on: bool, forced: bool, profile: &mut dyn ProfileAccess, ev: &mut Vec<GuiderEvent>) {
    profile.set_bool(keys::MULTISTAR_ENABLED, on);
    let forced = if forced { ", AutoFind forced" } else { "" };
    debug(ev, format!("MultiStar mode {}{forced}", if on { "enabled" } else { "disabled" }));
    ev.push(GuiderEvent::ParamChange { name: "MultiStar", value: Value::from(on.to_string()), force_log: true });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::f64::consts::PI;

    use super::*;
    use crate::engine::algo::MountClass;
    use crate::engine::calibration::{CALIBRATION_RATE_UNCALIBRATED, Calibration};
    use crate::engine::mount::{GuideOutput, StepFrame};
    use crate::engine::scope::ScopeIo;
    use crate::engine::star::{FindMode, FindResult};
    use crate::engine::stepguider::{AoMount, BumpEnv, StepGuider, StepGuiderDevice, StepResult};
    use crate::engine::traits::{CancelToken, MoveResult, Parity, PierSide};

    #[derive(Default)]
    struct Profile {
        f: HashMap<String, f64>,
        i: HashMap<String, i32>,
        s: HashMap<String, String>,
    }

    impl ProfileAccess for Profile {
        fn has(&self, key: &str) -> bool {
            self.f.contains_key(key) || self.i.contains_key(key) || self.s.contains_key(key)
        }
        fn get_f64(&self, key: &str, default: f64) -> f64 {
            self.f.get(key).copied().or(self.i.get(key).map(|&v| v as f64)).unwrap_or(default)
        }
        fn get_i32(&self, key: &str, default: i32) -> i32 {
            self.i.get(key).copied().unwrap_or(default)
        }
        fn get_str(&self, key: &str, default: &str) -> String {
            self.s.get(key).cloned().unwrap_or_else(|| default.to_string())
        }
        fn set_f64(&mut self, key: &str, value: f64) {
            self.f.insert(key.into(), value);
        }
        fn set_i32(&mut self, key: &str, value: i32) {
            self.i.insert(key.into(), value);
        }
        fn set_str(&mut self, key: &str, value: &str) {
            self.s.insert(key.into(), value.into());
        }
    }

    /// A mount that reports its declination only; `reports` is
    /// `CanReportPosition`.
    struct Pointing {
        dec: f64,
        reports: bool,
    }

    impl PointingSource for Pointing {
        fn coordinates(&self) -> Option<(f64, f64, f64)> {
            Some((0.0, self.dec, 0.0))
        }
        fn pier_side(&self) -> PierSide {
            PierSide::Unknown
        }
        fn guide_rates(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_slewing(&self) -> bool {
            false
        }
        fn can_report_position(&self) -> bool {
            self.reports
        }
    }

    /// Records the pulses; the bench then moves the star by them.
    #[derive(Default)]
    struct Pulses(Vec<(Dir, u32)>);

    impl GuideOutput for Pulses {
        fn guide(&mut self, dir: Dir, ms: u32, _cancel: &CancelToken) -> MoveResult {
            self.0.push((dir, ms));
            MoveResult::Ok
        }
    }

    /// A mount whose calibration is scripted, as the C++ harness's
    /// `MountStub`: it begins at once, is calibrated after `cal_frames`
    /// updates or fails on update number `fail_at` (from 0), and says each
    /// call it gets.
    struct FakeMount {
        mount: Mount,
        calibrated: bool,
        cal_frames: u32,
        fail_at: Option<u32>,
        calls: u32,
    }

    impl FakeMount {
        fn new(class: MountClass, calibrated: bool) -> FakeMount {
            let mut mount = Mount::new(class, &mut Profile::default(), 0.0, &mut Vec::new());
            mount.connect();
            FakeMount { mount, calibrated, cal_frames: 1, fail_at: None, calls: 0 }
        }
    }

    impl GuiderMount for FakeMount {
        fn mount(&self) -> &Mount {
            &self.mount
        }
        fn mount_mut(&mut self) -> &mut Mount {
            &mut self.mount
        }
        fn is_calibrated(&self) -> bool {
            self.mount.is_connected() && self.calibrated
        }
        fn calibration_tot_distance(&self) -> i32 {
            0
        }
        fn begin_calibration(
            &mut self,
            _: Option<Point>,
            _: &CalibrationEnv,
            _: Option<&dyn PointingSource>,
            _: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> bool {
            events.push(MountEvent::Debug("BeginCalibration".into()));
            self.calls = 0;
            true
        }
        fn update_calibration_state(
            &mut self,
            _: Point,
            _: &CalibrationEnv,
            _: Option<&dyn PointingSource>,
            _: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> CalibrationNext {
            events.push(MountEvent::Debug("UpdateCalibrationState".into()));
            let n = self.calls;
            self.calls += 1;
            if self.fail_at == Some(n) {
                return CalibrationNext::Failed;
            }
            if self.calls >= self.cal_frames {
                self.calibrated = true;
            }
            CalibrationNext::Moves(Vec::new())
        }
        fn adjust_calibration_for_scope_pointing(
            &mut self,
            _: &ScopePointing,
            _: &str,
            _: &mut dyn ProfileAccess,
            events: &mut Vec<MountEvent>,
        ) -> Adjustment {
            events.push(MountEvent::Debug("AdjustCalibrationForScopePointing".into()));
            Adjustment::default()
        }
        fn notify_guiding_stopped(&mut self, ctx: &AlgoCtx, _: &mut dyn ProfileAccess, events: &mut Vec<MountEvent>) {
            self.mount.notify_guiding_stopped(ctx, events);
        }
    }

    const W: i32 = 200;
    const H: i32 = 200;
    /// px per ms of pulse: West moves the star +x, North +y.
    const RATE: f64 = 0.01;
    /// px per AO step: right moves the star +x, up +y.
    const AO_RATE: f64 = 0.25;

    /// The bench's AO device: 10 steps of travel each way, its position
    /// kept so the star can follow it.
    #[derive(Default)]
    struct BenchAo {
        x: i32,
        y: i32,
    }

    impl StepGuiderDevice for BenchAo {
        fn name(&self) -> String {
            "AO-Simulator".into()
        }
        fn step(&mut self, dir: Dir, steps: u32) -> StepResult {
            let n = steps as i32;
            match dir {
                Dir::North => self.y += n,
                Dir::South => self.y -= n,
                Dir::East => self.x += n,
                Dir::West => self.x -= n,
            }
            StepResult::Ok
        }
        fn max_position(&self, _: Dir) -> i32 {
            10
        }
        fn set_max_position(&mut self, _: i32) -> bool {
            true
        }
        fn center(&mut self) -> bool {
            (self.x, self.y) = (0, 0);
            true
        }
    }

    /// Saturation by ADU with no level yet: AutoSelect sets one. The
    /// bench's 2″/px makes "Auto" downsampling 1×.
    const FP: FindParams = FindParams { mode: FindMode::Centroid, saturation_by_adu: true, saturation_adu: 0 };

    /// Fixed pattern noise in −20..=20 ADU.
    fn noise(x: i32, y: i32) -> f64 {
        let mut h = (x as u32).wrapping_mul(73856093) ^ (y as u32).wrapping_mul(19349663);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1e995);
        h ^= h >> 15;
        ((h % 41) as i32 - 20) as f64
    }

    /// A 16-bit frame: 1000 ADU, the noise and a Gaussian star (σ 1.6 px)
    /// `amp` ADU high.
    fn render(star: Option<Point>, amp: f64, frame: u32) -> Arc<UsImage> {
        let mut img = UsImage::new(W, H);
        img.bits_per_pixel = 16;
        img.frame_num = frame;
        for y in 0..H {
            for x in 0..W {
                let mut v = 1000.0 + noise(x, y);
                if let Some(s) = star {
                    let (dx, dy) = (x as f64 - s.x, y as f64 - s.y);
                    v += amp * (-(dx * dx + dy * dy) / (2.0 * 1.6 * 1.6)).exp();
                }
                *img.pixel_mut(x, y) = (v + 0.5).floor() as u16;
            }
        }
        Arc::new(img)
    }

    /// The guider on a scope that moves a star: each frame drifts the star,
    /// goes through the guider, and the moves are made before the next one.
    struct Bench {
        guider: Guider,
        scope: Scope,
        profile: Profile,
        star: Point,
        visible: bool,
        /// The star's peak over the sky, ADU.
        amp: f64,
        drift: Point,
        env: CalibrationEnv,
        sp: ScopePointing,
        algo: AlgoCtx,
        now_ms: i64,
        frame: u32,
        busy: bool,
        /// What each call's finds read, [`FP`] at first.
        fp: FindParams,
        pointing: Option<Pointing>,
        /// Scripted mounts the calls drive instead of the scope: the
        /// primary and the secondary.
        fakes: Option<(FakeMount, Option<FakeMount>)>,
        /// An AO as the primary mount, the scope behind it.
        ao: Option<(StepGuider, BenchAo)>,
        ev: Vec<GuiderEvent>,
        /// What the scope said while the bench made the moves.
        mev: Vec<MountEvent>,
    }

    impl Bench {
        fn new() -> Bench {
            let mut profile = Profile::default();
            let mut ev = Vec::new();
            let guider = Guider::new(&mut profile, false, &mut ev);
            let mut scope = Scope::new(&mut profile, false, 0.0, &mut Vec::new());
            scope.mount.connect();
            Bench {
                guider,
                scope,
                profile,
                star: Point::new(100.0, 100.0),
                visible: true,
                amp: 2000.0,
                drift: Point::default(),
                env: CalibrationEnv {
                    binning: 1,
                    focal_length: 400,
                    pixel_size: 3.8,
                    image_scale: 2.0,
                    rotator_angle: None,
                    fast_recenter: true,
                    max_move_pixels: 15,
                    can_pulse_guide: true,
                    timestamp: "T".into(),
                },
                sp: ScopePointing {
                    declination: None,
                    pier_side: PierSide::Unknown,
                    rotator_angle: None,
                    binning: 1,
                    guide_rates: None,
                    camera_pixel_size: 3.8,
                    profile_pixel_size: 3.8,
                },
                algo: AlgoCtx::default(),
                now_ms: 1_700_000_000_000,
                frame: 0,
                busy: false,
                fp: FP,
                pointing: None,
                fakes: None,
                ao: None,
                ev,
                mev: Vec::new(),
            }
        }

        fn call<R>(&mut self, f: impl FnOnce(&mut Guider, &mut GuiderCx, &mut Vec<GuiderEvent>) -> R) -> R {
            let mut ao_mount;
            let (primary, secondary): (&mut dyn GuiderMount, _) = match (&mut self.fakes, &mut self.ao) {
                (Some((p, s)), _) => (p, s.as_mut().map(|s| s as &mut dyn GuiderMount)),
                (None, Some((ao, dev))) => {
                    ao_mount = AoMount { ao, dev };
                    (&mut ao_mount, Some(&mut self.scope as &mut dyn GuiderMount))
                }
                (None, None) => (&mut self.scope, None),
            };
            let mut cx = GuiderCx {
                primary: Some(primary),
                secondary,
                profile: &mut self.profile,
                pointing: self.pointing.as_ref().map(|p| p as &dyn PointingSource),
                cal_env: &self.env,
                scope_pointing: &self.sp,
                algo: &self.algo,
                find: self.fp,
                use_subframes: false,
                ra_only: false,
                settling: false,
                exposure_ms: 1000,
                auto_exposure: false,
                guide_time: 12.3456,
                now_ms: self.now_ms,
                busy: self.busy,
                gamma: 0.4,
            };
            f(&mut self.guider, &mut cx, &mut self.ev)
        }

        /// One exposure, two seconds after the last.
        fn frame(&mut self) -> GuideUpdate {
            self.frame += 1;
            self.now_ms += 2000;
            self.star = self.star + self.drift;
            let img = render(self.visible.then_some(self.star), self.amp, self.frame);
            let upd = self.call(|g, cx, ev| g.update_guide_state(Some(img), false, cx, ev));
            self.make_moves(&upd.moves);
            upd
        }

        fn make_moves(&mut self, moves: &[ScheduledMove]) {
            for mv in moves {
                if let Some((ao, dev)) = &mut self.ao {
                    // The AO's moves; the star follows its steps.
                    let before = (dev.x, dev.y);
                    match *mv {
                        ScheduledMove::Calibration { role: MountRole::Primary, dir, amount } => {
                            ao.move_axis(dir, amount, MoveOpts::CALIBRATION_MOVE, dev, &mut self.mev);
                        }
                        ScheduledMove::Offset { mut ofs, opts } => {
                            let env = BumpEnv::default();
                            let (algo, profile) = (&self.algo, &self.profile);
                            ao.move_offset(&mut ofs, opts, StepFrame::default, algo, &env, dev, profile, &mut self.mev);
                        }
                        ScheduledMove::Calibration { .. } => panic!("the scope behind the AO is calibrated"),
                    }
                    self.star.x += AO_RATE * (dev.x - before.0) as f64;
                    self.star.y += AO_RATE * (dev.y - before.1) as f64;
                    self.call(|g, cx, ev| g.move_complete(cx, ev));
                    continue;
                }
                let mut out = Pulses::default();
                let cancel = CancelToken::new();
                let mut io = ScopeIo { output: &mut out, pointing: None, cancel: &cancel, now: 0.0 };
                match *mv {
                    ScheduledMove::Calibration { dir, amount, .. } => {
                        self.scope.move_axis(dir, amount, MoveOpts::CALIBRATION_MOVE, &mut io, &mut self.mev);
                    }
                    ScheduledMove::Offset { mut ofs, opts } => {
                        let (algo, profile) = (&self.algo, &mut self.profile);
                        let mev = &mut self.mev;
                        self.scope.move_offset(&mut ofs, opts, StepFrame::default, algo, &mut io, profile, mev);
                    }
                }
                for (dir, ms) in out.0 {
                    let d = RATE * ms as f64;
                    match dir {
                        Dir::West => self.star.x += d,
                        Dir::East => self.star.x -= d,
                        Dir::North => self.star.y += d,
                        Dir::South => self.star.y -= d,
                    }
                }
                self.call(|g, cx, ev| g.move_complete(cx, ev));
            }
        }

        fn has(&self, e: &GuiderEvent) -> bool {
            self.ev.contains(e)
        }

        fn debug(&self, line: &str) -> bool {
            self.has(&GuiderEvent::Debug(line.into()))
        }

        /// SELECTING to SELECTED through AutoSelect, on a frame of the star.
        fn select(&mut self) {
            self.frame();
            let (star, _) = self.call(|g, cx, ev| g.auto_select(None, cx, ev));
            assert!(star.is_some());
        }

        /// The calibration the bench's sky gives (see the calibration test).
        fn calibrate_scope(&mut self) {
            let cal = Calibration {
                x_rate: RATE,
                y_rate: RATE,
                x_angle: PI,
                y_angle: PI / 2.0,
                ra_guide_parity: Parity::Even,
                dec_guide_parity: Parity::Even,
                is_valid: true,
                ..Calibration::default()
            };
            self.scope.set_calibration(&cal, "T", &mut self.profile, &mut Vec::new());
            assert!(GuiderMount::is_calibrated(&self.scope));
        }
    }

    #[test]
    fn new_guider_is_selecting_and_writes_its_settings() {
        let mut profile = Profile::default();
        profile.set_f64(keys::SEARCH_REGION, 60.0);
        profile.set_f64(keys::MASS_CHANGE_THRESHOLD, -1.0);
        let mut ev = Vec::new();
        let g = Guider::new(&mut profile, false, &mut ev);
        assert_eq!(g.state(), GuiderState::Selecting);
        assert_eq!(g.max_move_pixels(), 50);
        assert!(g.is_fast_recenter_enabled());
        assert_eq!(profile.get_i32(keys::SEARCH_REGION, 0), 50);
        assert_eq!(profile.get_f64(keys::MASS_CHANGE_THRESHOLD, 0.0), 0.5);
        assert_eq!(profile.get_i32(FAST_RECENTER_KEY, -1), 1);
        assert_eq!(profile.get_i32(SCALE_IMAGE_KEY, -1), 1);
        let slit: Vec<i32> = SLIT_KEYS.iter().map(|(k, _)| profile.get_i32(k, -1)).collect();
        assert_eq!(slit, [376, 290, 8, 100, 0]);
        let lines: Vec<String> = ev
            .iter()
            .filter_map(|e| match e {
                GuiderEvent::Debug(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                "guider state => UNINITIALIZED",
                "Changing from state UNINITIALIZED to UNINITIALIZED",
                "guider state => SELECTING",
                "Setting StarMinHFD = 1.50",
                "Setting MaxHFD = 20.0",
                "Setting StarMinSNR = 6.0",
                "Setting AutoSelDownsample = 0",
                "Error thrown from guider_multistar.cpp:304->massChangeThreshold < 0",
                "Error thrown from guider_multistar.cpp:347->searchRegion > MAX_SEARCH_REGION",
                "MultiStar mode disabled",
            ]
        );
        assert_eq!(
            ev.last(),
            Some(&GuiderEvent::ParamChange { name: "MultiStar", value: Value::from("false"), force_log: true })
        );
    }

    #[test]
    fn auto_select_moves_selecting_to_selected() {
        let mut b = Bench::new();
        // Looping with no star: nothing to look for.
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Selecting);
        assert_eq!(b.guider.app_state(true).as_str(), "Looping");
        assert!(b.debug("UpdateCurrentPosition: no star selected"));
        assert!(b.debug("UpdateGuideState exits: No star selected"));

        b.ev.clear();
        let (star, upd) = b.call(|g, cx, ev| g.auto_select(None, cx, ev));
        let star = star.unwrap();
        assert!(star.position().distance(&Point::new(100.0, 100.0)) < 0.1);
        // AutoFind set the saturation level from the 16-bit depth.
        assert_eq!(upd.saturation_adu, Some(65535));
        assert!(matches!(upd.position, Some(Ok(_))));
        assert!(upd.moves.is_empty());
        assert_eq!(b.guider.state(), GuiderState::Selected);
        assert_eq!(b.guider.lock_position(), Some(star.position()));
        assert!(b.has(&GuiderEvent::LockPositionSet(star.position())));
        assert!(b.has(&GuiderEvent::StarSelected(star.position())));
        assert!(b.debug("AutoSelect: state = 1, call UpdateGuideState"));
        assert_eq!(b.guider.app_state(true).as_str(), "Selected");
        assert_eq!(b.guider.app_state(false).as_str(), "Stopped");

        // The next frames track it; SELECTED hands the frame to the PA tools.
        let upd = b.frame();
        assert_eq!(upd.pa_tools, PaTools::UpdateState);
        assert_eq!(b.guider.state(), GuiderState::Selected);
    }

    #[test]
    fn a_star_picked_by_position_is_selected_on_the_next_frame() {
        let mut b = Bench::new();
        b.frame();
        assert!(b.call(|g, cx, ev| g.set_lock_pos_to_star_at_position(Point::new(102.0, 99.0), cx, ev)));
        assert_eq!(b.guider.state(), GuiderState::Selecting);
        assert_eq!(b.guider.app_state(true), AppState::Selected);
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Selected);
        // Off the frame: refused.
        assert!(!b.call(|g, cx, ev| g.set_lock_pos_to_star_at_position(Point::new(0.0, 50.0), cx, ev)));
        assert!(b.debug("Error thrown from guider_multistar.cpp:380->invalid x value"));
    }

    fn debug_lines(b: &Bench) -> Vec<&str> {
        b.ev.iter()
            .filter_map(|e| match e {
                GuiderEvent::Debug(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Each of PHD2's lines once, the tracker's and the guider's in
    /// PHD2's order: AutoSelect's throw before the tracker's "failed.",
    /// SetCurrentPosition's after its line.
    #[test]
    fn auto_select_and_set_current_position_lines_in_phd2s_order() {
        let mut b = Bench::new();
        b.ev.clear();
        assert!(b.call(|g, cx, ev| g.auto_select(None, cx, ev)).0.is_none());
        assert_eq!(
            debug_lines(&b),
            ["GuiderMultiStar::AutoSelect enter", "Error thrown from guider_multistar.cpp:457->No Current Image"]
        );

        // An empty sky: AutoFind finds nothing.
        b.visible = false;
        b.frame();
        b.ev.clear();
        assert!(b.call(|g, cx, ev| g.auto_select(None, cx, ev)).0.is_none());
        assert_eq!(
            debug_lines(&b),
            [
                "GuiderMultiStar::AutoSelect enter",
                "Error thrown from guider_multistar.cpp:474->Unable to AutoFind",
                "GuiderMultiStar::AutoSelect failed.",
            ]
        );

        // Looping without a star: the tracker's line, no throw.
        b.visible = true;
        b.ev.clear();
        b.frame();
        assert_eq!(
            debug_lines(&b),
            [
                "UpdateGuideState(): m_state=1",
                "UpdateCurrentPosition: no star selected",
                "Throw from guider.cpp:1371->unable to update current position",
                "UpdateImageDisplay: Size=(200,200) min=0, max=0, med=0, FiltMin=0, FiltMax=0, Gamma=0.400",
                "UpdateGuideState exits: No star selected",
            ]
        );

        b.ev.clear();
        assert!(b.call(|g, cx, ev| g.auto_select(None, cx, ev)).0.is_some());
        assert_eq!(
            debug_lines(&b)[..5],
            [
                "GuiderMultiStar::AutoSelect enter",
                "MultiStar: List (1): {99.99, 100.00}(117.0), ",
                "setting lock position to (99.99, 100.00)",
                "AutoSelect: state = 1, call UpdateGuideState",
                "UpdateGuideState(): m_state=1",
            ]
        );

        b.ev.clear();
        assert!(!b.call(|g, cx, ev| g.set_lock_pos_to_star_at_position(Point::new(150.0, 250.0), cx, ev)));
        assert_eq!(
            debug_lines(&b),
            ["SetCurrentPosition(150.00,250.00)", "Error thrown from guider_multistar.cpp:385->invalid y value"]
        );
    }

    /// A rejected star: the mass status before the tracker's mass line,
    /// "Recovering" after the distance checker's lines, each throw last;
    /// a guide step's transform lines come from the tracker.
    #[test]
    fn rejected_stars_and_the_transform_lines_in_phd2s_order() {
        let mut b = Bench::new();
        b.select();
        for _ in 0..5 {
            b.frame();
        }
        b.amp = 6000.0;
        b.ev.clear();
        b.frame();
        // The frame switched in goes to the image logger's ring at once.
        assert!(matches!(&b.ev[1], GuiderEvent::ImageReplaced(_)));
        let ev: Vec<_> = b.ev.iter().filter(|e| !matches!(e, GuiderEvent::ImageReplaced(_))).cloned().collect();
        let dropped = |star_error, active| GuiderEvent::ImageLog(ImageLogCall::Dropped { star_error, active });
        assert_eq!(
            ev[..6],
            [
                GuiderEvent::Debug("UpdateGuideState(): m_state=2".into()),
                GuiderEvent::Status("Mass: 95766 vs 31725".into()),
                GuiderEvent::Debug(
                    "UpdateCurrentPosition: star mass new=95766.3 exp=31725.2 thresh=50% \
                     limits=(15862.6, 47587.8, 63450.5)"
                        .into()
                ),
                GuiderEvent::Debug("DistanceChecker: activated".into()),
                // LogImage right before the throw (guider_multistar.cpp:987).
                dropped(FindResult::MassChange, false),
                GuiderEvent::Debug("Throw from guider_multistar.cpp:989->massChangeThreshold error".into()),
            ]
        );

        let mut b = guiding_at(Point::new(100.0, 100.0));
        b.guider.multistar_mut().cfg.tolerate_jumps_enabled = true;
        b.ev.clear();
        b.frame();
        assert_eq!(
            debug_lines(&b)[..5],
            [
                "UpdateGuideState(): m_state=6",
                "CameraToMount -- cameraTheta (0.00) - m_xAngle (3.14) = xAngle (-3.14 = -3.14)",
                "CameraToMount -- cameraTheta (0.00) - (m_xAngle (3.14) + m_yAngleError (-3.14)) = \
                 yAngle (0.00 = 0.00)",
                "CameraToMount -- cameraX=0.00 cameraY=0.00 hyp=0.00 cameraTheta=0.00 mountX=-0.00 mountY=0.00, \
                 mountTheta=0.00",
                "SchedulePrimaryMove(, x=0.00, y=0.00, opts=13)",
            ]
        );
        // Ten guiding frames on a still star, then a 6 px jump.
        for _ in 0..10 {
            b.frame();
        }
        b.star = b.star + Point::new(6.0, 0.0);
        b.ev.clear();
        b.frame();
        let ev: Vec<_> = b.ev.iter().filter(|e| !matches!(e, GuiderEvent::ImageReplaced(_))).cloned().collect();
        assert_eq!(
            ev[..6],
            [
                GuiderEvent::Debug("UpdateGuideState(): m_state=6".into()),
                GuiderEvent::Debug(
                    "DistanceChecker: reject for large offset (6.01 > 0.00) avgDist = 0.00 count = 12".into()
                ),
                GuiderEvent::Debug("DistanceChecker: activated".into()),
                GuiderEvent::Status("Recovering".into()),
                dropped(FindResult::Error, true),
                GuiderEvent::Debug("Throw from guider_multistar.cpp:1020->CheckDistance error".into()),
            ]
        );
        // A star found: LogImage with the frame's error averages, before the
        // tracker's transform lines (guider_multistar.cpp:1023).
        b.star = b.star - Point::new(6.0, 0.0);
        b.ev.clear();
        b.frame();
        let at = b.ev.iter().position(|e| matches!(e, GuiderEvent::ImageLog(_))).unwrap();
        let transform = b.ev.iter().position(|e| matches!(e, GuiderEvent::Debug(l) if l.starts_with("CameraToMount"))).unwrap();
        assert!(at < transform);
        let GuiderEvent::ImageLog(ImageLogCall::Distance { stats, .. }) = b.ev[at] else { panic!() };
        assert!(stats.guiding && stats.frame_count > 0);
    }

    #[test]
    fn calibration_runs_through_the_scope_then_guiding_starts() {
        let mut b = Bench::new();
        b.select();
        // Not from SELECTING: more than one state up.
        let mut c = Bench::new();
        c.call(|g, cx, ev| g.start_guiding(cx, ev));
        assert_eq!(c.guider.state(), GuiderState::Selecting);
        assert!(c.debug("Error thrown from guider.cpp:972->Illegal state transition"));

        b.ev.clear();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        assert_eq!(b.guider.state(), GuiderState::CalibratingPrimary);
        assert!(b.has(&GuiderEvent::StartCalibration(MountRole::Primary)));
        assert_eq!(b.guider.app_state(true).as_str(), "Calibrating");

        let mut moves = 0;
        for _ in 0..80 {
            let upd = b.frame();
            moves += upd.moves.iter().filter(|m| matches!(m, ScheduledMove::Calibration { .. })).count();
            if b.guider.state() == GuiderState::Guiding {
                break;
            }
        }
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        assert!(moves > 8, "{moves} calibration moves");
        let c = b.scope.mount.mount_cal();
        // West moved the star +x: the RA axis points back along −x; North
        // moved it +y, the Dec axis along it.
        assert!((c.x_angle.abs() - PI).abs() < 0.05, "{}", c.x_angle);
        assert!((c.y_angle - PI / 2.0).abs() < 0.05, "{}", c.y_angle);
        assert!((c.x_rate - RATE).abs() < 0.0005, "{}", c.x_rate);
        assert!((c.y_rate - RATE).abs() < 0.0005, "{}", c.y_rate);
        assert!(b.has(&GuiderEvent::Mount(MountRole::Primary, MountEvent::CalibrationComplete)));
        // Guiding started: the lock where the star is, the status, the
        // mounts' notice, then the frame's part; auto-restore offered off
        // with no pointing source.
        let pos = |e: &GuiderEvent| b.ev.iter().position(|x| x == e).unwrap();
        let status = pos(&GuiderEvent::Status("Guiding".into()));
        let notice = MountEvent::Debug("Mount: notify guiding started".into());
        let notice = pos(&GuiderEvent::Mount(MountRole::Primary, notice));
        let started = pos(&GuiderEvent::GuidingStarted);
        assert!(status < notice && notice < started);
        assert!(b.ev[started + 1..].contains(&GuiderEvent::SetAutoLoadCalibration(false)));
        assert!(b.ev.iter().any(|e| matches!(e, GuiderEvent::PointingAdjusted(MountRole::Primary, _))));
        let lock = b.guider.lock_position().unwrap();
        assert!(lock.distance(&b.star) < 0.2);

        // Guiding against a drift keeps the star near the lock.
        b.drift = Point::new(0.3, -0.2);
        for _ in 0..15 {
            let upd = b.frame();
            assert!(matches!(upd.moves[..], [ScheduledMove::Offset { opts: MoveOpts::GUIDE_STEP, .. }]));
        }
        assert!(b.star.distance(&lock) < 1.5, "{:?} vs {:?}", b.star, lock);
        assert_eq!(b.guider.app_state(true).as_str(), "Guiding");
    }

    /// The controller's hooks on a scope: its own `ClearCalibration`, no
    /// AO bump, and the scope itself for the Dec guide mode.
    #[test]
    fn the_scope_answers_the_controllers_hooks() {
        use crate::engine::scope::CalibrationState;
        let mut b = Bench::new();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        assert_ne!(b.scope.calibration_state(), CalibrationState::Cleared);
        let m: &mut dyn GuiderMount = &mut b.scope;
        assert!(!m.is_bump_in_progress());
        assert!(m.scope_mut().is_some());
        m.clear_calibration();
        assert_eq!(b.scope.calibration_state(), CalibrationState::Cleared);

        b.calibrate_scope();
        GuiderMount::clear_calibration(&mut b.scope);
        assert!(!GuiderMount::is_calibrated(&b.scope));
    }

    /// An AO as the primary mount, the scope behind it already calibrated:
    /// the AO's calibration schedules two moves on a frame (down and right
    /// to the lower-right corner), guiding starts on the AO with its
    /// pointing adjustment (the binning only), and stopping recentres it.
    #[test]
    fn the_guider_calibrates_and_guides_an_ao() {
        use crate::engine::calibration::AdjustStep;
        let mut b = Bench::new();
        b.calibrate_scope();
        let mut ao = StepGuider::new(&mut b.profile, 0.0, &mut Vec::new());
        let dev = BenchAo::default();
        ao.connect(&dev, &mut Vec::new());
        b.ao = Some((ao, dev));
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        assert_eq!(b.guider.state(), GuiderState::CalibratingPrimary);
        assert!(b.has(&GuiderEvent::StartCalibration(MountRole::Primary)));

        let upd = b.frame();
        let cal = |dir, amount| ScheduledMove::Calibration { role: MountRole::Primary, dir, amount };
        assert_eq!(upd.moves, [cal(Dir::DOWN, 4), cal(Dir::RIGHT, 4)]);
        for _ in 0..60 {
            b.frame();
            if b.guider.state() == GuiderState::Guiding {
                break;
            }
        }
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        let c = b.ao.as_ref().unwrap().0.mount().mount_cal().clone();
        // Right moves the star along +x, up along +y.
        assert!(c.x_angle.abs() < 0.01 && (c.y_angle - PI / 2.0).abs() < 0.01, "{c:?}");
        assert!((c.x_rate - AO_RATE).abs() < 0.005 && (c.y_rate - AO_RATE).abs() < 0.005, "{c:?}");
        let adj = b.ev.iter().find_map(|e| match e {
            GuiderEvent::PointingAdjusted(MountRole::Primary, a) => Some(a),
            _ => None,
        });
        let no_change = AdjustStep::Log("stepguider: scope pointing change, no change to calibration".into());
        assert_eq!(adj.unwrap().steps, [no_change]);

        // The AO takes up a drift on its own.
        let lock = b.guider.lock_position().unwrap();
        b.drift = Point::new(0.1, -0.05);
        for _ in 0..10 {
            let upd = b.frame();
            assert!(matches!(upd.moves[..], [ScheduledMove::Offset { opts: MoveOpts::GUIDE_STEP, .. }]));
        }
        assert!(b.star.distance(&lock) < 0.5, "{:?} vs {lock:?}", b.star);
        assert!(b.ao.as_ref().unwrap().1.x < -2);

        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        let (ao, dev) = b.ao.as_ref().unwrap();
        assert_eq!((ao.ao_pos(), (dev.x, dev.y)), ((0, 0), (0, 0)));
        assert!(b.has(&GuiderEvent::GuidingStopped));
    }

    #[test]
    fn a_calibrated_mount_skips_to_calibrated() {
        let mut b = Bench::new();
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        // No secondary mount: straight to CALIBRATED, no calibration.
        assert_eq!(b.guider.state(), GuiderState::Calibrated);
        assert!(!b.ev.iter().any(|e| matches!(e, GuiderEvent::StartCalibration(_))));
        assert_eq!(b.guider.app_state(true).as_str(), "Selected");
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
    }

    #[test]
    fn a_star_lost_while_guiding_drops_the_frame_and_deduces_a_move() {
        let mut b = Bench::new();
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.frame();
        b.frame();
        assert_eq!(b.guider.app_state(true), AppState::Guiding);

        b.ev.clear();
        b.visible = false;
        let upd = b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        assert_eq!(b.guider.app_state(true).as_str(), "LostLock");
        assert_eq!(upd.moves, [deduced_move()]);
        let dropped = b.ev.iter().find_map(|e| match e {
            GuiderEvent::FrameDropped(i) => Some(*i),
            _ => None,
        });
        let info = dropped.unwrap();
        assert_eq!(info.frame_number, b.frame);
        // TimeSinceGuidingStarted counts whole ms.
        assert_eq!(info.time, 12.345);
        assert_eq!(info.star.status, "Star lost - low mass");
        let i = b.ev.iter().position(|e| *e == GuiderEvent::FrameDropped(info)).unwrap();
        assert_eq!(b.ev[i + 1], GuiderEvent::StarLost(info));
        assert!(b.debug("Error thrown from guider_multistar.cpp:958->UpdateCurrentPosition():newStar not found"));
        assert!(b.debug("SchedulePrimaryMove(, x=0.00, y=0.00, opts=14)"));
        assert!(b.debug("Throw from guider.cpp:1371->unable to update current position"));
        assert!(b.has(&GuiderEvent::Status("Star lost - low mass".into())));
        assert!(!b.ev.iter().any(|e| matches!(e, GuiderEvent::LoopingExposures { .. })));

        // Back again: guide steps again.
        b.visible = true;
        let upd = b.frame();
        assert!(matches!(upd.moves[..], [ScheduledMove::Offset { opts: MoveOpts::GUIDE_STEP, .. }]));
        assert_eq!(b.guider.app_state(true), AppState::Guiding);
    }

    #[test]
    fn a_star_lost_while_selected_drops_the_selection() {
        let mut b = Bench::new();
        b.select();
        b.visible = false;
        b.ev.clear();
        let upd = b.frame();
        assert_eq!(upd.pa_tools, PaTools::StarLost);
        assert_eq!(b.guider.state(), GuiderState::Selecting);
        assert!(b.has(&GuiderEvent::LockPositionLost));
        assert!(b.ev.iter().any(|e| matches!(e, GuiderEvent::StarLost(_))));
        assert!(b.guider.lock_position().is_none());

        // Told to ignore it, the drift tool keeps it.
        let mut b = Bench::new();
        b.select();
        b.call(|g, _, ev| g.set_ignore_lost_star_looping(true, ev));
        b.visible = false;
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Selected);
        assert!(b.guider.lock_position().is_some());
    }

    #[test]
    fn stopping_reports_a_manual_calibration_stop_and_waits_for_the_move() {
        let mut b = Bench::new();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.frame();
        b.ev.clear();
        // A calibration move is still out.
        b.busy = true;
        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        assert_eq!(
            b.ev[0],
            GuiderEvent::CalibrationFailed { role: MountRole::Primary, reason: "Calibration manually stopped" }
        );
        assert!(!b.has(&GuiderEvent::GuidingStopped));
        // Calibrating stops to UNINITIALIZED, which goes on to SELECTING.
        assert_eq!(b.guider.state(), GuiderState::Selecting);
        assert!(b.has(&GuiderEvent::LockPositionLost));
        b.busy = false;
        b.call(|g, cx, ev| g.move_complete(cx, ev));
        assert_eq!(b.ev.last(), Some(&GuiderEvent::GuidingStopped));

        // Guiding with nothing out: stopped at once, back to SELECTED.
        let mut b = Bench::new();
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.frame();
        b.ev.clear();
        let upd = b.call(|g, cx, ev| g.update_guide_state(None, true, cx, ev));
        assert!(upd.moves.is_empty());
        assert_eq!(b.guider.state(), GuiderState::Selected);
        assert!(!b.ev.iter().any(|e| matches!(e, GuiderEvent::CalibrationFailed { .. })));
        assert!(b.has(&GuiderEvent::GuidingStopped));
        assert!(b.has(&GuiderEvent::Status("Stopped Guiding".into())));
        assert_eq!(b.ev.last(), Some(&GuiderEvent::Debug("UpdateGuideState exits: Stopped Guiding".into())));
    }

    #[test]
    fn a_paused_guider_only_deduces() {
        let mut b = Bench::new();
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.frame();
        b.call(|g, _, ev| g.set_paused(PauseType::Guiding, ev));
        b.ev.clear();
        let upd = b.frame();
        assert_eq!(upd.moves, [deduced_move()]);
        assert!(b.has(&GuiderEvent::Status("Paused/looping".into())));
        assert_eq!(b.guider.app_state(false).as_str(), "Paused");
        // Leaving a full pause restarts the averages on the next star.
        b.call(|g, _, ev| g.set_paused(PauseType::Full, ev));
        assert_eq!(b.call(|g, _, ev| g.set_paused(PauseType::None, ev)), PauseType::Full);
        assert!(b.guider.avg_distance_need_reset);
    }

    #[test]
    fn app_state_per_state() {
        let mut ev = Vec::new();
        let mut g = Guider::new(&mut Profile::default(), false, &mut ev);
        let mut app = |s: GuiderState| {
            g.state = s;
            g.app_state(true).as_str()
        };
        assert_eq!(app(GuiderState::Uninitialized), "Stopped");
        assert_eq!(app(GuiderState::Selecting), "Looping");
        assert_eq!(app(GuiderState::Selected), "Selected");
        assert_eq!(app(GuiderState::CalibratingPrimary), "Calibrating");
        assert_eq!(app(GuiderState::CalibratingSecondary), "Calibrating");
        assert_eq!(app(GuiderState::Calibrated), "Selected");
        // Guiding with no star found on the last frame.
        assert_eq!(app(GuiderState::Guiding), "LostLock");
        g.paused = PauseType::Full;
        assert_eq!(g.app_state(false).as_str(), "Paused");
        assert_eq!(AppState::Looping as i32, 101);
    }

    #[test]
    fn error_averages_over_ten_guiding_frames() {
        let mut ev = Vec::new();
        let mut g = Guider::new(&mut Profile::default(), false, &mut ev);
        let t0 = 1_700_000_000_000;
        // Not guiding: every sample restarts them.
        g.update_current_distance(2.0, 1.0, t0);
        assert_eq!((g.current_error(false, t0), g.current_error_frame_count()), (2.0, 1));
        g.state = GuiderState::Guiding;
        for i in 1..=10 {
            g.update_current_distance(i as f64, i as f64 / 2.0, t0);
        }
        assert_eq!(g.current_error_frame_count(), 11);
        assert_eq!(g.current_error(false, t0), 7.7890726079000006);
        assert_eq!(g.current_error(true, t0), 3.8945363039500003);
        assert_eq!(g.current_error_smoothed(false, t0), 4.687547222222222);
        assert_eq!(g.current_error_smoothed(true, t0), 2.343773611111111);
        // 100 px once no star was found for more than 20 whole seconds.
        assert_eq!(g.current_error(false, t0 + 20_999), 7.7890726079000006);
        assert_eq!(g.current_error(false, t0 + 21_000), 100.0);
        // A reset asked for restarts them on the next sample.
        g.avg_distance_need_reset = true;
        g.update_current_distance(0.5, 0.25, t0);
        assert_eq!((g.current_error_smoothed(false, t0), g.current_error_frame_count()), (0.5, 1));
        let fresh = Guider::new(&mut Profile::default(), false, &mut ev);
        assert_eq!(fresh.current_error(false, t0), 100.0);
    }

    /// The bench guiding on its stored calibration, the star at `at`.
    fn guiding_at(at: Point) -> Bench {
        let mut b = Bench::new();
        b.star = at;
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        b
    }

    #[test]
    fn a_dither_is_caught_up_by_the_fast_recenter() {
        let mut b = guiding_at(Point::new(100.0, 100.0));
        let lock0 = b.guider.lock_position().unwrap();
        let err0 = b.call(|g, cx, _| g.current_error(false, cx.now_ms));
        b.ev.clear();
        // −20 px of RA: +20 px in x on this camera.
        assert!(b.call(|g, cx, ev| g.move_lock_position(Point::new(-20.0, 0.0), cx, ev)));
        let lock = b.guider.lock_position().unwrap();
        assert!((lock.x - lock0.x - 20.0).abs() < 1e-9 && (lock.y - lock0.y).abs() < 1e-9, "{lock:?}");
        let set = b.ev.iter().position(|e| *e == GuiderEvent::LockPositionSet(lock)).unwrap();
        let dithered = MountEvent::Debug("Mount: notify guiding dithered (20.0, 0.0)".into());
        assert_eq!(b.ev[set + 1], GuiderEvent::Mount(MountRole::Primary, dithered));
        // The guide log reads the lock before it moved.
        let info = format!("SET LOCK POSITION, new lock pos = {}, {}", fmt_f(lock0.x, 3), fmt_f(lock0.y, 3));
        assert_eq!(b.ev[set + 2], GuiderEvent::Info(info));
        let err = b.call(|g, cx, _| g.current_error(false, cx.now_ms));
        assert!((err - err0 - 20.0).abs() < 1e-9);
        assert!(b.guider.is_recentering());

        // Two straight moves of at most 0.7 search regions (10.5 px).
        b.ev.clear();
        let upd = b.frame();
        let [ScheduledMove::Offset { ofs, opts: MoveOpts::RECOVERY_MOVE }] = upd.moves[..] else {
            panic!("{:?}", upd.moves);
        };
        assert_eq!(ofs.mount_ofs.map(|m| m.x), Some(10.5));
        assert!((ofs.camera_ofs.x + 10.5).abs() < 1e-9 && ofs.camera_ofs.y.abs() < 1e-9);
        assert!(b.debug("dither recenter: remaining=(20.0,-0.0) step=(10.5,-0.0)"));
        let direct = MountEvent::Debug("Mount: notify direct move 10.50,-0.00".into());
        assert!(b.has(&GuiderEvent::Mount(MountRole::Primary, direct)));
        assert!(b.guider.is_recentering());
        b.ev.clear();
        b.frame();
        assert!(b.debug("dither recenter: remaining=(9.5,-0.0) step=(9.5,-0.0)"));
        assert!(!b.guider.is_recentering());

        // There: guide steps again, the averages restarted.
        let upd = b.frame();
        assert!(matches!(upd.moves[..], [ScheduledMove::Offset { opts: MoveOpts::GUIDE_STEP, .. }]));
        assert!(b.star.distance(&lock) < 0.5, "{:?} vs {lock:?}", b.star);
        assert_eq!(b.guider.current_error_frame_count(), 1);
    }

    #[test]
    fn a_dither_off_the_frame_is_reflected() {
        // +20 px would leave the lock 10 px from the right edge: −20 it is.
        let mut b = guiding_at(Point::new(170.0, 100.0));
        let lock0 = b.guider.lock_position().unwrap();
        b.ev.clear();
        assert!(b.call(|g, cx, ev| g.move_lock_position(Point::new(-20.0, 0.0), cx, ev)));
        assert!((b.guider.lock_position().unwrap().x - (lock0.x - 20.0)).abs() < 1e-9);
        let tries = |b: &Bench| {
            b.ev.iter().filter(|e| matches!(e, GuiderEvent::Debug(s) if s.starts_with("dither produces"))).count()
        };
        assert_eq!(tries(&b), 1);
        // Recentering heads back the reflected way.
        let upd = b.frame();
        let [ScheduledMove::Offset { ofs, .. }] = upd.moves[..] else { panic!() };
        assert_eq!(ofs.mount_ofs.map(|m| m.x), Some(-10.5));

        // No way fits: the one farthest from an edge, here the second.
        let mut b = guiding_at(Point::new(110.0, 100.0));
        let lock0 = b.guider.lock_position().unwrap();
        b.ev.clear();
        assert!(b.call(|g, cx, ev| g.move_lock_position(Point::new(-95.0, 0.0), cx, ev)));
        assert!((b.guider.lock_position().unwrap().x - (lock0.x - 95.0)).abs() < 1e-9);
        assert_eq!(tries(&b), 4);
    }

    #[test]
    fn a_lock_move_needs_a_calibrated_mount_and_a_length_to_recenter() {
        let mut b = Bench::new();
        b.select();
        assert!(!b.call(|g, cx, ev| g.move_lock_position(Point::new(5.0, 0.0), cx, ev)));
        assert!(b.debug("Error thrown from guider.cpp:851->No mount"));

        // A zero move still sets the lock (a settle without a dither).
        let mut b = guiding_at(Point::new(100.0, 100.0));
        assert!(b.call(|g, cx, ev| g.move_lock_position(Point::new(0.0, 0.0), cx, ev)));
        assert!(!b.guider.is_recentering());
        // Fast recenter off: the algorithms bring the star back.
        b.call(|g, cx, _| g.enable_fast_recenter(false, cx.profile));
        assert!(b.call(|g, cx, ev| g.move_lock_position(Point::new(3.0, 0.0), cx, ev)));
        assert!(!b.guider.is_recentering());
        assert_eq!(b.profile.get_i32(FAST_RECENTER_KEY, -1), 0);
        let upd = b.frame();
        assert!(matches!(upd.moves[..], [ScheduledMove::Offset { opts: MoveOpts::GUIDE_STEP, .. }]));
    }

    fn infos(b: &Bench) -> Vec<&str> {
        b.ev.iter()
            .filter_map(|e| match e {
                GuiderEvent::Info(s) => Some(s.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_lock_shifts_at_the_set_rate() {
        let mut b = guiding_at(Point::new(100.0, 100.0));
        let lock0 = b.guider.lock_position().unwrap();
        b.ev.clear();
        // 36 px/hr right, 72 px/hr up, on the camera's axes.
        b.call(|g, cx, ev| g.set_lock_shift_rate(Point::new(36.0, -72.0), ShiftUnits::Pixels, false, cx, ev));
        let rate = b.guider.lock_shift_rate().unwrap();
        assert!((rate.x - 0.01).abs() < 1e-15 && (rate.y + 0.02).abs() < 1e-15, "{rate:?}");
        assert!(b.debug("UpdateLockPosShiftCameraCoords: shift rate camera coords = 36.00,-72.00 pixels/hr"));
        assert!(b.debug("UpdateLockPosShiftCameraCoords: shift rate 0.01,-0.02 px/sec"));
        assert!(infos(&b).is_empty());
        b.call(|g, cx, ev| {
            g.enable_lock_shift(true, cx, ev);
        });
        assert!(b.debug("Multi-star mode disabled for comet tracking"));
        assert_eq!(infos(&b), ["LOCK SHIFT, enabled = 1 X,Y rate (36.00,-72.00) pixels/hr (36.00,-72.00) px/hr"]);
        assert!(b.guider.lock_shift_params().enabled);

        for _ in 0..10 {
            b.frame();
        }
        // 20 s later.
        let lock = b.guider.lock_position().unwrap();
        assert!((lock.x - lock0.x - 0.2).abs() < 1e-9 && (lock.y - lock0.y + 0.4).abs() < 1e-9, "{lock:?}");
        let line = format!("ShiftLockPos: new pos = {}, {} valid=1", fmt_f(lock.x, 2), fmt_f(lock.y, 2));
        assert!(b.debug(&line));
        // The star follows the lock.
        assert!(b.star.distance(&lock) < 0.5);
    }

    #[test]
    fn an_ephemeris_rate_turns_into_camera_pixels() {
        let mut b = Bench::new();
        b.pointing = Some(Pointing { dec: 60.0, reports: false });
        b.calibrate_scope();
        b.select();
        // Before the camera angle is known, the rate is only kept.
        b.call(|g, cx, ev| g.set_lock_shift_rate(Point::new(15.0, -30.0), ShiftUnits::Arcsec, true, cx, ev));
        b.call(|g, cx, ev| {
            g.enable_lock_shift(true, cx, ev);
        });
        assert_eq!(b.guider.lock_shift_rate(), None);
        assert!(infos(&b).is_empty());
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.ev.clear();
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        // RA × cos 60°, through the calibration (this camera turns RA by
        // 180° and mirrors Dec), over 2″/px and 3600 s.
        let rate = b.guider.lock_shift_rate().unwrap();
        assert!((rate.x + 7.5 / 7200.0).abs() < 1e-12 && (rate.y + 30.0 / 7200.0).abs() < 1e-12, "{rate:?}");
        assert!(b.debug("UpdateLockPosShiftCameraCoords: shift rate mount coords = 15.00,-30.00"));
        assert!(b.debug("UpdateLockPosShiftCameraCoords: after parity adjustment: 15.00,-30.00"));
        assert!(b.debug("UpdateLockPosShiftCameraCoords: RA shift rate adjusted for declination 60.0"));
        assert!(b.debug("UpdateLockPosShiftCameraCoords: shift rate camera coords = -7.50,-30.00 arcsec/hr"));
        assert!(b.debug("UpdateLockPosShiftCameraCoords: shift rate -0.001,-0.0042 px/sec"));
        assert_eq!(infos(&b), ["LOCK SHIFT, enabled = 1 RA,Dec rate (15.00,-30.00) arc-sec/hr (-3.75,-15.00) px/hr"]);
    }

    #[test]
    fn a_lock_shifted_off_the_frame_turns_the_shift_off() {
        let mut b = guiding_at(Point::new(170.0, 100.0));
        // Multi-star on while guiding: guiding restarts on a fresh pick.
        b.ev.clear();
        b.call(|g, cx, ev| {
            g.set_multi_star_mode(true, cx, ev);
        });
        assert!(b.debug("MultiStar mode enabled, AutoFind forced"));
        // The pick's lock, then the tracker's line, once.
        let lines = debug_lines(&b);
        let lock = lines.iter().position(|l| l.starts_with("setting lock position")).unwrap();
        assert_eq!(lines[lock + 1], "MultiStar: stabilizing after lock position change");
        assert_eq!(lines.iter().filter(|l| l.starts_with("MultiStar: stabilizing")).count(), 1);
        assert_eq!(b.guider.state(), GuiderState::Calibrated);
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        b.call(|g, cx, ev| g.set_lock_shift_rate(Point::new(36000.0, 0.0), ShiftUnits::Pixels, false, cx, ev));
        b.call(|g, cx, ev| {
            g.enable_lock_shift(true, cx, ev);
        });
        assert!(!b.guider.multistar().cfg.multi_star);

        // 20 px in the next 2 s: past the valid area.
        b.ev.clear();
        b.frame();
        let at = |e: &GuiderEvent| b.ev.iter().position(|x| x == e).unwrap();
        let limit = at(&GuiderEvent::LockShiftLimitReached);
        let GuiderEvent::Alert { kind: AlertKind::Warning, msg, button: None } = &b.ev[limit + 1] else { panic!() };
        assert!(msg.starts_with("Shifted lock"));
        assert!(!b.guider.lock_shift_params().enabled);
        // The user's multi-star mode is back, which re-selects the star and
        // starts guiding again within the frame.
        assert!(b.guider.multistar().cfg.multi_star);
        assert!(at(&GuiderEvent::GuidingStopped) > limit);
        assert!(at(&GuiderEvent::GuidingStarted) > at(&GuiderEvent::GuidingStopped));
        assert!(b.debug("Comet tracking disabled, user's multi-star mode restored"));
        assert!(infos(&b).contains(&"LOCK SHIFT, enabled = 0 "));
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        assert!(b.guider.lock_position().unwrap().distance(&b.star) < 0.5);
    }

    #[test]
    fn measurement_mode_hands_the_frames_to_the_assistant() {
        let mut b = Bench::new();
        b.select();
        // Not guiding: nothing.
        b.guider.enable_measurement_mode(true);
        assert!(!b.guider.measurement_mode);

        let mut b = guiding_at(Point::new(100.0, 100.0));
        b.guider.enable_measurement_mode(true);
        b.ev.clear();
        let upd = b.frame();
        assert!(upd.moves.is_empty());
        let star = b.guider.primary_star().position();
        assert!(b.has(&GuiderEvent::BacklashStep(star)));
        // Guiding ends: the measurement fails.
        b.call(|g, cx, ev| g.update_guide_state(None, true, cx, ev));
        assert!(b.has(&GuiderEvent::BacklashError));
        assert!(!b.guider.measurement_mode);
    }

    #[test]
    fn a_dec_uncalibrated_scope_is_not_calibrated() {
        let mut b = Bench::new();
        let cal = Calibration { y_rate: CALIBRATION_RATE_UNCALIBRATED, is_valid: true, ..Calibration::default() };
        b.scope.set_calibration(&cal, "T", &mut b.profile, &mut Vec::new());
        assert!(!GuiderMount::is_calibrated(&b.scope));
    }

    /// AutoFind sets the saturation level from the bit depth, and the frame
    /// AutoSelect runs from SELECTING finds the star with it: a flat top
    /// under 65535 is no longer "Saturated" as the flat-top test has it.
    #[test]
    fn auto_selects_frame_finds_the_star_with_autofinds_saturation_level() {
        let mut b = Bench::new();
        let mut img = (*render(Some(b.star), 40000.0, 1)).clone();
        for v in img.data.iter_mut() {
            *v = (*v).min(30000);
        }
        let flat = Star::find(&img, 15, 100.0, 100.0, FindMode::Centroid, 1.5, 20.0, 0);
        assert_eq!(flat.result, FindResult::Saturated);
        b.call(|g, cx, ev| g.update_guide_state(Some(Arc::new(img)), false, cx, ev));
        b.ev.clear();
        let (star, upd) = b.call(|g, cx, ev| g.auto_select(None, cx, ev));
        assert_eq!(upd.saturation_adu, Some(65535));
        assert_eq!(star.map(|s| s.result), Some(FindResult::Ok));
        let looping = b.ev.iter().find_map(|e| match e {
            GuiderEvent::LoopingExposures { star, .. } => Some(*star),
            _ => None,
        });
        assert_eq!(looping.flatten().map(|s| s.result), Some(FindResult::Ok));
        // Not "m=615965 SNR=551.2 Saturated".
        assert!(b.debug("UpdateGuideState exits: m=615965 SNR=551.2"));
    }

    /// The status and the star returned are the primary as AutoSelect's own
    /// frame left it: found again in the user's star find mode, here the
    /// peak pixel, while AutoSelect's pick is a centroid.
    #[test]
    fn auto_select_reports_the_star_its_frame_found_again() {
        let mut b = Bench::new();
        b.star = Point::new(100.3, 99.6);
        b.fp.mode = FindMode::Peak;
        b.frame();
        b.ev.clear();
        let star = b.call(|g, cx, ev| g.auto_select(None, cx, ev)).0.unwrap();
        assert!(b.debug("setting lock position to (100.30, 99.60)"));
        assert_eq!((star.x, star.y), (100.0, 100.0));
        assert_eq!(star, *b.guider.primary_star());
        assert_eq!(b.guider.lock_position(), Some(Point::new(100.0, 100.0)));
        assert_eq!(b.ev.last(), Some(&GuiderEvent::Status("Auto-selected star at (100.0, 100.0)".into())));
    }

    /// A new profile gets auto-restore as the mount can do it; one with it
    /// off is offered it once, with the button that turns it on.
    #[test]
    fn auto_restore_is_offered_once_with_its_button() {
        let mut b = Bench::new();
        b.pointing = Some(Pointing { dec: 0.0, reports: true });
        b.profile.set_i32("/AutoLoadCalibration", 0);
        b.calibrate_scope();
        b.select();
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.ev.clear();
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        let offer = GuiderEvent::Alert {
            msg: "Do you want to automatically restore this calibration whenever the profile is used?".into(),
            kind: AlertKind::Info,
            button: Some(AlertButton::AutoRestoreCalibration),
        };
        assert!(b.has(&offer));
        assert_eq!(AlertButton::AutoRestoreCalibration.label(), "Auto-restore");
        assert_eq!(b.profile.get_i32("/AlreadyAskedCalibAutoload", 0), 1);
        assert!(!b.ev.iter().any(|e| matches!(e, GuiderEvent::SetAutoLoadCalibration(_))));

        // Guiding again from the stored calibration: asked already.
        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.ev.clear();
        b.frame();
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        assert!(!b.ev.iter().any(|e| matches!(e, GuiderEvent::Alert { .. })));

        // A new profile: on, since the mount reports where it points.
        b.profile.i.remove("/AutoLoadCalibration");
        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        b.ev.clear();
        b.frame();
        assert!(b.has(&GuiderEvent::SetAutoLoadCalibration(true)));
    }

    /// A profile switch re-selects the star when multi-star turns on.
    /// Guiding, the frame is kept: AutoSelect finds the star, sets the
    /// saturation level and guiding restarts. Selected, the frame is cleared
    /// and AutoSelect has no image.
    #[test]
    fn a_profile_switch_re_selects_on_the_kept_frame_only_while_guiding() {
        let mut b = guiding_at(Point::new(100.0, 100.0));
        b.profile.set_i32(keys::MULTISTAR_ENABLED, 1);
        b.ev.clear();
        let upd = b.call(|g, cx, ev| g.load_profile_settings(cx, ev));
        assert_eq!(upd.saturation_adu, Some(65535));
        assert!(b.guider.multistar().cfg.multi_star);
        assert!(b.debug("MultiStar mode enabled, AutoFind forced"));
        assert!(b.has(&GuiderEvent::GuidingStopped));
        assert_eq!(b.guider.state(), GuiderState::Calibrated);
        assert!(!b.guider.current_image().data.is_empty());

        let mut b = Bench::new();
        b.select();
        b.profile.set_i32(keys::MULTISTAR_ENABLED, 1);
        b.ev.clear();
        let upd = b.call(|g, cx, ev| g.load_profile_settings(cx, ev));
        assert_eq!(upd, GuideUpdate::default());
        assert!(b.guider.current_image().data.is_empty());
        assert!(b.debug("Error thrown from guider_multistar.cpp:457->No Current Image"));
        assert!(b.debug("MultiStar mode enabled"));
        assert_eq!(b.guider.state(), GuiderState::Selected);
        assert!(b.guider.current_position().is_none());
    }

    /// A polar tool that fails on a SELECTED frame sends the guider back to
    /// looping, with its own message and throw line.
    #[test]
    fn a_failed_polar_tool_drops_the_selection() {
        let cases = [
            (PaFailure::StaticPa, "1413", "Static PA rotation failed"),
            (PaFailure::PolarDrift, "1419", "Polar Drift PA drift failed"),
        ];
        for (failure, line, msg) in cases {
            let mut b = Bench::new();
            b.select();
            assert_eq!(b.frame().pa_tools, PaTools::UpdateState);
            b.ev.clear();
            b.call(|g, cx, ev| g.pa_tool_failed(failure, cx, ev));
            assert_eq!(b.guider.state(), GuiderState::Selecting);
            assert_eq!(
                b.ev,
                [
                    GuiderEvent::Debug("Changing from state SELECTED to UNINITIALIZED".into()),
                    GuiderEvent::LockPositionLost,
                    GuiderEvent::Debug("guider state => SELECTING".into()),
                    GuiderEvent::Debug(format!("Error thrown from guider.cpp:{line}->{msg}")),
                    GuiderEvent::Status(msg.into()),
                ]
            );
        }
    }

    /// An event as the C++ harness prints it; `None` for what it stubs out
    /// (the tracker's transform lines, the lock shift's rate, the pointing
    /// adjustments, the auto-exposure step of `UpdateCurrentPosition`).
    fn harness_line(e: &GuiderEvent) -> Option<String> {
        let role = |r: &MountRole| if *r == MountRole::Primary { "primary" } else { "secondary" };
        Some(match e {
            GuiderEvent::AdjustAutoExposure(_) | GuiderEvent::ResetAutoExposure => return None,
            GuiderEvent::ImageReplaced(_) | GuiderEvent::ImageLog(_) => return None,
            GuiderEvent::Debug(s) if s.starts_with("CameraToMount") => return None,
            GuiderEvent::Debug(s) if s.starts_with("UpdateLockPosShiftCameraCoords") => return None,
            GuiderEvent::Mount(_, MountEvent::Debug(s)) if s.starts_with("MountToCamera") => return None,
            GuiderEvent::Debug(s) if s.starts_with("UpdateImageDisplay:") => "DEBUG: UpdateImageDisplay".into(),
            GuiderEvent::Debug(s) => format!("DEBUG: {s}"),
            GuiderEvent::Mount(r, MountEvent::Debug(s)) => format!("MOUNT {}: {s}", role(r)),
            GuiderEvent::PointingAdjusted(..) => return None,
            GuiderEvent::StartCalibration(r) => format!("StartCalibration {}", role(r)),
            GuiderEvent::Status(s) => format!("STATUS: {s}"),
            GuiderEvent::Alert { msg, button: Some(b), .. } => format!("ALERT: {msg} [{}]", b.label()),
            GuiderEvent::SetAutoLoadCalibration(on) => format!("SetAutoLoadCalibration {}", *on as i32),
            other => format!("{other:?}"),
        })
    }

    fn flush(b: &mut Bench, out: &mut Vec<String>) {
        out.extend(b.ev.drain(..).filter_map(|e| harness_line(&e)));
    }

    /// A frame of the star where it was selected; its moves are not made.
    fn still_frame(b: &mut Bench, n: u32, out: &mut Vec<String>) {
        out.push(format!("== frame {n}"));
        let img = render(Some(b.star), b.amp, n);
        b.call(|g, cx, ev| g.update_guide_state(Some(img), false, cx, ev));
        flush(b, out);
    }

    /// SELECTED, then the scripted mounts in place of the scope.
    fn selected_on(primary: FakeMount, secondary: Option<FakeMount>) -> Bench {
        let mut b = Bench::new();
        b.select();
        b.fakes = Some((primary, secondary));
        b.ev.clear();
        b
    }

    fn start(b: &mut Bench, label: &str, out: &mut Vec<String>) {
        out.push(format!("== {label}"));
        b.call(|g, cx, ev| g.start_guiding(cx, ev));
        flush(b, out);
    }

    /// The state machine against PHD2's own, guider.cpp compiled over the
    /// same scripted mounts (testdata/guider_phd2_state_machine.txt): an AO
    /// calibrated with the scope behind it not, whose calibration begins
    /// twice; the same failing; an AO failing alone; a sticky lock, the
    /// auto-restore offer asked once, then a new profile.
    #[test]
    fn the_state_machine_matches_phd2() {
        const GOLDEN: &str = include_str!("testdata/guider_phd2_state_machine.txt");
        let mut out = Vec::new();
        let ao = || FakeMount::new(MountClass::StepGuider, true);

        out.push("#### secondary".into());
        let mut scope = FakeMount::new(MountClass::Scope, false);
        scope.cal_frames = 2;
        let mut b = selected_on(ao(), Some(scope));
        start(&mut b, "start", &mut out);
        // The quirk: CALIBRATING_PRIMARY begins the secondary's calibration
        // and stays there; the next frame begins it again.
        assert_eq!(b.guider.state(), GuiderState::CalibratingPrimary);
        for n in 1..=4 {
            still_frame(&mut b, n, &mut out);
        }
        assert_eq!(b.guider.state(), GuiderState::Guiding);
        let begun = out.iter().filter(|l| *l == "StartCalibration secondary").count();
        assert_eq!(begun, 2);

        out.push("#### secondary fails".into());
        let mut scope = FakeMount::new(MountClass::Scope, false);
        (scope.cal_frames, scope.fail_at) = (5, Some(1));
        let mut b = selected_on(ao(), Some(scope));
        start(&mut b, "start", &mut out);
        for n in 1..=3 {
            still_frame(&mut b, n, &mut out);
        }

        out.push("#### ao fails".into());
        let mut lone = FakeMount::new(MountClass::StepGuider, false);
        lone.fail_at = Some(0);
        let mut b = selected_on(lone, None);
        start(&mut b, "start", &mut out);
        still_frame(&mut b, 1, &mut out);

        out.push("#### sticky".into());
        let mut b = selected_on(FakeMount::new(MountClass::Scope, true), None);
        b.pointing = Some(Pointing { dec: 0.0, reports: true });
        b.profile.set_i32("/AutoLoadCalibration", 0);
        b.guider.set_lock_pos_is_sticky(true);
        b.call(|g, cx, ev| g.set_lock_position(Point::new(101.5, 98.25), cx, ev));
        b.ev.clear();
        start(&mut b, "start", &mut out);
        still_frame(&mut b, 1, &mut out);
        still_frame(&mut b, 2, &mut out);
        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        start(&mut b, "stop, start", &mut out);
        still_frame(&mut b, 3, &mut out);
        b.profile.i.remove("/AutoLoadCalibration");
        b.call(|g, cx, ev| g.stop_guiding(cx, ev));
        start(&mut b, "stop, start, new profile", &mut out);
        still_frame(&mut b, 4, &mut out);
        out.push(format!("asked {}", b.profile.get_i32("/AlreadyAskedCalibAutoload", -1)));

        let golden: Vec<&str> = GOLDEN.lines().collect();
        for (i, (got, want)) in out.iter().zip(&golden).enumerate() {
            assert_eq!(got, want, "line {}", i + 1);
        }
        assert_eq!(out.len(), golden.len());
    }
}
