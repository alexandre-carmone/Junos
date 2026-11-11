//! What the outside asks of the engine: one [`Command`] per PHD2 RPC that
//! changes something (the read-only ones answer from the
//! [`Snapshot`](super::event::Snapshot)), the junos UI's extras, and the
//! equipment the daemon connected ([`Gear`]). Each command answers like
//! PHD2's event-server handler once its parameters have parsed: a JSON
//! result, or PHD2's error code and message. Parsing the parameters (the
//! `-32602` errors) is the server's.
//!
//! Ported from PHD2 src/event_server.cpp (the method handlers' errors), src/gear_dialog.cpp (the equipment) — BSD-3-Clause, see THIRD_PARTY.md

use std::fmt;

use serde_json::Value;

use super::algo::{AlgoKind, Axis};
use super::assistant::RecKind;
use super::calibration::Calibration;
use super::camera::GuideCamera;
use super::control::{DitherMode, SettleParams};
use super::guider::ShiftUnits;
use super::log::image_log::ImageLoggerSettings;
use super::mount::{GuideOutput, PointingSource};
use super::scope::DecGuideMode;
use super::static_pa_win::StaticPaSet;
use super::stepguider::StepGuiderDevice;
use super::traits::{Dir, PierSide, Point, Rect};

/// JSON-RPC's "invalid request" (`JSONRPC_INVALID_REQUEST`), which PHD2
/// answers a refused `set_lock_position` with.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC's "invalid params" (`JSONRPC_INVALID_PARAMS`).
pub const INVALID_PARAMS: i32 = -32602;

/// A refused command: PHD2's `jrpc_error(code, message)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdError {
    pub code: i32,
    pub message: String,
}

impl CmdError {
    pub fn new(code: i32, message: impl Into<String>) -> CmdError {
        CmdError { code, message: message.into() }
    }
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// What a command answers: PHD2's `jrpc_result`, most often `0`.
pub type Reply = Result<Value, CmdError>;

/// PHD2's `WHICH_MOUNT` (`mount`, `ao`, `both`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Mount,
    Ao,
    Both,
}

/// Where the scope's guide pulses go.
pub enum ScopeOutput {
    /// The mount's own pulse guiding (an INDI mount).
    Mount(Box<dyn GuideOutput + Send>),
    /// "On Camera": the camera's ST-4 port (the gear simulator's rig).
    OnCamera,
    /// "On AO": the AO's ST-4 port, its own timed guide properties.
    OnAo(Box<dyn GuideOutput + Send>),
}

/// The mount the guide pulses go to, as the daemon connected it.
pub struct ScopeGear {
    /// The equipment name (`INDI Mount [<dev>]`, `On Camera`, …).
    pub name: String,
    pub output: ScopeOutput,
    /// What the mount reports about where it points, when it reports
    /// anything; the gear simulator's mount stands in for "On Camera".
    pub pointing: Option<Box<dyn PointingSource + Send>>,
}

/// A rotator: PHD2's `Rotator`, reached over `ABS_ROTATOR_ANGLE`.
pub trait Rotator: Send {
    fn name(&self) -> String;
    fn is_connected(&self) -> bool;
    /// `Rotator::RotatorPosition`, degrees; `None` when unknown.
    fn position_deg(&self) -> Option<f64>;
}

/// Where the mount points when nothing reports it (the junos UI's manual
/// pointing): a fixed position, pier side and site, no slewing and no guide
/// rates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ManualPointing {
    pub ra_hours: f64,
    pub dec_deg: f64,
    pub pier_side: PierSide,
    /// Local sidereal time, hours.
    pub lst_hours: f64,
}

impl PointingSource for ManualPointing {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        Some((self.ra_hours, self.dec_deg, self.lst_hours))
    }
    fn pier_side(&self) -> PierSide {
        self.pier_side
    }
    fn guide_rates(&self) -> Option<(f64, f64)> {
        None
    }
    fn is_slewing(&self) -> bool {
        false
    }
    fn can_report_position(&self) -> bool {
        true
    }
}

