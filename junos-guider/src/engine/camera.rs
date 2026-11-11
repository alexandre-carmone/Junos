//! The guide camera: the device seam the engine captures through (PHD2's
//! `GuideCamera` without its dark library), the camera settings PHD2 keeps
//! in the profile and its settings pane, and the "On Camera" mount that
//! sends its guide pulses through the camera's ST-4 port.
//!
//! A camera hands back the raw frame. PHD2's cameras subtract the dark and
//! run the Bayer and square-pixel reconstruction inside `Capture` when the
//! capture options ask for it; here the engine does that on the returned
//! frame, in PHD2's order (see [`super::darks`]), as the [`CaptureKind`]
//! says. The INDI client and the gear simulator implement [`GuideCamera`].
//!
//! Ported from PHD2 src/camera.h, src/camera.cpp, src/onboard_st4.h, src/scope_onboard_st4.cpp, src/scope_oncamera.cpp, src/advanced_dialog.cpp (`PercentChange`) — BSD-3-Clause, see THIRD_PARTY.md

use super::calibration::ProfileAccess;
use super::image::UsImage;
use super::log::cfmt::fmt_f;
use super::mount::GuideOutput;
use super::multistar::FindParams;
use super::star::FindMode;
use super::traits::{CancelToken, Clock, Dir, MoveResult, Rect};

/// `DefaultGuideCameraGain` (camera.cpp:44).
pub const DEFAULT_CAMERA_GAIN: i32 = 95;
/// `DefaultGuideCameraTimeoutMs`: how long past the exposure a frame may
/// take before the camera is dropped.
pub const DEFAULT_CAMERA_TIMEOUT_MS: i32 = 15000;
/// `SetTimeoutMs`' floor.
pub const MIN_CAMERA_TIMEOUT_MS: i32 = 5000;
/// `GuideCamera::UnknownPixelSize`.
pub const UNKNOWN_PIXEL_SIZE: f64 = 0.0;
/// `ScopeOnCamera`'s name: the mount of the equipment list and the guide
/// log when the pulses go through the camera.
pub const ON_CAMERA_NAME: &str = "On Camera";
/// `UNDEFINED_FRAME_SIZE` (camera.cpp:51): the frame size before the first
/// frame.
pub const UNDEFINED_FRAME_SIZE: (i32, i32) = (0, 0);

/// PHD2's capture options (`CaptureOptionBits`, camera.h:97), one per use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// `CAPTURE_LIGHT`: a guide frame, dark subtracted and reconstructed.
    Light,
    /// `CAPTURE_DARK`: a dark-library or defect-map frame, nothing applied.
    /// Whether the shutter is closed is the camera's own state
    /// ([`CameraSettings::shutter_closed`]), not the frame's.
    Dark,
    /// `CAPTURE_BPM_REVIEW`: the raw-image mode of the defect-map review,
    /// dark subtracted but not reconstructed.
    BpmReview,
}

impl CaptureKind {
    /// `CAPTURE_SUBTRACT_DARK`: subtract the defect map or the dark.
    pub fn subtract_dark(self) -> bool {
        self != CaptureKind::Dark
    }

    /// `CAPTURE_RECON`: Bayer mean and square pixels.
    pub fn recon(self) -> bool {
        self == CaptureKind::Light
    }
}

/// Why a capture failed. The camera has already done what PHD2's camera
/// does: on a disconnect (`DisconnectWithAlert`), it is disconnected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// A wait was interrupted (`WorkerThread::MilliSleep` with `INT_ANY`):
    /// no alert.
    Interrupted,
    /// `DisconnectWithAlert(CAPT_FAIL_TIMEOUT)`: the frame took longer than
    /// the exposure plus `/camera/TimeoutMs` (the camera watchdog).
    Timeout,
    /// `DisconnectWithAlert(CAPT_FAIL_MEMORY)`: no memory for the frame.
    Memory,
    /// `DisconnectWithAlert(msg, reconnect)` with the camera's own text.
    Disconnected { msg: String, reconnect: bool },
    /// Any other failure, the camera still connected, with the alert it
    /// raised (`pFrame->Alert`), if any.
    Failed(Option<String>),
}

/// `DisconnectWithAlert`'s timeout text while the shutter is closed (dark
/// frames' exposures differ from the exposure setting).
const SHUTTER_CLOSED_TIMEOUT: &str = "The camera has not completed an exposure in at least 15 seconds, so \
    it has been disconnected to prevent other problems. Refer to Trouble-shooting section of Help.";

