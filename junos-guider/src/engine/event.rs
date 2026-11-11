//! What the engine tells the outside: the 27 events of PHD2's event server
//! with their fields ([`EngineEvent`]), the frame for the display, the
//! guide-log lines for the relay, the status line — and the state the
//! read-only RPCs answer from without the engine thread ([`Snapshot`]).
//!
//! The events carry the engine's own values; the event server formats them
//! (`%.3f` times, `%.2f` points, …). The guide log and the debug log are
//! written by the engine itself.
//!
//! Ported from PHD2 src/event_server.cpp (the events and the state the methods read) — BSD-3-Clause, see THIRD_PARTY.md

use std::sync::Arc;

use serde_json::Value;

use super::camera::CoolerStatus;
use super::guider::{AlertButton, AppState, FrameDroppedInfo, GuiderState, LockShiftParams, PauseType};
use super::image::UsImage;
use super::mount::{AlertKind, CalibrationStepInfo, GuideStepInfo};
use super::multistar::FrameErrorInfo;
use super::scope::DecGuideMode;
use super::star::Star;
use super::traits::{Parity, Point, Rect};

/// One event, in PHD2's order. `mount` is the mount's equipment name, the
/// event's `Mount` field.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// `Version`: sent by the server on connect, never by the engine.
    Version,
    /// `LockPositionSet`.
    LockPositionSet(Point),
    /// `LockPositionLost`.
    LockPositionLost,
    /// `LockPositionShiftLimitReached`.
    LockPositionShiftLimitReached,
    /// `StarSelected`.
    StarSelected(Point),
    /// `StarLost`.
    StarLost(FrameDroppedInfo),
    /// `LoopingExposures`: the star when found, else the drop.
    LoopingExposures { frame: u32, star: Option<Star>, dropped: Option<FrameErrorInfo> },
    /// `LoopingExposuresStopped`.
    LoopingExposuresStopped,
    /// `StartCalibration`.
    StartCalibration { mount: String },
    /// `Calibrating`.
    Calibrating { mount: String, step: CalibrationStepInfo },
    /// `CalibrationComplete`. PHD2 never sends the AO's live
    /// (`stepguider.cpp:762` only logs it), only in the catch-up: the server
    /// drops the event when `ao` is set.
    CalibrationComplete { mount: String, ao: bool },
    /// `CalibrationFailed`.
    CalibrationFailed { mount: String, reason: String },
    /// `CalibrationDataFlipped`.
    CalibrationDataFlipped { mount: String },
    /// `StartGuiding`.
    StartGuiding,
    /// `GuidingStopped`.
    GuidingStopped,
    /// `Paused`.
    Paused,
    /// `Resumed`.
    Resumed,
    /// `GuideStep`: a move made, the scope's or the AO's (`step.is_ao`).
    GuideStep { mount: String, step: GuideStepInfo },
    /// `GuidingDithered`, RA/Dec px.
    GuidingDithered { dx: f64, dy: f64 },
    /// `SettleBegin`.
    SettleBegin,
    /// `Settling`.
    Settling { distance: f64, time: f64, settle_time: f64, star_locked: bool },
    /// `SettleDone`: `Status` 1 with an error, else 0.
    SettleDone { error: Option<String>, total_frames: i32, dropped_frames: i32 },
    /// `SingleFrameComplete`: `path` when the frame was saved.
    SingleFrameComplete { success: bool, error: Option<String>, path: Option<String> },
    /// `Alert`. The event server sends `Msg` and `Type`; the button and the
    /// "Don't show again" key are the junos UI's.
    Alert { msg: String, kind: AlertKind, button: Option<AlertButton>, dont_show: Option<&'static str> },
    /// `GuideParamChange`: the value as PHD2 typed it.
    GuideParamChange { name: String, value: Value },
    /// `ConfigurationChange`: the profile changed (debounced: one per
    /// command or frame).
    ConfigurationChange,
    /// `AppState`: sent by the server's catch-up only (PHD2 has no live
    /// caller), from the snapshot.
    AppState(AppState),

    /// The frame just processed, for the display and `get_star_image`.
    FrameReady(Arc<UsImage>),
    /// A guide-log line the relay shows (`guider_log`): the `INFO:` and
    /// calibration lines, without the newline.
    GuideLogLine(String),
    /// `pFrame->StatusMsg`: the status bar.
    Status(String),
}