/// The equipment the daemon connected (`set_connected true`, the INDI
/// connect being the daemon's: it polls up to 30 s per device). Each
/// device is connected; a role left `None` has no device.
#[derive(Default)]
pub struct Gear {
    pub camera: Option<Box<dyn GuideCamera>>,
    pub scope: Option<ScopeGear>,
    /// The aux mount: a pointing source only.
    pub aux_mount: Option<(String, Box<dyn PointingSource + Send>)>,
    pub ao: Option<Box<dyn StepGuiderDevice + Send>>,
    pub rotator: Option<Box<dyn Rotator>>,
}

impl fmt::Debug for Gear {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Gear")
            .field("camera", &self.camera.as_ref().map(|c| c.name()))
            .field("scope", &self.scope.as_ref().map(|s| &s.name))
            .field("aux_mount", &self.aux_mount.as_ref().map(|a| &a.0))
            .field("ao", &self.ao.as_ref().map(|a| a.name()))
            .field("rotator", &self.rotator.as_ref().map(|r| r.name()))
            .finish()
    }
}

/// `capture_single_frame`'s parameters once parsed; `None` takes PHD2's
/// default (the exposure setting, the camera's binning and gain).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SingleFrame {
    pub exposure_ms: Option<i32>,
    pub binning: Option<i32>,
    pub gain: Option<i32>,
    /// Empty: the full frame.
    pub subframe: Rect,
    /// Where to save it: an absolute path that does not exist yet.
    pub path: Option<String>,
    pub save: bool,
}

/// A dark-library or bad-pixel-map build (the Darks dialog's Start).
#[derive(Clone, Debug, PartialEq)]
pub enum DarksBuildKind {
    /// Master darks of these exposures (ms), `count` frames each; a new
    /// library forgets the darks held first. `note` is each file's
    /// `USERNOTE`.
    Library { exposures_ms: Vec<i32>, count: i32, new_library: bool, note: String },
    /// The defect map's master dark: `count` frames of `exp_s` seconds,
    /// saved with its median-filtered copy for the map to be built from.
    DefectMap { exp_s: i32, count: i32, note: String },
}

/// A change to the lock-position bookmarks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BookmarkOp {
    /// "Bookmark Lock Position".
    Lock,
    /// "Bookmark Star Position".
    Star,
    /// Shift-click: the bookmark there goes, or one is added.
    Toggle(Point),
    /// The bookmark there goes.
    Delete(Point),
    /// "Delete all" (PHD2 asks first; the UI does).
    Clear,
}

/// The Calibration Assistant's controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CalAssistCmd {
    Open,
    /// The target: meridian offset (degrees), Dec, east of the meridian.
    Set { offset: Option<i32>, dec: Option<i32>, east: Option<bool> },
    /// "Save custom" (with its east/west-only box), "Load custom",
    /// "Restore defaults".
    SaveCustom { single_side: bool },
    LoadCustom,
    Restore,
    Slew,
    Calibrate,
    /// The slew's Cancel.
    Stop,
    Close,
}

/// The Drift Align window's controls.
#[derive(Clone, Debug, PartialEq)]
pub enum DriftAlignCmd {
    Open,
    /// Slew to the phase's location; the fields (degrees: RA offset from
    /// the meridian, Dec) when sent.
    Slew { ra: Option<f64>, dec: Option<f64> },
    Drift,
    Adjust,
    /// "> Altitude" / "< Azimuth".
    Phase,
    /// The phase's adjustment notes.
    Notes(String),
    Close,
}