impl CaptureError {
    /// The alert a failed capture raises, with PHD2's default icon (a
    /// warning), and whether PHD2 then tries to reconnect the camera
    /// (`TryReconnect`). A disconnect gets `DisconnectWithAlert`'s second
    /// line (camera.cpp:1512-1559). The timeout's text reads the main
    /// exposure setting (`RequestedExposureDuration`, ms), not the frame's,
    /// and has its 15-second form while the shutter is closed.
    pub fn alert(&self, requested_exposure_ms: i32, settings: &CameraSettings) -> Option<(String, bool)> {
        let (msg, reconnect) = match self {
            CaptureError::Interrupted | CaptureError::Failed(None) => return None,
            CaptureError::Failed(Some(msg)) => return Some((msg.clone(), false)),
            CaptureError::Memory => ("Memory allocation error during capture".to_string(), false),
            CaptureError::Timeout if settings.shutter_closed => (SHUTTER_CLOSED_TIMEOUT.to_string(), true),
            CaptureError::Timeout => (
                format!(
                    "After {} sec the camera has not completed a {} sec exposure, so it has been disconnected to \
                     prevent other problems. Refer to Trouble-shooting section of Help.",
                    // `(RequestedExposureDuration() + m_timeoutMs) / 1000.`, an int sum.
                    fmt_f(requested_exposure_ms.wrapping_add(settings.timeout_ms) as f64 / 1000.0, 1),
                    fmt_f(requested_exposure_ms as f64 / 1000.0, 1)
                ),
                true,
            ),
            CaptureError::Disconnected { msg, reconnect } => (msg.clone(), *reconnect),
        };
        let tail = if reconnect {
            "PHD will make several attempts to re-connect the camera."
        } else {
            "The camera has been disconnected. Please resolve the problem and re-connect the camera."
        };
        Some((format!("{msg}\n{tail}"), reconnect))
    }
}

/// `GetCoolerStatus`' answer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoolerStatus {
    pub on: bool,
    /// °C; only meaningful while on (`get_cooler_status` leaves it out).
    pub setpoint: f64,
    /// %; only meaningful while on.
    pub power: f64,
    /// The sensor's, °C.
    pub temperature: f64,
}