/// One mount's calibration, as `get_calibration_data` reads it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalibrationView {
    /// The mount exists (PHD2's `TheScope()` / `TheAO()`).
    pub present: bool,
    pub name: String,
    pub connected: bool,
    /// `IsCalibrated()`, the scope's Dec check included.
    pub calibrated: bool,
    /// Radians, px/ms, as stored.
    pub x_angle: f64,
    pub x_rate: f64,
    pub x_parity: Parity,
    pub y_angle: f64,
    pub y_rate: f64,
    pub y_parity: Parity,
    /// Radians, or [`super::calibration::UNKNOWN_DECLINATION`].
    pub declination: f64,
    /// `GetAoMaxPos` on an AO (the catch-up's `Limit`), else (0, 0).
    pub ao_max_pos: (i32, i32),
    /// pMount's guide output (`GetGuidingEnabled`).
    pub guiding_enabled: bool,
}

/// One device of `get_current_equipment`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceView {
    pub name: String,
    pub connected: bool,
}

/// pMount's algorithm on one axis: `get_algo_param_names` and
/// `get_algo_param`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlgoView {
    /// `GetGuideAlgorithmClassName` (`algorithmName`).
    pub class_name: String,
    /// The parameters by their event-server names, in PHD2's order.
    pub params: Vec<(String, f64)>,
}

/// Everything the read-only RPCs and the relay's `guider_state` read,
/// taken after each frame and each command.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// `GetExposedState`.
    pub app_state: Option<AppState>,
    pub guider_state: Option<GuiderState>,
    /// `CaptureActive`.
    pub capture_active: bool,
    /// `all_equipment_connected`: the camera, and every mount there is.
    pub connected: bool,
    pub camera: Option<DeviceView>,
    pub mount: Option<DeviceView>,
    pub aux_mount: Option<DeviceView>,
    pub ao: Option<DeviceView>,
    pub rotator: Option<DeviceView>,
    /// pMount and pSecondaryMount calibrated (`get_calibrated`).
    pub calibrated: bool,
    pub mount_calibration: CalibrationView,
    pub ao_calibration: CalibrationView,
    /// `LockPosition`, `None` when invalid.
    pub lock_position: Option<Point>,
    /// `CurrentPosition` and the primary star.
    pub star_position: Option<Point>,
    pub star: Option<Star>,
    /// `RequestedExposureDuration`, ms.
    pub exposure_ms: i32,
    /// The exposure setting: −1 under auto exposure.
    pub exposure_setting_ms: i32,
    pub exposure_durations: Vec<i32>,
    /// (min ms, max ms, target SNR).
    pub auto_exposure: (i32, i32, f64),
    /// `GetCameraPixelScale`: 1.0 when unknown (the server prints `null`).
    pub pixel_scale: f64,
    /// `TheScope()`'s, `Off` without one.
    pub dec_guide_mode: Option<DecGuideMode>,
    pub paused: PauseType,
    /// `PhdController::IsSettling`, and settling after a dither.
    pub settling: bool,
    pub dithering: bool,
    pub lock_shift: LockShiftParams,
    /// The camera's last frame size, px; binning.
    pub frame_size: (i32, i32),
    pub binning: u8,
    pub max_binning: u8,
    pub use_subframes: bool,
    pub search_region: i32,
    /// The current frame (`get_star_image`, `save_image`).
    pub image: Option<Arc<UsImage>>,
    /// The subframe of the current frame, if any.
    pub subframe: Option<Rect>,
    /// pMount's algorithms; `None` without pMount.
    pub algo_x: Option<AlgoView>,
    pub algo_y: Option<AlgoView>,
    /// The variable delay: enabled, short ms, long ms.
    pub var_delay: (bool, i32, i32),
    pub cooler: Option<CoolerStatus>,
    pub has_cooler: bool,
    pub sensor_temperature: Option<f64>,
    pub multistar: bool,
    pub stars_used: u32,
    pub dither_ra_only: bool,
    pub dither_scale: f64,
    pub dark_loaded: bool,
    pub defect_map_loaded: bool,
    pub profile_id: i32,
    pub profile_name: String,
    /// The last frame's number (`m_frameCounter`).
    pub frame: u32,
}