/// Everything that changes the engine. The read-only RPCs answer from the
/// snapshot, without the engine thread.
pub enum Command {
    // ---------------------------------------------------------- PHD2 RPCs
    /// `loop`.
    Loop,
    /// `stop_capture`.
    StopCapture,
    /// `guide`: the event server always asks for the sticky lock.
    Guide { settle: SettleParams, recalibrate: bool, roi: Option<Rect> },
    /// `dither`: `amount` is taken as its absolute value.
    Dither { amount: f64, ra_only: bool, settle: SettleParams },
    /// `set_paused`: `full` is the `"full"` type.
    SetPaused { paused: bool, full: bool },
    /// `find_star`.
    FindStar { roi: Option<Rect> },
    /// `deselect_star`.
    DeselectStar,
    /// `set_lock_position`: `exact` false locks on the star near (x, y).
    SetLockPosition { x: f64, y: f64, exact: bool },
    /// `clear_calibration`.
    ClearCalibration(Which),
    /// `flip_calibration`.
    FlipCalibration,
    /// `set_exposure`, ms.
    SetExposure(i32),
    /// `set_dec_guide_mode`.
    SetDecGuideMode(DecGuideMode),
    /// `set_guide_output_enabled`, on pMount (the AO when there is one).
    SetGuideOutputEnabled(bool),
    /// `set_algo_param` on pMount's axis.
    SetAlgoParam { axis: Axis, name: String, value: f64 },
    /// `guide_pulse`: a negative amount goes the other way. `which` both
    /// is refused, as in PHD2.
    GuidePulse { amount: i32, dir: Dir, which: Which },
    /// `capture_single_frame`.
    CaptureSingleFrame(SingleFrame),
    /// `set_lock_shift_enabled`.
    SetLockShiftEnabled(bool),
    /// `set_lock_shift_params`: per hour in `units`, along RA/Dec when
    /// `mount_coords`.
    SetLockShiftParams { rate: Point, units: ShiftUnits, mount_coords: bool },
    /// `set_variable_delay_settings`, ms (PHD2 truncates the seconds first).
    SetVariableDelay { enabled: bool, short_ms: i32, long_ms: i32 },
    /// `set_limit_frame`; `None` clears it.
    SetLimitFrame(Option<Rect>),
    /// `set_cooler_state`.
    SetCoolerState(bool),
    /// `save_image`: the current frame into a new `save_image_*` file (no
    /// extension, as `CreateTempFileName` makes it) of the default folder.
    SaveImage,

    // --------------------------------------------------------- equipment
    /// The devices `set_connected true` connected: the engine runs what
    /// PHD2 does after connecting (auto-loading darks, the defect map and
    /// the calibration, …).
    Attach(Gear),
    /// `set_connected false` disconnected everything.
    Detach,
    /// A profile switch (`set_profile`, the daemon's): the settings read
    /// again.
    ProfileChanged,
    /// The 1 s tick: the Guiding Assistant's countdown.
    Tick,
    /// The daemon connects the gear off the engine thread (`set_connected
    /// true`): `true` before it starts, refused while capturing as
    /// `ConnectAll` is; `false` once the gear is attached or failed. In
    /// between, `loop` and `guide` are refused.
    Connecting(bool),
    /// The device behind the scope or the AO dropped (the INDI server or
    /// the driver went): its mount is disconnected, as `ScopeINDI` and the
    /// INDI AO do from their INDI thread.
    DeviceLost(Which),

