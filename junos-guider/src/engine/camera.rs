//! The guide camera: the device seam the engine captures through (PHD2's
//! `GuideCamera` without its dialogs, dark library and cooler), the camera
//! settings PHD2 keeps in the profile, and the "On Camera" mount that sends
//! its guide pulses through the camera's ST-4 port.
//!
//! A camera hands back the raw frame. PHD2's cameras subtract the dark and
//! run the Bayer and square-pixel reconstruction inside `Capture` when the
//! capture options ask for it; here the engine does that on the returned
//! frame, in PHD2's order (see [`super::darks`]), as the [`CaptureKind`]
//! says. The INDI client and the gear simulator implement [`GuideCamera`].
//!
//! Ported from PHD2 src/camera.h, src/camera.cpp, src/onboard_st4.h, src/scope_onboard_st4.cpp, src/scope_oncamera.cpp — BSD-3-Clause, see THIRD_PARTY.md

use super::calibration::ProfileAccess;
use super::image::UsImage;
use super::mount::GuideOutput;
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

/// PHD2's capture options (`CaptureOptionBits`, camera.h:97), one per use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// `CAPTURE_LIGHT`: a guide frame, dark subtracted and reconstructed.
    Light,
    /// `CAPTURE_DARK`: a dark-library or defect-map frame, nothing applied.
    /// PHD2 closes the shutter around every such capture
    /// (`ShutterClosed`, darks_dialog.cpp:263), so a camera reads it as a
    /// shutter-closed exposure.
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

    /// `ShutterClosed` for the exposure.
    pub fn shutter_closed(self) -> bool {
        self == CaptureKind::Dark
    }
}

/// The camera as the engine sees it. Every call blocks on the engine
/// thread; `cancel` interrupts a wait (PHD2's `INT_ANY` sleeps).
pub trait GuideCamera {
    /// The user-facing name (`Name`), e.g. `INDI Camera [CCD Simulator]`.
    fn name(&self) -> String;
    fn connect(&mut self) -> Result<(), String>;
    fn disconnect(&mut self) -> Result<(), String>;
    fn is_connected(&self) -> bool;
    /// `FrameSize`: the full frame at the current binning, px.
    fn full_size(&self) -> (i32, i32);
    /// `Binning`.
    fn binning(&self) -> u8;
    /// Store a binning already clamped to `1..=max_binning()`; use
    /// [`CameraSettings::set_binning`], which clamps and saves it.
    fn set_binning(&mut self, binning: u8);
    /// `MaxBinning`.
    fn max_binning(&self) -> u8;
    /// `GetDevicePixelSize`: µm when the driver reports it. Otherwise the
    /// profile's `/camera/pixelsize` stands (see [`CameraSettings`]).
    fn pixel_size_um(&self) -> Option<f64>;
    /// `BitsPerPixel`.
    fn bits_per_pixel(&self) -> u8;
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
    /// `ST4HasGuideOutput`: the camera has an ST-4 port.
    fn has_guide_output(&self) -> bool;
    /// `ST4PulseGuideScope`: one pulse through the ST-4 port, blocking for
    /// its length.
    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String>;
    /// One exposure of `dur_ms` ms, the raw frame back. `subframe` is the
    /// area to read when subframes are in use; a camera that cannot take it
    /// reads the full frame.
    fn capture(
        &mut self,
        dur_ms: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        cancel: &CancelToken,
    ) -> Result<UsImage, String>;
}

/// The static `GuideCamera::Capture` (camera.cpp:1557): capture and stamp
/// the frame with its start time, the camera's bit depth and the exposure.
pub fn capture_frame(
    camera: &mut dyn GuideCamera,
    dur_ms: u32,
    kind: CaptureKind,
    subframe: Option<Rect>,
    cancel: &CancelToken,
    clock: &dyn Clock,
) -> Result<UsImage, String> {
    let start = clock.wall();
    let mut img = camera.capture(dur_ms, kind, subframe, cancel)?;
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

/// The camera settings of the profile, as the `GuideCamera` constructor
/// reads them (camera.cpp:210).
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
    /// `/camera/binning`. The camera holds the live value; this follows it
    /// through [`CameraSettings::set_binning`].
    pub binning: u8,
}