/// The camera as the engine sees it. Every call blocks on the engine
/// thread, which owns the camera (hence `Send`); `cancel` interrupts a wait
/// (PHD2's `INT_ANY` sleeps). The camera is connected and disconnected
/// outside the engine, like [`super::stepguider::StepGuiderDevice`] (the
/// INDI layer waits up to 30 s for it); a capture that ends in a disconnect
/// drops it by itself ([`CaptureError`]).
pub trait GuideCamera: Send {
    /// The user-facing name (`Name`), e.g. `INDI Camera [CCD Simulator]`.
    fn name(&self) -> String;
    /// `Connected`.
    fn is_connected(&self) -> bool;
    /// `FrameSize`: the size of the last frame, px, and
    /// [`UNDEFINED_FRAME_SIZE`] until the first one; a binning change only
    /// shows with the next frame. `get_camera_frame_size` answers it and
    /// the dark-frame geometry check reads it (`DarkFrameSize`).
    fn frame_size(&self) -> (i32, i32);
    /// `Binning`. A new camera holds the profile's, unclamped (see
    /// [`profile_binning`]).
    fn binning(&self) -> u8;
    /// Store a binning; use [`set_binning`], which clamps and saves it.
    fn set_binning(&mut self, binning: u8);
    /// `MaxBinning`.
    fn max_binning(&self) -> u8;
    /// `GetDevicePixelSize`: µm when the driver reports it. Otherwise the
    /// profile's `/camera/pixelsize` stands (see [`CameraSettings`]).
    fn pixel_size_um(&self) -> Option<f64>;
    /// `BitsPerPixel`.
    fn bits_per_pixel(&self) -> u8;
    /// `HasBayer`: a colour sensor, whose light frames taken at binning 1
    /// get `QuickLRecon` (cam_indi.cpp:1133).
    fn has_bayer(&self) -> bool {
        false
    }
    /// The pixel's width and height, µm (`PixSizeX`, `PixSizeY`): a light
    /// frame is resampled by `SquarePixels` when they differ
    /// (cam_indi.cpp:1137). `None` when the camera does not tell them.
    fn pixel_size_xy(&self) -> Option<(f64, f64)> {
        None
    }
    /// `HasGainControl`: the gain setting is shown.
    fn has_gain_control(&self) -> bool {
        false
    }
    /// `HasShutter`: dark frames need no cover on the scope.
    fn has_shutter(&self) -> bool {
        false
    }
    /// `HasSubframes`.
    fn has_subframes(&self) -> bool;
    /// `HasCooler`.
    fn has_cooler(&self) -> bool {
        false
    }
    /// `SetCoolerOn`: true on error, which is all PHD2's base class answers.
    fn set_cooler_on(&mut self, _on: bool) -> bool {
        true
    }
    /// `SetCoolerSetpoint`, °C: true on error.
    fn set_cooler_setpoint(&mut self, _celsius: f64) -> bool {
        true
    }
    /// `GetCoolerStatus`; `None` is PHD2's error ("failed to get cooler
    /// status").
    fn cooler_status(&self) -> Option<CoolerStatus> {
        None
    }
    /// `GetSensorTemperature`, °C; `None` on error.
    fn sensor_temperature(&self) -> Option<f64> {
        None
    }
    /// `ST4HasGuideOutput`: the camera has an ST-4 port.
    fn has_guide_output(&self) -> bool;
    /// `ST4PulseGuideScope`: one pulse through the ST-4 port, blocking for
    /// its length.
    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String>;
    /// The device's debug-log lines since the last call, oldest first: the
    /// engine writes them to the debug log after each capture and pulse.
    fn take_debug(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// One exposure of `dur_ms` ms, the raw frame back. `subframe` is the
    /// area to read when subframes are in use; a camera that cannot take it
    /// reads the full frame. `settings` holds what PHD2's camera reads from
    /// itself: `ShutterClosed`, and `/camera/TimeoutMs` for the watchdog.
    fn capture(
        &mut self,
        dur_ms: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError>;
}

/// The static `GuideCamera::Capture` (camera.cpp:1563): capture and stamp
/// the frame with its start time, the camera's bit depth and the exposure.
pub fn capture_frame(
    camera: &mut dyn GuideCamera,
    settings: &CameraSettings,
    dur_ms: u32,
    kind: CaptureKind,
    subframe: Option<Rect>,
    cancel: &CancelToken,
    clock: &dyn Clock,
) -> Result<UsImage, CaptureError> {
    let start = clock.wall();
    let mut img = camera.capture(dur_ms, kind, subframe, settings, cancel)?;
    img.img_start_time = start;
    img.bits_per_pixel = camera.bits_per_pixel();
    img.img_exp_dur = dur_ms as i32;
    Ok(img)
}

/// `SaturationValFromBPP` (camera.cpp:1044): the brightest value of the bit
/// depth. The settings dialog proposes it the first time, and puts it back
/// for a 0 or anything above it.
pub fn saturation_from_bpp(bpp: u8) -> u16 {
    ((1u32 << bpp) - 1) as u16
}

/// `AdvancedDialog::PercentChange`: how much `new` differs from `old`, %;
/// `100 · new` when `old` is about 0.
fn percent_change(old: f64, new: f64) -> f64 {
    if old.abs() < 0.0001 { 100.0 * new } else { 100.0 * (1.0 - new / old).abs() }
}

/// The `GuideCamera` constructor's `Binning = GetInt("/camera/binning", 1)`
/// (camera.cpp:225): the profile's binning, unclamped and cut to the
/// `wxByte` it lands in. Whoever makes a camera gives it this with
/// [`GuideCamera::set_binning`]; only [`set_binning`] clamps.
pub fn profile_binning(p: &impl ProfileAccess) -> u8 {
    p.get_i32("/camera/binning", 1) as u8
}

/// `GuideCamera::SetBinning`: clamped to `1..=max_binning`, given to the
/// camera and stored. PHD2's frame-limit rescale is left out: neither the
/// INDI camera nor the simulator limits frames.
pub fn set_binning(camera: &mut dyn GuideCamera, binning: i32, p: &mut impl ProfileAccess, log: &mut Vec<String>) {
    // `if (b < 1) b = 1; if (b > MaxBinning) b = MaxBinning;`, in that
    // order, so a camera reporting 0 gets 0.
    let b = binning.max(1).min(camera.max_binning() as i32);
    log.push(format!("camera: set binning = {b}"));
    camera.set_binning(b as u8);
    p.set_i32("/camera/binning", b);
}

/// The camera settings of the profile, as the `GuideCamera` constructor
/// reads them (camera.cpp:210), and the camera's `ShutterClosed`. The
/// binning is the camera's own (see [`profile_binning`]).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraSettings {
    /// `/camera/UseSubframes`.
    pub use_subframes: bool,
    /// `/camera/gain`, 0–100. Kept and shown; the INDI camera never sends
    /// it to the driver.
    pub gain: i32,
    /// `/camera/TimeoutMs`.
    pub timeout_ms: i32,
    /// `/camera/SaturationByADU`: saturation from the level below, else
    /// from the star's flat top.
    pub saturation_by_adu: bool,
    /// `/camera/SaturationADU`, capped at 65535.
    pub saturation_adu: u16,
    /// `/camera/pixelsize`, µm, [`UNKNOWN_PIXEL_SIZE`] when not set.
    pub pixel_size: f64,
    /// `ShutterClosed`, not a profile key: the dark build sets it before
    /// its first frame and clears it when the build completes or a capture
    /// fails (darks_dialog.cpp:263, 385, 520). Quirk kept: a cancelled build
    /// leaves it set, so the simulator then draws no stars and a timeout
    /// alert has its 15-second text until a build ends.
    pub shutter_closed: bool,
}

impl CameraSettings {
    pub fn load(p: &impl ProfileAccess) -> Self {
        CameraSettings {
            use_subframes: p.get_bool("/camera/UseSubframes", false),
            gain: p.get_i32("/camera/gain", DEFAULT_CAMERA_GAIN),
            timeout_ms: p.get_i32("/camera/TimeoutMs", DEFAULT_CAMERA_TIMEOUT_MS),
            // `(unsigned short) wxMin(GetInt(..), 65535)`: a negative value
            // wraps like the C cast.
            saturation_adu: p.get_i32("/camera/SaturationADU", 0).min(65535) as u16,
            saturation_by_adu: p.get_bool("/camera/SaturationByADU", true),
            pixel_size: p.get_f64("/camera/pixelsize", UNKNOWN_PIXEL_SIZE),
            shutter_closed: false,
        }
    }