    // ---------------------------------------------------- the junos UI's
    /// A click on the frame: lock on the star there (`select_star`).
    SelectStar { x: f64, y: f64 },
    /// `set_calibration_data`: a calibration typed in or restored.
    SetCalibrationData { which: Which, cal: Calibration },
    /// Nudge the lock by (dx, dy) camera px (the Adjust Lock Position
    /// tool's arrows), save it, restore the saved one.
    NudgeLock { dx: f64, dy: f64 },
    SaveLock,
    RestoreLock,
    /// The Bookmarks menu and shift-click.
    Bookmark(BookmarkOp),
    /// `set_algorithm`: the axis' algorithm on pMount.
    SetAlgorithm { axis: Axis, kind: AlgoKind },
    /// `reset_algo_params`: the Brain's Reset.
    ResetAlgoParams { axis: Axis },
    /// `set_dither_mode`.
    SetDitherMode { mode: DitherMode, ra_only: bool, scale: f64 },
    /// `set_auto_exposure`: min/max ms, target SNR.
    SetAutoExposure { min_ms: i32, max_ms: i32, target_snr: f64 },
    /// `set_image_logger`: the settings dialog's fields, taken as its OK
    /// does ([`ImageLoggerSettings::from_dialog`]).
    SetImageLogger(ImageLoggerSettings),
    /// `set_manual_pointing`; `None` drops it.
    SetManualPointing(Option<ManualPointing>),
    /// The Guiding Assistant window: open (it starts measuring at once
    /// when guiding, as PHD2's), Start, Stop, the countdown's Cancel
    /// (`accept`: PHD2's Ctrl-click, which keeps the samples), the backlash
    /// check box, an Apply, a past run, close.
    GaOpen,
    GaStart,
    GaStop,
    GaEndWait { accept: bool },
    GaSetMeasureBacklash(bool),
    GaApply(RecKind),
    GaReview(String),
    GaClose,
    /// The Darks dialog's Start and Stop; the Darks menu's "Use Dark
    /// Library" and "Use Bad-pixel Map".
    DarksBuild(DarksBuildKind),
    DarksStop,
    DarksUse(bool),
    DefectMapUse(bool),
    /// The Polar Drift tool: open (looping starts), Start, Stop, close.
    PolarDriftOpen,
    PolarDriftStart,
    PolarDriftStop,
    PolarDriftClose,
    /// The star-cross test's Start (pulses per direction, pulse ms; the
    /// dialog's values when `None`) and Stop.
    StarCrossStart { pulse_count: Option<i32>, pulse_size: Option<i32> },
    StarCrossStop,
    /// The Static Polar Alignment window: open (looping starts), Rotate /
    /// "Get first position", "Get second / third position", Clear, its
    /// choices, close.
    StaticPaOpen,
    StaticPaRotate,
    StaticPaMark(i32),
    StaticPaClear,
    StaticPaSet(StaticPaSet),
    StaticPaClose,
    /// The comet tool's Start and Stop of training.
    CometTrainStart,
    CometTrainStop,
    CalAssistant(CalAssistCmd),
    /// The Meridian Flip Calibration Tool: open, Next / Calibrate / Apply,
    /// Start over, close.
    PierFlipOpen,
    PierFlipNext,
    PierFlipRestart,
    PierFlipClose,
    DriftAlign(DriftAlignCmd),
    /// The Refine Bad-pixel Map window: the pixel counts at these
    /// aggressiveness values (0..=100), and its Apply.
    BpmSet { cold: i32, hot: i32 },
    BpmSave { cold: i32, hot: i32 },
}