impl CameraSettings {
    pub fn load(p: &impl ProfileAccess) -> Self {
        CameraSettings {
            use_subframes: p.get_i32("/camera/UseSubframes", 0) != 0,
            gain: p.get_i32("/camera/gain", DEFAULT_CAMERA_GAIN),
            timeout_ms: p.get_i32("/camera/TimeoutMs", DEFAULT_CAMERA_TIMEOUT_MS),
            // `(unsigned short) wxMin(GetInt(..), 65535)`: a negative value
            // wraps like the C cast.
            saturation_adu: p.get_i32("/camera/SaturationADU", 0).min(65535) as u16,
            saturation_by_adu: p.get_i32("/camera/SaturationByADU", 1) != 0,
            pixel_size: p.get_f64("/camera/pixelsize", UNKNOWN_PIXEL_SIZE),
            // `wxByte Binning = GetInt(..)`: truncated like the C cast.
            binning: p.get_i32("/camera/binning", 1) as u8,
        }
    }

    /// `GetSaturationADU`: the level, or 0 when saturation is by profile
    /// (what `Star::find` takes as `max_adu`).
    pub fn saturation_adu(&self) -> u16 {
        if self.saturation_by_adu { self.saturation_adu } else { 0 }
    }

    /// `SetCameraGain`: outside 0–100 the default is stored and `false`
    /// comes back (PHD2's error).
    pub fn set_gain(&mut self, gain: i32, p: &mut impl ProfileAccess) -> bool {
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
    pub fn set_saturation_by_adu(
        &mut self,
        by_adu: bool,
        adu: u16,
        p: &mut impl ProfileAccess,
        log: &mut Vec<String>,
    ) {
        self.saturation_by_adu = by_adu;
        p.set_i32("/camera/SaturationByADU", by_adu as i32);
        if by_adu {
            self.saturation_adu = adu;
            p.set_i32("/camera/SaturationADU", adu as i32);
            log.push(format!("Saturation detection set to Max-ADU value {adu}"));
        } else {
            log.push("Saturation detection set to star-profile-mode".into());
        }
    }

    /// `SetCameraPixelSize`: a size ≤ 0 stores "unknown" and gives `false`.
    pub fn set_pixel_size(&mut self, um: f64, p: &mut impl ProfileAccess) -> bool {
        let ok = um > 0.0;
        self.pixel_size = if ok { um } else { UNKNOWN_PIXEL_SIZE };
        p.set_f64("/camera/pixelsize", self.pixel_size);
        ok
    }

    /// `/camera/UseSubframes`, written by the settings dialog.
    pub fn set_use_subframes(&mut self, on: bool, p: &mut impl ProfileAccess) {
        self.use_subframes = on;
        p.set_i32("/camera/UseSubframes", on as i32);
    }

    /// `GuideCamera::SetBinning`: clamped to `1..=max_binning`, given to the
    /// camera and stored. PHD2's frame-limit rescale is left out: neither
    /// the INDI camera nor the simulator limits frames.
    pub fn set_binning(
        &mut self,
        camera: &mut dyn GuideCamera,
        binning: i32,
        p: &mut impl ProfileAccess,
        log: &mut Vec<String>,
    ) {
        // `if (b < 1) b = 1; if (b > MaxBinning) b = MaxBinning;`, in that
        // order, so a camera reporting 0 gets 0.
        let b = binning.max(1).min(camera.max_binning() as i32);
        log.push(format!("camera: set binning = {b}"));
        camera.set_binning(b as u8);
        self.binning = b as u8;
        p.set_i32("/camera/binning", b);
    }
}

/// `ScopeOnboardST4::ConnectOnboardST4`: the "On Camera" mount connects
/// only to a connected camera with an ST-4 port.
pub fn on_camera_connectable(camera: &dyn GuideCamera) -> bool {
    camera.has_guide_output() && camera.is_connected()
}

/// The "On Camera" mount's guide output (`ScopeOnboardST4::Guide`): the
/// pulse goes through the camera's ST-4 port. Borrow the camera for the
/// move: `ScopeIo { output: &mut OnCamera(&mut *camera), .. }`.
pub struct OnCamera<'a>(pub &'a mut dyn GuideCamera);