    /// What `Star::find` reads from the camera: its saturation level is
    /// [`FindParams::max_adu`] (`GetSaturationADU`, 0 when saturation is by
    /// star profile).
    pub fn find_params(&self, mode: FindMode) -> FindParams {
        FindParams { mode, saturation_by_adu: self.saturation_by_adu, saturation_adu: self.saturation_adu }
    }

    /// `SetCameraGain`: outside 0–100 the default is stored and `false`
    /// comes back (PHD2's error), with its debug-log line in `log`.
    pub fn set_gain(&mut self, gain: i32, p: &mut impl ProfileAccess, log: &mut Vec<String>) -> bool {
        if gain < 0 {
            log.push("Error thrown from camera.cpp:699->cameraGain < 0".into());
        } else if gain > 100 {
            log.push("Error thrown from camera.cpp:703->cameraGain > 100".into());
        }
        let ok = (0..=100).contains(&gain);
        self.gain = if ok { gain } else { DEFAULT_CAMERA_GAIN };
        p.set_i32("/camera/gain", self.gain);
        ok
    }

    /// `SetTimeoutMs`: at least 5 s.
    pub fn set_timeout_ms(&mut self, ms: i32, p: &mut impl ProfileAccess) {
        self.timeout_ms = ms.max(MIN_CAMERA_TIMEOUT_MS);
        p.set_i32("/camera/TimeoutMs", self.timeout_ms);
    }

    /// `SetSaturationByADU`: the level is only stored when saturation is by
    /// ADU. The debug-log line goes to `log`.
    pub fn set_saturation_by_adu(&mut self, by_adu: bool, adu: u16, p: &mut impl ProfileAccess, log: &mut Vec<String>) {
        self.saturation_by_adu = by_adu;
        p.set_bool("/camera/SaturationByADU", by_adu);
        if by_adu {
            self.saturation_adu = adu;
            p.set_i32("/camera/SaturationADU", adu as i32);
            log.push(format!("Saturation detection set to Max-ADU value {adu}"));
        } else {
            log.push("Saturation detection set to star-profile-mode".into());
        }
    }