impl Command {
    /// The method name, for the debug log.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Loop => "loop",
            Command::StopCapture => "stop_capture",
            Command::Guide { .. } => "guide",
            Command::Dither { .. } => "dither",
            Command::SetPaused { .. } => "set_paused",
            Command::FindStar { .. } => "find_star",
            Command::DeselectStar => "deselect_star",
            Command::SetLockPosition { .. } => "set_lock_position",
            Command::ClearCalibration(_) => "clear_calibration",
            Command::FlipCalibration => "flip_calibration",
            Command::SetExposure(_) => "set_exposure",
            Command::SetDecGuideMode(_) => "set_dec_guide_mode",
            Command::SetGuideOutputEnabled(_) => "set_guide_output_enabled",
            Command::SetAlgoParam { .. } => "set_algo_param",
            Command::GuidePulse { .. } => "guide_pulse",
            Command::CaptureSingleFrame(_) => "capture_single_frame",
            Command::SetLockShiftEnabled(_) => "set_lock_shift_enabled",
            Command::SetLockShiftParams { .. } => "set_lock_shift_params",
            Command::SetVariableDelay { .. } => "set_variable_delay_settings",
            Command::SetLimitFrame(_) => "set_limit_frame",
            Command::SetCoolerState(_) => "set_cooler_state",
            Command::SaveImage => "save_image",
            Command::Attach(_) => "set_connected",
            Command::Detach => "set_connected",
            Command::ProfileChanged => "set_profile",
            Command::Tick => "tick",
            Command::Connecting(_) => "set_connected",
            Command::DeviceLost(_) => "device_lost",
            Command::SelectStar { .. } => "select_star",
            Command::SetCalibrationData { .. } => "set_calibration_data",
            Command::NudgeLock { .. } => "nudge_lock",
            Command::SaveLock => "save_lock",
            Command::RestoreLock => "restore_lock",
            Command::Bookmark(BookmarkOp::Lock | BookmarkOp::Star) => "bookmark_add",
            Command::Bookmark(BookmarkOp::Toggle(_)) => "bookmark_toggle",
            Command::Bookmark(BookmarkOp::Delete(_)) => "bookmark_delete",
            Command::Bookmark(BookmarkOp::Clear) => "bookmarks_clear",
            Command::SetAlgorithm { .. } => "set_algorithm",
            Command::ResetAlgoParams { .. } => "reset_algo_params",
            Command::SetDitherMode { .. } => "set_dither_mode",
            Command::SetAutoExposure { .. } => "set_auto_exposure",
            Command::SetImageLogger(_) => "set_image_logger",
            Command::SetManualPointing(_) => "set_manual_pointing",
            Command::GaOpen => "ga_open",
            Command::GaStart => "ga_start",
            Command::GaStop => "ga_stop",
            Command::GaEndWait { .. } => "ga_end_wait",
            Command::GaSetMeasureBacklash(_) => "ga_set_measure_backlash",
            Command::GaApply(_) => "ga_apply",
            Command::GaReview(_) => "ga_review",
            Command::GaClose => "ga_close",
            Command::DarksBuild(_) => "darks_build",
            Command::DarksStop => "darks_stop",
            Command::DarksUse(_) => "darks_use",
            // `darks_use {dark, defect}` carries both menu items.
            Command::DefectMapUse(_) => "darks_use",
            Command::PolarDriftOpen => "pa_drift_open",
            Command::PolarDriftStart => "pa_drift_start",
            Command::PolarDriftStop => "pa_drift_stop",
            Command::PolarDriftClose => "pa_drift_close",
            Command::StarCrossStart { .. } => "starcross_start",
            Command::StarCrossStop => "starcross_stop",
            Command::StaticPaOpen => "pa_static_start",
            Command::StaticPaRotate => "pa_static_rotate",
            Command::StaticPaMark(_) => "pa_static_mark",
            Command::StaticPaClear => "pa_static_clear",
            Command::StaticPaSet(_) => "pa_static_set",
            Command::StaticPaClose => "pa_static_stop",
            Command::CometTrainStart => "comet_train_start",
            Command::CometTrainStop => "comet_train_stop",
            Command::DriftAlign(c) => match c {
                DriftAlignCmd::Open => "drift_align_start",
                DriftAlignCmd::Slew { .. } => "drift_align_slew",
                DriftAlignCmd::Drift => "drift_align_drift",
                DriftAlignCmd::Adjust => "drift_align_adjust",
                DriftAlignCmd::Phase => "drift_align_phase",
                DriftAlignCmd::Notes(_) => "drift_align_notes",
                DriftAlignCmd::Close => "drift_align_stop",
            },
            Command::BpmSet { .. } => "bpm_set",
            Command::BpmSave { .. } => "bpm_save",
            Command::PierFlipOpen => "pierflip_start",
            Command::PierFlipNext => "pierflip_next",
            Command::PierFlipRestart => "pierflip_restart",
            Command::PierFlipClose => "pierflip_stop",
            Command::CalAssistant(c) => match c {
                CalAssistCmd::Open => "cal_assistant_start",
                CalAssistCmd::Set { .. } => "cal_assistant_set",
                CalAssistCmd::SaveCustom { .. } | CalAssistCmd::LoadCustom | CalAssistCmd::Restore => {
                    "cal_assistant_location"
                }
                CalAssistCmd::Slew => "cal_assistant_slew",
                CalAssistCmd::Calibrate => "cal_assistant_calibrate",
                CalAssistCmd::Stop => "cal_assistant_cancel",
                CalAssistCmd::Close => "cal_assistant_stop",
            },
        }
    }
}