impl GuideOutput for OnCamera<'_> {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        if !self.0.is_connected() {
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
        fn connect(&mut self) -> Result<(), String> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), String> {
            self.connected = false;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn full_size(&self) -> (i32, i32) {
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
            false
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
            _cancel: &CancelToken,
        ) -> Result<UsImage, String> {
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
        assert!(CaptureKind::Dark.shutter_closed());
        assert!(!CaptureKind::BpmReview.shutter_closed());
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
        let img = capture_frame(&mut cam, 1500, CaptureKind::Light, None, &CancelToken::new(), &Fixed).unwrap();
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
                binning: 1,
            }
        );
        assert!(!s.set_gain(101, &mut p));
        assert_eq!((s.gain, p.i["/camera/gain"]), (95, 95));
        assert!(s.set_gain(40, &mut p));
        assert_eq!(p.i["/camera/gain"], 40);

        s.set_timeout_ms(1000, &mut p);
        assert_eq!(p.i["/camera/TimeoutMs"], 5000);

        assert!(!s.set_pixel_size(-3.0, &mut p));
        assert_eq!(p.f["/camera/pixelsize"], 0.0);
        assert!(s.set_pixel_size(3.75, &mut p));

        let mut log = Vec::new();
        s.set_saturation_by_adu(true, 4095, &mut p, &mut log);
        assert_eq!(s.saturation_adu(), 4095);
        s.set_saturation_by_adu(false, 1, &mut p, &mut log);
        assert_eq!((s.saturation_adu(), p.i["/camera/SaturationADU"]), (0, 4095));
        assert_eq!(log, ["Saturation detection set to Max-ADU value 4095", "Saturation detection set to star-profile-mode"]);

        p.i.insert("/camera/SaturationADU".into(), 70000);
        assert_eq!(CameraSettings::load(&p).saturation_adu, 65535);
        assert_eq!(saturation_from_bpp(16), 65535);
        assert_eq!(saturation_from_bpp(12), 4095);
        assert_eq!(saturation_from_bpp(8), 255);
    }

    #[test]
    fn binning_is_clamped_and_stored() {
        let mut p = Profile::default();
        let mut s = CameraSettings::load(&p);
        let mut cam = Fake::default();
        let mut log = Vec::new();
        s.set_binning(&mut cam, 3, &mut p, &mut log);
        assert_eq!((cam.binning, s.binning, p.i["/camera/binning"]), (2, 2, 2));
        s.set_binning(&mut cam, 0, &mut p, &mut log);
        assert_eq!(cam.binning, 1);
        assert_eq!(log, ["camera: set binning = 2", "camera: set binning = 1"]);
    }

    #[test]
    fn on_camera_pulses_through_the_st4_port() {
        let mut cam = Fake::default();
        assert!(!on_camera_connectable(&cam));
        let cancel = CancelToken::new();
        assert_eq!(OnCamera(&mut cam).guide(Dir::West, 500, &cancel), MoveResult::Error);
        assert!(cam.pulses.is_empty());

        cam.connect().unwrap();
        assert!(on_camera_connectable(&cam));
        assert_eq!(OnCamera(&mut cam).guide(Dir::North, 250, &cancel), MoveResult::Ok);
        cam.fail_pulse = true;
        assert_eq!(OnCamera(&mut cam).guide(Dir::East, 100, &cancel), MoveResult::Error);
        assert_eq!(cam.pulses, [(Dir::North, 250), (Dir::East, 100)]);
    }
}