    /// `SetCameraPixelSize`: a size ≤ 0 stores "unknown" and gives `false`,
    /// with its debug-log line in `log`.
    pub fn set_pixel_size(&mut self, um: f64, p: &mut impl ProfileAccess, log: &mut Vec<String>) -> bool {
        // `if (pixel_size <= 0.0) throw`: a NaN passes, as in PHD2.
        let ok = !(um <= 0.0);
        if !ok {
            log.push("Error thrown from camera.cpp:792->pixel_size <= 0".into());
        }
        self.pixel_size = if ok { um } else { UNKNOWN_PIXEL_SIZE };
        p.set_f64("/camera/pixelsize", self.pixel_size);
        ok
    }

    /// `/camera/UseSubframes`, written by the settings dialog.
    pub fn set_use_subframes(&mut self, on: bool, p: &mut impl ProfileAccess) {
        self.use_subframes = on;
        p.set_bool("/camera/UseSubframes", on);
    }

    /// `CameraConfigDialogCtrlSet::UnloadValues` (camera.cpp:1233): the
    /// camera pane of the settings dialog applied, in PHD2's order, and
    /// what it asks of the rest of PHD2. The debug-log lines go to `log`.
    pub fn apply_dialog(
        &mut self,
        camera: &mut dyn GuideCamera,
        d: &CameraDialog,
        p: &mut impl ProfileAccess,
        log: &mut Vec<String>,
    ) -> DialogEffects {
        let mut fx = DialogEffects::default();
        if camera.has_subframes() {
            fx.subframes_turned_off = self.use_subframes && !d.use_subframes;
            self.set_use_subframes(d.use_subframes, p);
        }
        if camera.has_gain_control() {
            self.set_gain(d.gain, p, log);
        }
        if d.binning != camera.binning() as i32 {
            fx.image_scale_changed = true;
        }
        set_binning(camera, d.binning, p, log);
        self.set_timeout_ms(d.timeout_s.wrapping_mul(1000), p);
        // A change of 5 % or less keeps the calibration.
        if self.pixel_size != d.pixel_size && percent_change(self.pixel_size, d.pixel_size) > 5.0 {
            fx.image_scale_changed = true;
        }
        self.set_pixel_size(d.pixel_size, p, log);
        // A level of 0 or less, or above the bit depth's, is the bit
        // depth's.
        let full = saturation_from_bpp(camera.bits_per_pixel());
        let adu = match d.saturation_by_adu {
            false => 0,
            true if d.saturation_adu > 0 => d.saturation_adu.min(full as i64) as u16,
            true => full,
        };
        self.set_saturation_by_adu(d.saturation_by_adu, adu, p, log);
        if camera.has_cooler() {
            camera.set_cooler_on(d.cooler_on);
            camera.set_cooler_setpoint(d.cooler_setpoint as f64);
            p.set_f64("/camera/CoolerSetpt", d.cooler_setpoint as f64);
        }
        fx
    }
}

/// The camera pane of PHD2's settings dialog as the user leaves it.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraDialog {
    pub use_subframes: bool,
    pub gain: i32,
    /// The binning choice, 1-based.
    pub binning: i32,
    /// The timeout spinner, s.
    pub timeout_s: i32,
    pub pixel_size: f64,
    pub saturation_by_adu: bool,
    /// The level typed (`ToLong`, 0 when it does not parse).
    pub saturation_adu: i64,
    pub cooler_on: bool,
    /// The setpoint spinner, °C.
    pub cooler_setpoint: i32,
}

/// What applying the camera pane asks of the rest of PHD2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DialogEffects {
    /// Subframes were turned off: with multi-star guiding on, PHD2 calls
    /// `SetMultiStarMode(true)` to find the secondary stars again.
    pub subframes_turned_off: bool,
    /// `FlagImageScaleChange`: the binning changed, or the pixel size by
    /// more than 5 %; the dialog's OK then runs `MakeImageScaleAdjustments`.
    pub image_scale_changed: bool,
}

/// `ScopeOnboardST4::ConnectOnboardST4`: the "On Camera" mount connects
/// only to a connected camera with an ST-4 port.
pub fn on_camera_connectable(camera: &dyn GuideCamera) -> bool {
    camera.has_guide_output() && camera.is_connected()
}

/// The "On Camera" mount's guide output (`ScopeOnboardST4::Guide`): the
/// pulse goes through the camera's ST-4 port, and a pulse refused because
/// the camera is gone leaves its debug-log line in the second field. Borrow
/// both for the move: `ScopeIo { output: &mut OnCamera(&mut *camera, &mut
/// lines), .. }`. The mount's own connection is the scope's to check.
pub struct OnCamera<'a>(pub &'a mut dyn GuideCamera, pub &'a mut Vec<String>);

impl GuideOutput for OnCamera<'_> {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        if !self.0.is_connected() {
            self.1.push(
                "Error thrown from scope_onboard_st4.cpp:133->Attempt to Guide On Camera mount when camera is not \
                 connected"
                    .into(),
            );
            return MoveResult::Error;
        }
        match self.0.st4_pulse(dir, ms, cancel) {
            Ok(()) => MoveResult::Ok,
            Err(_) => MoveResult::Error,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[derive(Default)]
    struct Profile {
        f: HashMap<String, f64>,
        i: HashMap<String, i32>,
    }

    impl ProfileAccess for Profile {
        fn has(&self, key: &str) -> bool {
            self.f.contains_key(key) || self.i.contains_key(key)
        }
        fn get_f64(&self, key: &str, default: f64) -> f64 {
            self.f.get(key).copied().unwrap_or(default)
        }
        fn get_i32(&self, key: &str, default: i32) -> i32 {
            self.i.get(key).copied().unwrap_or(default)
        }
        fn get_str(&self, _key: &str, default: &str) -> String {
            default.to_string()
        }
        fn set_f64(&mut self, key: &str, value: f64) {
            self.f.insert(key.into(), value);
        }
        fn set_i32(&mut self, key: &str, value: i32) {
            self.i.insert(key.into(), value);
        }
        fn set_str(&mut self, _key: &str, _value: &str) {}
    }

    /// A camera that records its pulses and returns 4×3 frames.
    #[derive(Default)]
    struct Fake {
        connected: bool,
        binning: u8,
        pulses: Vec<(Dir, u32)>,
        fail_pulse: bool,
    }

    impl GuideCamera for Fake {
        fn name(&self) -> String {
            "Fake".into()
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn frame_size(&self) -> (i32, i32) {
            (4, 3)
        }
        fn binning(&self) -> u8 {
            self.binning
        }
        fn set_binning(&mut self, binning: u8) {
            self.binning = binning;
        }
        fn max_binning(&self) -> u8 {
            2
        }
        fn pixel_size_um(&self) -> Option<f64> {
            None
        }
        fn bits_per_pixel(&self) -> u8 {
            12
        }
        fn has_subframes(&self) -> bool {
            true
        }
        fn has_guide_output(&self) -> bool {
            true
        }
        fn st4_pulse(&mut self, dir: Dir, ms: u32, _cancel: &CancelToken) -> Result<(), String> {
            self.pulses.push((dir, ms));
            if self.fail_pulse { Err("no".into()) } else { Ok(()) }
        }
        fn capture(
            &mut self,
            _dur_ms: u32,
            _kind: CaptureKind,
            _subframe: Option<Rect>,
            _settings: &CameraSettings,
            _cancel: &CancelToken,
        ) -> Result<UsImage, CaptureError> {
            Ok(UsImage::new(4, 3))
        }
    }

    #[test]
    fn capture_kinds_are_phd2_option_bits() {
        // CAPTURE_LIGHT = 3, CAPTURE_DARK = 0, CAPTURE_BPM_REVIEW = 1.
        let bits = |k: CaptureKind| k.subtract_dark() as u8 | (k.recon() as u8) << 1;
        assert_eq!(bits(CaptureKind::Light), 3);
        assert_eq!(bits(CaptureKind::Dark), 0);
        assert_eq!(bits(CaptureKind::BpmReview), 1);
    }

    #[test]
    fn capture_frame_stamps_the_frame() {
        struct Fixed;
        impl Clock for Fixed {
            fn mono(&self) -> f64 {
                0.0
            }
            fn wall(&self) -> chrono::DateTime<chrono::Local> {
                chrono::DateTime::from_timestamp(1_000_000, 0).unwrap().into()
            }
            fn sleep_ms(&self, _ms: u64) {}
        }
        let mut cam = Fake::default();
        let s = CameraSettings::load(&Profile::default());
        let img = capture_frame(&mut cam, &s, 1500, CaptureKind::Light, None, &CancelToken::new(), &Fixed).unwrap();
        assert_eq!((img.w, img.h, img.img_exp_dur, img.bits_per_pixel), (4, 3, 1500, 12));
        assert_eq!(img.img_start_time.timestamp(), 1_000_000);
    }

    #[test]
    fn settings_defaults_and_setters() {
        let mut p = Profile::default();
        let mut s = CameraSettings::load(&p);
        assert_eq!(
            s,
            CameraSettings {
                use_subframes: false,
                gain: 95,
                timeout_ms: 15000,
                saturation_by_adu: true,
                saturation_adu: 0,
                pixel_size: 0.0,
                shutter_closed: false,
            }
        );
        let mut log = Vec::new();
        assert!(!s.set_gain(101, &mut p, &mut log));
        assert_eq!((s.gain, p.i["/camera/gain"]), (95, 95));
        assert!(!s.set_gain(-1, &mut p, &mut log));
        assert!(s.set_gain(40, &mut p, &mut log));
        assert_eq!(p.i["/camera/gain"], 40);

        s.set_timeout_ms(1000, &mut p);
        assert_eq!(p.i["/camera/TimeoutMs"], 5000);

        assert!(!s.set_pixel_size(-3.0, &mut p, &mut log));
        assert_eq!(p.f["/camera/pixelsize"], 0.0);
        assert!(s.set_pixel_size(3.75, &mut p, &mut log));
        assert_eq!(
            log,
            [
                "Error thrown from camera.cpp:703->cameraGain > 100",
                "Error thrown from camera.cpp:699->cameraGain < 0",
                "Error thrown from camera.cpp:792->pixel_size <= 0",
            ]
        );

        let mut log = Vec::new();
        s.set_saturation_by_adu(true, 4095, &mut p, &mut log);
        assert_eq!(s.find_params(FindMode::Centroid).max_adu(), 4095);
        s.set_saturation_by_adu(false, 1, &mut p, &mut log);
        assert_eq!((s.find_params(FindMode::Peak).max_adu(), p.i["/camera/SaturationADU"]), (0, 4095));
        assert_eq!(
            log,
            ["Saturation detection set to Max-ADU value 4095", "Saturation detection set to star-profile-mode"]
        );

        p.i.insert("/camera/SaturationADU".into(), 70000);
        assert_eq!(CameraSettings::load(&p).saturation_adu, 65535);
        assert_eq!(saturation_from_bpp(16), 65535);
        assert_eq!(saturation_from_bpp(12), 4095);
        assert_eq!(saturation_from_bpp(8), 255);
    }

    #[test]
    fn binning_is_clamped_and_stored_but_not_on_load() {
        let mut p = Profile::default();
        let mut cam = Fake::default();
        let mut log = Vec::new();
        set_binning(&mut cam, 3, &mut p, &mut log);
        assert_eq!((cam.binning, p.i["/camera/binning"]), (2, 2));
        set_binning(&mut cam, 0, &mut p, &mut log);
        assert_eq!(cam.binning, 1);
        assert_eq!(log, ["camera: set binning = 2", "camera: set binning = 1"]);

        // The constructor takes the profile's as a byte, unclamped.
        for (stored, want) in [(4, 4), (300, 44), (-1, 255), (0, 0)] {
            p.i.insert("/camera/binning".into(), stored);
            assert_eq!(profile_binning(&p), want);
        }
        assert_eq!(profile_binning(&Profile::default()), 1);
    }

    #[test]
    fn capture_failures_alert_as_phd2() {
        let mut s = CameraSettings::load(&Profile::default());
        assert_eq!(CaptureError::Interrupted.alert(2000, &s), None);
        assert_eq!(CaptureError::Failed(None).alert(2000, &s), None);
        assert_eq!(
            CaptureError::Failed(Some("Unknown image format: .png".into())).alert(2000, &s),
            Some(("Unknown image format: .png".into(), false))
        );
        let (msg, reconnect) = CaptureError::Timeout.alert(2500, &s).unwrap();
        assert!(reconnect);
        assert_eq!(
            msg,
            "After 17.5 sec the camera has not completed a 2.5 sec exposure, so it has been disconnected to prevent \
             other problems. Refer to Trouble-shooting section of Help.\n\
             PHD will make several attempts to re-connect the camera."
        );
        s.shutter_closed = true;
        assert_eq!(
            CaptureError::Timeout.alert(2500, &s).unwrap().0,
            "The camera has not completed an exposure in at least 15 seconds, so it has been disconnected to \
             prevent other problems. Refer to Trouble-shooting section of Help.\n\
             PHD will make several attempts to re-connect the camera."
        );
        assert_eq!(
            CaptureError::Memory.alert(2500, &s),
            Some((
                "Memory allocation error during capture\n\
                 The camera has been disconnected. Please resolve the problem and re-connect the camera."
                    .into(),
                false
            ))
        );
        let lost = CaptureError::Disconnected { msg: "Lost".into(), reconnect: true };
        assert_eq!(lost.alert(0, &s).unwrap().0, "Lost\nPHD will make several attempts to re-connect the camera.");
    }

    #[test]
    fn the_settings_pane_applies_in_phd2_order() {
        let mut p = Profile::default();
        let mut cam = Fake { binning: 1, ..Fake::default() };
        let mut s = CameraSettings { use_subframes: true, pixel_size: 3.75, ..CameraSettings::load(&p) };
        let pane = CameraDialog {
            use_subframes: false,
            gain: 120,
            binning: 1,
            timeout_s: 3,
            pixel_size: 3.9,
            saturation_by_adu: true,
            saturation_adu: 70000,
            cooler_on: true,
            cooler_setpoint: -10,
        };
        let mut log = Vec::new();
        let fx = s.apply_dialog(&mut cam, &pane, &mut p, &mut log);
        // 4 % more pixel size and the same binning keep the image scale;
        // the gain is not shown, so not checked; no cooler, no setpoint.
        assert_eq!(fx, DialogEffects { subframes_turned_off: true, image_scale_changed: false });
        assert_eq!((s.use_subframes, s.gain, s.timeout_ms, s.pixel_size), (false, 95, 5000, 3.9));
        // Above the 12-bit maximum: the maximum.
        assert_eq!(s.saturation_adu, 4095);
        assert!(!p.f.contains_key("/camera/CoolerSetpt"));
        assert_eq!(log, ["camera: set binning = 1", "Saturation detection set to Max-ADU value 4095"]);

        // 6.7 % more, a new binning, a level of 0.
        let fx = s.apply_dialog(
            &mut cam,
            &CameraDialog { pixel_size: 4.16, saturation_adu: 0, ..pane.clone() },
            &mut p,
            &mut Vec::new(),
        );
        assert_eq!(fx, DialogEffects { subframes_turned_off: false, image_scale_changed: true });
        assert_eq!(s.saturation_adu, 4095);
        let fx = s.apply_dialog(
            &mut cam,
            &CameraDialog { binning: 2, pixel_size: 4.16, saturation_adu: 900, ..pane },
            &mut p,
            &mut Vec::new(),
        );
        assert!(fx.image_scale_changed);
        assert_eq!((cam.binning, s.saturation_adu), (2, 900));
        // From an unknown size, any size changes the scale.
        assert_eq!(percent_change(0.0, 3.75), 375.0);
    }

    #[test]
    fn on_camera_pulses_through_the_st4_port() {
        let mut cam = Fake::default();
        let mut lines = Vec::new();
        assert!(!on_camera_connectable(&cam));
        let cancel = CancelToken::new();
        assert_eq!(OnCamera(&mut cam, &mut lines).guide(Dir::West, 500, &cancel), MoveResult::Error);
        assert!(cam.pulses.is_empty());
        assert_eq!(
            lines,
            [
                "Error thrown from scope_onboard_st4.cpp:133->Attempt to Guide On Camera mount when camera is not connected"
            ]
        );

        cam.connected = true;
        assert!(on_camera_connectable(&cam));
        assert_eq!(OnCamera(&mut cam, &mut lines).guide(Dir::North, 250, &cancel), MoveResult::Ok);
        cam.fail_pulse = true;
        assert_eq!(OnCamera(&mut cam, &mut lines).guide(Dir::East, 100, &cancel), MoveResult::Error);
        assert_eq!(cam.pulses, [(Dir::North, 250), (Dir::East, 100)]);
        assert_eq!(lines.len(), 1);
        // No cooler on PHD2's base camera.
        assert_eq!((cam.cooler_status(), cam.sensor_temperature(), cam.set_cooler_on(true)), (None, None, true));
    }
}
