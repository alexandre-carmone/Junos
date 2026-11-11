//! The INDI guide camera, PHD2's `CameraINDI`: exposures through
//! `CCD_EXPOSURE` with the frame back as a FITS BLOB, the frame area and
//! binning set before each one, the ST-4 port through the driver's
//! `TELESCOPE_TIMED_GUIDE_*`, and the video stream stacked for the cameras
//! that only stream.
//!
//! The main chip uses the `CCD_*` properties and the `CCD1` BLOB, the guide
//! head the `GUIDER_*` ones and `CCD2` (`/indi/INDIcam_ccd`). What PHD2
//! keeps from the driver's updates (the binning in use and its maximum, the
//! sensor size, the bit depth, the Bayer pattern) is read from the store
//! when needed, a property's sequence number telling a new update from one
//! already seen. Two additions from the PLAN: before an exposure the
//! driver's compression is turned off and its transfer format set to FITS
//! when it has those properties, and a capture waits on the cancel token as
//! well as on the watchdog (exposure + `/camera/TimeoutMs`), so a stop does
//! not wait for the frame. The dark, the Bayer reconstruction and the
//! square pixels are the engine's, on the returned frame.
//!
//! The camera is connected when its driver's `CONNECT` switch is On; it
//! drops (PHD2's `DisconnectWithAlert`, no reconnect) when the server goes,
//! when the device goes, or when `CONNECT` turns Off once the camera had
//! what it needs to capture. A drop shows at the next capture or
//! [`IndiCamera::check_connection`].
//!
//! Ported from PHD2 src/cam_indi.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::cell::Cell;
use std::thread;
use std::time::{Duration, Instant};

use super::client::{self, IndiClient, Link, PulseEnd, TimedGuide, WaitError};
use super::store::GUIDER_INTERFACE;
use super::xml::{Kind, PropState, Value};
use crate::engine::calibration::ProfileAccess;
use crate::engine::camera::{CameraSettings, CaptureError, CaptureKind, GuideCamera, UNDEFINED_FRAME_SIZE};
use crate::engine::fits::read_fits_blob;
use crate::engine::image::UsImage;
use crate::engine::traits::{CancelToken, Dir, Rect, c_int};

/// `/indi/INDIcam`'s default: no camera chosen yet (PHD2 then opens its
/// chooser).
pub const UNSET_CAMERA: &str = "INDI Camera";

/// The camera's profile keys, as `CameraINDI`'s constructor reads them.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraOptions {
    /// `/indi/INDIcam`.
    pub device: String,
    /// `/indi/INDIcam_ccd`: 0 for the main chip, anything else for the
    /// guide head.
    pub ccd: i32,
    /// `/indi/INDIcam_forcevideo`: stack the stream even when the driver
    /// can expose.
    pub force_video: bool,
    /// `/indi/INDIcam_forceexposure`: never fall back to the stream.
    pub force_exposure: bool,
    /// `/indi/PulseMinWait`.
    pub pulse_min_wait: bool,
}

impl CameraOptions {
    pub fn from_profile(p: &impl ProfileAccess) -> Self {
        CameraOptions {
            device: p.get_str("/indi/INDIcam", UNSET_CAMERA),
            ccd: p.get_i32("/indi/INDIcam_ccd", 0),
            force_video: p.get_bool("/indi/INDIcam_forcevideo", false),
            force_exposure: p.get_bool("/indi/INDIcam_forceexposure", false),
            pulse_min_wait: client::pulse_min_wait(p),
        }
    }
}

pub struct IndiCamera {
    client: IndiClient,
    device: String,
    /// `INDICameraCCDCmd` and `INDICameraBlobName`.
    prefix: &'static str,
    blob: &'static str,
    /// `INDICameraForceVideo`, which a failed first exposure turns on.
    force_video: bool,
    force_exposure: bool,
    /// The link generation the camera connected in: `Connected`.
    attached: Option<u64>,
    binning: u8,
    /// `m_curBinning`, and the binning property's sequence number when it
    /// was last read from the driver.
    cur_binning: u8,
    binning_seq: u64,
    /// `MaxBinning`, `m_maxSize`, `m_bitsPerPixel` and `HasBayer`: kept when
    /// the device goes, as PHD2 keeps them.
    max_binning: Cell<u8>,
    max_size: Cell<(i32, i32)>,
    bits: Cell<u8>,
    bayer: Cell<bool>,
    frame_size: (i32, i32),
    /// `m_roi`: the frame area last sent, in binned pixels.
    roi: Rect,
    first_frame: bool,
    pulse: TimedGuide,
    debug: Vec<String>,
    alerts: Vec<String>,
}

impl IndiCamera {
    pub fn new(client: IndiClient, opts: &CameraOptions) -> Self {
        // `SetCCDdevice`.
        let (prefix, blob) = if opts.ccd == 0 { ("CCD_", "CCD1") } else { ("GUIDER_", "CCD2") };
        IndiCamera {
            client,
            device: opts.device.clone(),
            prefix,
            blob,
            force_video: opts.force_video,
            force_exposure: opts.force_exposure,
            attached: None,
            binning: 1,
            cur_binning: 0,
            binning_seq: 0,
            max_binning: Cell::new(1),
            max_size: Cell::new((0, 0)),
            bits: Cell::new(0),
            bayer: Cell::new(false),
            frame_size: UNDEFINED_FRAME_SIZE,
            roi: Rect::default(),
            first_frame: false,
            pulse: TimedGuide::new(opts.pulse_min_wait),
            debug: Vec::new(),
            alerts: Vec::new(),
        }
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// `Connect`: waits up to 30 s for the device to connect.
    pub fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        if self.attached.is_some() {
            return Ok(());
        }
        if self.device.is_empty() || self.device == UNSET_CAMERA {
            return Err("No INDI camera is selected".into());
        }
        self.debug.push(format!("INDI Camera connecting to device [{}]", self.device));
        self.client.want_blob(&self.device, self.blob);
        self.debug.push(format!("Waiting for 30s for [{}] to connect...", self.device));
        self.attached = Some(client::connect_device(&self.client, &self.device, cancel)?);
        self.first_frame = true;
        // A driver defines its capture properties right after `CONNECT`
        // turns On; PHD2's first capture comes long after, ours may not.
        if self.client.wait(client::READY_GRACE, Some(cancel), |l| self.ready(l).then_some(())).is_ok() {
            self.debug.push("INDI Camera is ready".into());
        }
        Ok(())
    }

    /// `Disconnect`: the camera lets go of the device (PHD2 closes its
    /// connection, which stops the BLOBs), leaving the driver connected.
    pub fn disconnect(&mut self) {
        if self.attached.take().is_some() {
            self.client.drop_blob(&self.device, self.blob);
            self.debug.push("INDI Camera: serverDisconnected".into());
        }
    }

    /// The drop PHD2 notices on its INDI thread, noticed here: the camera
    /// is disconnected and the error carries the alert.
    pub fn check_connection(&mut self) -> Option<CaptureError> {
        let msg = self.lost(&self.client.lock())?;
        self.disconnect();
        Some(CaptureError::Disconnected { msg: msg.into(), reconnect: false })
    }

    /// PHD2's debug-log lines since the last call.
    pub fn take_debug(&mut self) -> Vec<String> {
        std::mem::take(&mut self.debug)
    }

    /// Alerts raised without failing the capture (the switch to streaming).
    pub fn take_alerts(&mut self) -> Vec<String> {
        std::mem::take(&mut self.alerts)
    }

    fn prop(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }

    fn has(&self, link: &Link, name: &str, kind: Kind) -> bool {
        link.store.property(&self.device, name).is_some_and(|p| p.kind == kind)
    }

    /// The stream switch and its two members: `<prefix>VIDEO_STREAM`, or the
    /// old `VIDEO_STREAM` with `ON`/`OFF`.
    fn video(&self, link: &Link) -> Option<(String, &'static str, &'static str)> {
        let new = self.prop("VIDEO_STREAM");
        if self.has(link, &new, Kind::Switch) {
            Some((new, "STREAM_ON", "STREAM_OFF"))
        } else {
            self.has(link, "VIDEO_STREAM", Kind::Switch).then(|| ("VIDEO_STREAM".to_string(), "ON", "OFF"))
        }
    }

    /// Why the camera dropped, if it did: the alert PHD2 raises.
    fn lost(&self, link: &Link) -> Option<&'static str> {
        let generation = self.attached?;
        if !link.is_up() || link.generation() != generation {
            return Some("INDI server disconnected");
        }
        if link.store.device(&self.device).is_none() {
            return Some("INDI camera disconnected");
        }
        (self.ready(link) && link.device_connected(&self.device) == Some(false)).then_some("INDI camera disconnected")
    }

    /// `CheckState`'s `ready`: the BLOB and a way to capture.
    fn ready(&self, link: &Link) -> bool {
        self.has(link, self.blob, Kind::Blob)
            && (self.has(link, &self.prop("EXPOSURE"), Kind::Number) || self.video(link).is_some())
    }

    fn is_connected_in(&self, link: &Link) -> bool {
        self.attached.is_some() && self.lost(link).is_none()
    }

    /// `CCD_INFO`, the binning maxima and the Bayer pattern, as PHD2's
    /// `updateProperty` keeps them.
    fn read_info(&self, link: &Link) {
        if let Some(p) = link.store.property(&self.device, &self.prop("INFO")).filter(|p| p.kind == Kind::Number) {
            let n = |m: &str| p.number(m).unwrap_or(0.0);
            self.max_size.set((c_int(n("CCD_MAX_X")), c_int(n("CCD_MAX_Y"))));
            self.bits.set(c_int(n("CCD_BITSPERPIXEL")) as u8);
        }
        if let Some(((_, xmax), (_, ymax))) = self.binning_members(link) {
            self.max_binning.set(c_int(xmax.min(ymax)) as u8);
        }
        let cfa = link.store.property(&self.device, &self.prop("CFA")).filter(|p| p.kind == Kind::Text);
        if cfa.and_then(|p| p.text("CFA_TYPE")).is_some_and(|t| !t.is_empty()) {
            self.bayer.set(true);
        }
    }

    /// `HOR_BIN` and `VER_BIN`: (value, max) each.
    fn binning_members(&self, link: &Link) -> Option<((f64, f64), (f64, f64))> {
        let p = link.store.property(&self.device, &self.prop("BINNING")).filter(|p| p.kind == Kind::Number)?;
        let member = |name: &str| match p.member(name)?.value {
            Value::Number { value, max, .. } => Some((value, max)),
            _ => None,
        };
        Some((member("HOR_BIN")?, member("VER_BIN")?))
    }

    /// The binning update PHD2 handles as it comes: the binning in use, and
    /// the binning setting clamped to the new maximum.
    fn refresh(&mut self, link: &Link) {
        self.read_info(link);
        let seq = link.store.property(&self.device, &self.prop("BINNING")).map(|p| p.seq);
        if let (Some(seq), Some(((x, _), (y, _)))) = (seq, self.binning_members(link)) {
            if seq != self.binning_seq {
                self.binning_seq = seq;
                self.cur_binning = c_int(x.min(y)) as u8;
                self.binning = self.binning.min(self.max_binning.get());
            }
        }
    }

    /// `SendBinning`.
    fn send_binning(&mut self, link: &Link) {
        let b = self.binning as f64;
        self.debug.push(format!("INDI Camera: send binning {}", self.binning));
        self.client.send_number(link, &self.device, &self.prop("BINNING"), &[("HOR_BIN", b), ("VER_BIN", b)]);
        self.cur_binning = self.binning;
    }

    /// A one-of-many switch set to `member`, the others Off, unless it is
    /// already: the compression and the transfer format.
    fn select(&self, link: &Link, property: &str, member: &str) {
        let Some(p) = link.store.property(&self.device, property).filter(|p| p.kind == Kind::Switch) else { return };
        if p.switch(member) == Some(false) {
            let states: Vec<(&str, bool)> = p.members.iter().map(|m| (m.name.as_str(), m.name == member)).collect();
            self.client.send_switch(link, &self.device, property, &states);
        }
    }

    fn capture_exposure(
        &mut self,
        generation: u64,
        dur_ms: u32,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError> {
        let client = self.client.clone();
        let device = self.device.clone();
        let expose = self.prop("EXPOSURE");
        let frame = self.prop("FRAME");
        let mut take_sub = subframe.is_some();
        let mut sf = subframe.unwrap_or_default();
        let busy = {
            let link = client.lock();
            self.select(&link, &self.prop("COMPRESSION"), "INDI_DISABLED");
            self.select(&link, "CCD_TRANSFER_FORMAT", "FORMAT_FITS");
            if self.binning_members(&link).is_some() && self.binning != self.cur_binning {
                self.send_binning(&link);
                // The subframe may be out of bounds now; the binned size is
                // only known from the next frame.
                take_sub = false;
                self.frame_size = if self.binning == 1 { self.max_size.get() } else { UNDEFINED_FRAME_SIZE };
            }
            let has_frame = self.has(&link, &frame, Kind::Number);
            if !has_frame || sf.w <= 0 || sf.h <= 0 || self.frame_size == UNDEFINED_FRAME_SIZE {
                take_sub = false;
            }
            if !take_sub {
                // The full frame; the maximum over the binning may be larger
                // than the binned frame, which asks for all of it.
                let b = (self.binning as i32).max(1);
                let (w, h) = if self.frame_size != UNDEFINED_FRAME_SIZE {
                    self.frame_size
                } else {
                    let (mx, my) = self.max_size.get();
                    (mx / b, my / b)
                };
                sf = Rect::new(0, 0, w, h);
            }
            if has_frame && sf != self.roi {
                let b = self.binning as i32;
                let v = |x: i32| (x * b) as f64;
                let values = [("X", v(sf.x)), ("Y", v(sf.y)), ("WIDTH", v(sf.w)), ("HEIGHT", v(sf.h))];
                client.send_number(&link, &device, &frame, &values);
                self.roi = sf;
            }
            link.store.property(&device, &expose).is_some_and(|p| p.state == PropState::Busy)
        };

        // `CameraWatchdog(duration, GetTimeoutMs())`: the sum is unsigned.
        let watchdog = Duration::from_millis(dur_ms.wrapping_add(settings.timeout_ms as u32) as u64);
        if busy {
            let done = |l: &mut Link| l.store.property(&device, &expose).is_none_or(|p| p.state != PropState::Busy);
            match client.wait(watchdog, Some(cancel), |l| done(l).then_some(())) {
                Ok(()) => {}
                Err(WaitError::Cancelled) => return Err(CaptureError::Interrupted),
                Err(WaitError::Timeout) => {
                    self.first_frame = false;
                    self.disconnect();
                    return Err(CaptureError::Timeout);
                }
            }
        }

        {
            let mut link = client.lock();
            // Discard any frame that came in between.
            link.take_blobs(&device, self.blob);
            let first = link.store.property(&device, &expose).and_then(|p| p.members.first()).map(|m| m.name.clone());
            client.send_number(
                &link,
                &device,
                &expose,
                &[(first.as_deref().unwrap_or_default(), dur_ms as f64 / 1000.0)],
            );
        }
        let r = client.wait(watchdog, Some(cancel), |l| {
            if !l.is_up() || l.generation() != generation {
                return Some(None);
            }
            // The newest frame wins.
            l.take_blobs(&device, self.blob).pop().map(Some)
        });
        let blob = match r {
            Ok(Some(b)) => b,
            Ok(None) => {
                self.disconnect();
                return Err(CaptureError::Disconnected { msg: "INDI server disconnected".into(), reconnect: false });
            }
            Err(WaitError::Cancelled) => return Err(CaptureError::Interrupted),
            Err(WaitError::Timeout) => {
                let video = self.video(&client.lock()).is_some();
                if self.first_frame && video && !self.force_exposure {
                    // Maybe a webcam that only streams. The double space is PHD2's.
                    self.alerts.push(format!("Camera  {device}, exposure error. Trying to use streaming instead."));
                    self.force_video = true;
                    self.first_frame = false;
                    return self.capture(dur_ms, CaptureKind::Light, subframe, settings, cancel);
                }
                self.first_frame = false;
                self.disconnect();
                return Err(CaptureError::Timeout);
            }
        };
        self.first_frame = false;

        // libindi already inflated a `.fits.z`.
        if blob.format != ".fits" {
            return Err(CaptureError::Failed(Some(format!("Unknown image format: {}", blob.format))));
        }
        let read = |sub| read_fits_blob(&blob.data, sub).map_err(|m| CaptureError::Failed(Some(m.into())));
        if take_sub {
            return read(Some((self.frame_size, sf)));
        }
        let img = read(None)?;
        self.frame_size = (img.w, img.h);
        Ok(img)
    }

    /// The camera without an exposure setting: the stream's 8-bit frames
    /// summed for the exposure's length, three at least. The stream is left
    /// on between captures, as in PHD2, which has no watchdog here; a
    /// server drop ends the wait.
    fn capture_video(&mut self, generation: u64, dur_ms: u32, cancel: &CancelToken) -> Result<UsImage, CaptureError> {
        let client = self.client.clone();
        let device = self.device.clone();
        self.first_frame = false;
        let (prop, on, off) = {
            let mut link = client.lock();
            if self.binning_members(&link).is_some() && self.binning != self.cur_binning {
                self.send_binning(&link);
            }
            // No frame size comes with the stream: the maximum over the binning.
            let b = (self.binning as i32).max(1);
            let (mx, my) = self.max_size.get();
            self.frame_size = (mx / b, my / b);
            let Some((prop, on, off)) = self.video(&link) else {
                return Err(CaptureError::Failed(None));
            };
            if link.store.property(&device, &prop).and_then(|p| p.switch(on)) != Some(true) {
                client.send_switch(&link, &device, &prop, &[(on, true), (off, false)]);
            }
            link.take_blobs(&device, self.blob);
            (prop, on, off)
        };
        let mut img = UsImage::new(self.frame_size.0, self.frame_size.1);
        let loopwait = Duration::from_millis(if dur_ms > 100 { 10 } else { 1 });
        let start = Instant::now();
        let mut frames = 0;
        loop {
            thread::sleep(loopwait);
            let (blobs, up) = {
                let mut l = client.lock();
                (l.take_blobs(&device, self.blob), l.is_up() && l.generation() == generation)
            };
            if !up {
                self.disconnect();
                return Err(CaptureError::Disconnected { msg: "INDI server disconnected".into(), reconnect: false });
            }
            for b in blobs {
                // `StackStream`: a frame of another size is dropped.
                if b.data.len() != img.npixels() {
                    self.debug.push(format!(
                        "INDI Camera: discarding blob with size {}, expected {}",
                        b.data.len(),
                        img.npixels()
                    ));
                    continue;
                }
                for (o, i) in img.data.iter_mut().zip(&b.data) {
                    *o = o.wrapping_add(*i as u16);
                }
                frames += 1;
            }
            if start.elapsed() >= Duration::from_millis(dur_ms as u64) && frames > 2 {
                return Ok(img);
            }
            if cancel.is_cancelled() {
                // Streaming stops on the way out of a stop or an exit.
                client.send_switch(&client.lock(), &device, &prop, &[(on, false), (off, true)]);
                return Err(CaptureError::Interrupted);
            }
        }
    }
}

impl GuideCamera for IndiCamera {
    fn name(&self) -> String {
        format!("INDI Camera [{}]", self.device)
    }

    fn is_connected(&self) -> bool {
        self.is_connected_in(&self.client.lock())
    }

    fn frame_size(&self) -> (i32, i32) {
        self.frame_size
    }

    fn binning(&self) -> u8 {
        self.binning
    }

    fn set_binning(&mut self, binning: u8) {
        self.binning = binning;
    }

    fn max_binning(&self) -> u8 {
        self.read_info(&self.client.lock());
        self.max_binning.get()
    }

    /// `GetDevicePixelSize`: `CCD_PIXEL_SIZE`, 0 before `CCD_INFO` comes.
    fn pixel_size_um(&self) -> Option<f64> {
        let link = self.client.lock();
        let info = link.store.property(&self.device, &self.prop("INFO"));
        self.is_connected_in(&link).then(|| info.and_then(|p| p.number("CCD_PIXEL_SIZE")).unwrap_or(0.0))
    }

    fn bits_per_pixel(&self) -> u8 {
        self.read_info(&self.client.lock());
        self.bits.get()
    }

    fn has_bayer(&self) -> bool {
        self.read_info(&self.client.lock());
        self.bayer.get()
    }

    fn pixel_size_xy(&self) -> Option<(f64, f64)> {
        let link = self.client.lock();
        let info = link.store.property(&self.device, &self.prop("INFO"))?;
        Some((info.number("CCD_PIXEL_SIZE_X").unwrap_or(0.0), info.number("CCD_PIXEL_SIZE_Y").unwrap_or(0.0)))
    }

    fn has_subframes(&self) -> bool {
        true
    }

    /// `m_hasGuideOutput`: the driver has the GUIDER interface.
    fn has_guide_output(&self) -> bool {
        self.client.lock().store.device(&self.device).is_some_and(|d| d.driver_interface() & GUIDER_INTERFACE != 0)
    }

    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String> {
        let client = self.client.clone();
        match self.pulse.pulse(&client, &self.device, dir, ms, cancel, "INDI Camera", &mut self.debug) {
            PulseEnd::Done => Ok(()),
            PulseEnd::Missing => {
                self.debug.push("INDI Camera missing pulse guide properties!".into());
                Err("missing pulse guide properties".into())
            }
            PulseEnd::Busy => Err("guide pulse in progress".into()),
            PulseEnd::Interrupted => Err("interrupted".into()),
            PulseEnd::TimedOut => Err("no answer to the pulse".into()),
        }
    }

    fn capture(
        &mut self,
        dur_ms: u32,
        _kind: CaptureKind,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError> {
        if let Some(e) = self.check_connection() {
            return Err(e);
        }
        let Some(generation) = self.attached else { return Err(CaptureError::Failed(None)) };
        let client = self.client.clone();
        let (expose, video) = {
            let link = client.lock();
            self.refresh(&link);
            (self.has(&link, &self.prop("EXPOSURE"), Kind::Number), self.video(&link).is_some())
        };
        if expose && !self.force_video {
            return self.capture_exposure(generation, dur_ms, subframe, settings, cancel);
        }
        if video {
            return self.capture_video(generation, dur_ms, cancel);
        }
        let msg = if self.force_video {
            "Camera has no VIDEO_STREAM property, please uncheck the option: Camera does not support exposure time."
        } else if self.force_exposure {
            "Camera has no CCD_EXPOSURE property, please uncheck the option: Camera does not support stream."
        } else {
            "Camera has no CCD_EXPOSURE or VIDEO_STREAM property"
        };
        self.disconnect();
        Err(CaptureError::Disconnected { msg: msg.into(), reconnect: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indi::fake::{self, Fake, Profile};

    const DEV: &str = "CCD Simulator";

    /// How the fake CCD Simulator behaves.
    #[derive(Clone, Copy)]
    struct Ccd {
        /// Answer an exposure with a frame.
        answer: bool,
        /// Also send a stale 1 × 1 frame first.
        stale: bool,
        format: &'static str,
        /// Define `CCD_VIDEO_STREAM`; streaming sends four 8-bit frames of
        /// value 10 and one of the wrong size.
        video: bool,
    }

    const SIM: Ccd = Ccd { answer: true, stale: false, format: ".fits", video: false };

    /// The properties the CCD Simulator defines once connected: a 64 × 48
    /// sensor, 5.2 µm pixels, binning up to 4, compression on and the
    /// native format selected, an ST-4 port.
    fn connected_props(video: bool) -> String {
        let mut s = fake::def_number(DEV, "CCD_EXPOSURE", "Idle", &[("CCD_EXPOSURE_VALUE", 1.0, 0.0, 3600.0)]);
        s += &fake::def_number(
            DEV,
            "CCD_FRAME",
            "Idle",
            &[("X", 0.0, 0.0, 64.0), ("Y", 0.0, 0.0, 48.0), ("WIDTH", 64.0, 1.0, 64.0), ("HEIGHT", 48.0, 1.0, 48.0)],
        );
        s += &fake::def_number(DEV, "CCD_BINNING", "Idle", &[("HOR_BIN", 1.0, 1.0, 4.0), ("VER_BIN", 1.0, 1.0, 4.0)]);
        s += &fake::def_number(
            DEV,
            "CCD_INFO",
            "Idle",
            &[
                ("CCD_MAX_X", 64.0, 1.0, 16000.0),
                ("CCD_MAX_Y", 48.0, 1.0, 16000.0),
                ("CCD_PIXEL_SIZE", 5.2, 1.0, 40.0),
                ("CCD_PIXEL_SIZE_X", 5.2, 1.0, 40.0),
                ("CCD_PIXEL_SIZE_Y", 5.2, 1.0, 40.0),
                ("CCD_BITSPERPIXEL", 16.0, 8.0, 64.0),
            ],
        );
        s +=
            &fake::def_switch(DEV, "CCD_COMPRESSION", "OneOfMany", &[("INDI_ENABLED", true), ("INDI_DISABLED", false)]);
        s += &fake::def_switch(
            DEV,
            "CCD_TRANSFER_FORMAT",
            "OneOfMany",
            &[("FORMAT_FITS", false), ("FORMAT_NATIVE", true)],
        );
        for (p, a, b) in [
            ("TELESCOPE_TIMED_GUIDE_NS", "TIMED_GUIDE_N", "TIMED_GUIDE_S"),
            ("TELESCOPE_TIMED_GUIDE_WE", "TIMED_GUIDE_W", "TIMED_GUIDE_E"),
        ] {
            s += &fake::def_number(DEV, p, "Idle", &[(a, 0.0, 0.0, 60000.0), (b, 0.0, 0.0, 60000.0)]);
        }
        if video {
            s += &fake::def_switch(DEV, "CCD_VIDEO_STREAM", "OneOfMany", &[("STREAM_ON", false), ("STREAM_OFF", true)]);
        }
        s
    }

    /// A pixel of the fake's frames, which tells the binning apart.
    fn pixel(i: usize, bin: u16) -> u16 {
        (i % 1000) as u16 * 50 + bin
    }

    fn ccd(cfg: Ccd) -> Fake {
        let (mut frame, mut bin) = ([0.0, 0.0, 64.0, 48.0], 1.0);
        Fake::start(move |e, peer| {
            let values = || fake::numbers(e);
            match (e.name.as_str(), e.attr("name").unwrap_or_default()) {
                ("getProperties", _) => {
                    peer.send(&(fake::driver(DEV, 6, false) + &fake::def_blob(DEV, "CCD1", &["CCD1"])))
                }
                ("newSwitchVector", "CONNECTION") => {
                    peer.send(&fake::connection_reply(e).unwrap());
                    peer.send(&connected_props(cfg.video));
                }
                ("newNumberVector", "CCD_FRAME") => {
                    for (i, (_, v)) in values().iter().enumerate() {
                        frame[i] = *v;
                    }
                }
                ("newNumberVector", "CCD_BINNING") => bin = values()[0].1,
                ("newNumberVector", "CCD_EXPOSURE") => {
                    let secs = values()[0].1;
                    peer.send(&fake::set_number(DEV, "CCD_EXPOSURE", "Busy", &[("CCD_EXPOSURE_VALUE", secs)]));
                    if !cfg.answer {
                        return;
                    }
                    let (w, h) = ((frame[2] / bin) as usize, (frame[3] / bin) as usize);
                    let px: Vec<u16> = (0..w * h).map(|i| pixel(i, bin as u16)).collect();
                    let mut reply = String::new();
                    if cfg.stale {
                        reply += &fake::set_blob(DEV, "CCD1", "CCD1", ".fits", &fake::fits(1, 1, &[7]));
                    }
                    reply += &fake::set_blob(DEV, "CCD1", "CCD1", cfg.format, &fake::fits(w, h, &px));
                    reply += &fake::set_number(DEV, "CCD_EXPOSURE", "Ok", &[("CCD_EXPOSURE_VALUE", 0.0)]);
                    peer.send_after((secs * 1000.0) as u64, reply);
                }
                ("newSwitchVector", "CCD_VIDEO_STREAM") if fake::summary(e).contains("STREAM_ON=On") => {
                    let mut frames = fake::set_blob(DEV, "CCD1", "CCD1", ".stream", &[1; 5]);
                    for _ in 0..4 {
                        frames += &fake::set_blob(DEV, "CCD1", "CCD1", ".stream", &[10; 64 * 48]);
                    }
                    peer.send_after(10, frames);
                }
                // The CCD Simulator's ST-4 answers Ok at once.
                ("newNumberVector", p @ ("TELESCOPE_TIMED_GUIDE_NS" | "TELESCOPE_TIMED_GUIDE_WE")) => {
                    let v = values();
                    let members: Vec<(&str, f64)> = v.iter().map(|(n, x)| (n.as_str(), *x)).collect();
                    peer.send(&fake::set_number(DEV, p, "Ok", &members));
                }
                _ => {}
            }
        })
    }

    fn opts() -> CameraOptions {
        CameraOptions { device: DEV.into(), ccd: 0, force_video: false, force_exposure: false, pulse_min_wait: true }
    }

    fn settings(timeout_ms: i32) -> CameraSettings {
        CameraSettings { timeout_ms, ..CameraSettings::load(&Profile::default()) }
    }

    fn connected(fake: &Fake) -> IndiCamera {
        let mut cam = IndiCamera::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts());
        cam.connect(&CancelToken::new()).unwrap();
        cam
    }

    /// What the camera sent the driver, but the `getProperties`.
    fn sent(fake: &Fake) -> Vec<String> {
        fake.received().iter().filter(|e| e.name != "getProperties").map(fake::summary).collect()
    }

    fn capture(cam: &mut IndiCamera, ms: u32, sub: Option<Rect>) -> Result<UsImage, CaptureError> {
        cam.capture(ms, CaptureKind::Light, sub, &settings(15000), &CancelToken::new())
    }

    #[test]
    fn options_from_the_profile() {
        let mut p = Profile::default();
        let o = CameraOptions::from_profile(&p);
        let unset = CameraOptions { device: "INDI Camera".into(), ..opts() };
        assert_eq!(o, unset);
        for (k, v) in [("/indi/INDIcam", "CCD Simulator"), ("/indi/INDIcam_ccd", "1"), ("/indi/PulseMinWait", "0")] {
            p.0.insert(k.into(), v.into());
        }
        let o = CameraOptions::from_profile(&p);
        assert_eq!((o.device.as_str(), o.ccd, o.pulse_min_wait), ("CCD Simulator", 1, false));
        // The guide head: GUIDER_* properties and the CCD2 BLOB.
        let cam = IndiCamera::new(IndiClient::start("127.0.0.1", 9, &[]), &o);
        assert_eq!((cam.prefix, cam.blob), ("GUIDER_", "CCD2"));
        cam.client.shutdown();
        // Nothing chosen: PHD2 would open its chooser.
        let mut cam = IndiCamera::new(IndiClient::start("127.0.0.1", 9, &[]), &unset);
        assert_eq!(cam.connect(&CancelToken::new()), Err("No INDI camera is selected".into()));
        cam.client.shutdown();
    }

    #[test]
    fn a_frame_comes_back_as_a_fits_blob() {
        let fake = ccd(SIM);
        let mut cam = connected(&fake);
        assert_eq!(cam.name(), "INDI Camera [CCD Simulator]");
        assert!(cam.is_connected());
        // Before the first frame the size is unknown.
        assert_eq!(cam.frame_size(), UNDEFINED_FRAME_SIZE);
        let img = capture(&mut cam, 100, None).unwrap();
        assert_eq!((img.w, img.h, img.subframe), (64, 48, None));
        assert_eq!((img.data[0], img.data[65], img.data[3071]), (pixel(0, 1), pixel(65, 1), pixel(3071, 1)));
        assert_eq!(cam.frame_size(), (64, 48));
        assert_eq!((cam.max_binning(), cam.bits_per_pixel(), cam.pixel_size_um()), (4, 16, Some(5.2)));
        assert_eq!((cam.pixel_size_xy(), cam.has_bayer(), cam.has_guide_output()), (Some((5.2, 5.2)), false, true));
        assert_eq!(
            sent(&fake),
            [
                "enableBLOB device=CCD Simulator name=CCD1 Also",
                "newSwitchVector CCD Simulator.CONNECTION CONNECT=On",
                "newSwitchVector CCD Simulator.CCD_COMPRESSION INDI_DISABLED=On",
                "newSwitchVector CCD Simulator.CCD_TRANSFER_FORMAT FORMAT_FITS=On",
                "newNumberVector CCD Simulator.CCD_FRAME X=0 Y=0 WIDTH=64 HEIGHT=48",
                "newNumberVector CCD Simulator.CCD_EXPOSURE CCD_EXPOSURE_VALUE=0.10000000000000000555",
            ]
        );
        assert_eq!(
            cam.take_debug(),
            [
                "INDI Camera connecting to device [CCD Simulator]",
                "Waiting for 30s for [CCD Simulator] to connect...",
                "INDI Camera is ready",
            ]
        );
        cam.client.shutdown();
    }

    #[test]
    fn subframes_and_binning() {
        let fake = ccd(SIM);
        let mut cam = connected(&fake);
        capture(&mut cam, 20, None).unwrap();
        // A subframe once the frame size is known, placed in the full frame.
        let sub = Rect::new(10, 8, 16, 12);
        let img = capture(&mut cam, 20, Some(sub)).unwrap();
        assert_eq!((img.w, img.h, img.subframe), (64, 48, Some(sub)));
        assert_eq!((img.pixel(9, 8), img.pixel(10, 8), img.pixel(11, 9)), (0, pixel(0, 1), pixel(17, 1)));
        // The same area again: no new frame request.
        capture(&mut cam, 20, Some(sub)).unwrap();
        // Binning 2: the subframe is dropped and the full frame asked in
        // unbinned pixels; the frame then tells the binned size.
        cam.set_binning(2);
        let img = capture(&mut cam, 20, Some(sub)).unwrap();
        assert_eq!((img.w, img.h, img.data[1]), (32, 24, pixel(1, 2)));
        assert_eq!(cam.frame_size(), (32, 24));
        let frames: Vec<String> =
            sent(&fake).into_iter().filter(|s| s.contains("CCD_FRAME") || s.contains("BINNING")).collect();
        assert_eq!(
            frames,
            [
                "newNumberVector CCD Simulator.CCD_FRAME X=0 Y=0 WIDTH=64 HEIGHT=48",
                "newNumberVector CCD Simulator.CCD_FRAME X=10 Y=8 WIDTH=16 HEIGHT=12",
                "newNumberVector CCD Simulator.CCD_BINNING HOR_BIN=2 VER_BIN=2",
                "newNumberVector CCD Simulator.CCD_FRAME X=0 Y=0 WIDTH=64 HEIGHT=48",
            ]
        );
        assert!(cam.take_debug().contains(&"INDI Camera: send binning 2".to_string()));
        // A binning past the driver's maximum is clamped when the driver's
        // binning property next changes.
        cam.set_binning(9);
        fake.push(&fake::set_number(DEV, "CCD_BINNING", "Ok", &[("HOR_BIN", 2.0), ("VER_BIN", 2.0)]));
        let ok = |l: &mut Link| l.store.property(DEV, "CCD_BINNING").is_some_and(|p| p.state == PropState::Ok);
        cam.client.wait(Duration::from_secs(2), None, |l| ok(l).then_some(())).unwrap();
        capture(&mut cam, 20, None).unwrap();
        assert_eq!(cam.binning(), 4);
        cam.client.shutdown();
    }

    #[test]
    fn the_newest_frame_wins_and_only_fits_is_read() {
        let fake = ccd(Ccd { stale: true, ..SIM });
        let mut cam = connected(&fake);
        let img = capture(&mut cam, 50, None).unwrap();
        assert_eq!((img.w, img.h), (64, 48));
        cam.client.shutdown();

        let fake = ccd(Ccd { format: ".jpg", ..SIM });
        let mut cam = connected(&fake);
        let unknown = CaptureError::Failed(Some("Unknown image format: .jpg".into()));
        assert_eq!(capture(&mut cam, 50, None).unwrap_err(), unknown);
        assert!(cam.is_connected());
        cam.client.shutdown();
    }

    #[test]
    fn the_watchdog_drops_the_camera() {
        let fake = ccd(Ccd { answer: false, ..SIM });
        let mut cam = connected(&fake);
        let t = Instant::now();
        let r = cam.capture(100, CaptureKind::Light, None, &settings(200), &CancelToken::new());
        assert_eq!(r.unwrap_err(), CaptureError::Timeout);
        assert!(t.elapsed() >= Duration::from_millis(300));
        assert!(!cam.is_connected());
        assert_eq!(cam.take_debug().last().unwrap(), "INDI Camera: serverDisconnected");
        assert!(fake.wait_for(|r| r.iter().any(|e| fake::summary(e).ends_with("name=CCD1 Never"))));
        // Not connected: an error, no alert.
        assert_eq!(capture(&mut cam, 100, None).unwrap_err(), CaptureError::Failed(None));
        cam.client.shutdown();
    }

    #[test]
    fn a_stop_ends_the_wait_for_the_frame() {
        let fake = ccd(SIM);
        let mut cam = connected(&fake);
        let cancel = CancelToken::new();
        let stop = cancel.clone();
        let t = Instant::now();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            stop.cancel();
        });
        let r = cam.capture(2000, CaptureKind::Light, None, &settings(15000), &cancel);
        stopper.join().unwrap();
        assert_eq!(r.unwrap_err(), CaptureError::Interrupted);
        assert!(t.elapsed() < Duration::from_millis(1000));
        assert!(cam.is_connected());
        cam.client.shutdown();
    }

    #[test]
    fn a_camera_that_cannot_expose_falls_back_to_its_stream() {
        let fake = ccd(Ccd { answer: false, video: true, ..SIM });
        let mut cam = connected(&fake);
        let img = cam.capture(100, CaptureKind::Light, None, &settings(100), &CancelToken::new()).unwrap();
        assert_eq!(cam.take_alerts(), ["Camera  CCD Simulator, exposure error. Trying to use streaming instead."]);
        // Four frames of 10 stacked; the 5-byte one is dropped.
        assert_eq!((img.w, img.h), (64, 48));
        assert!(img.data.iter().all(|&v| v == 40));
        assert!(cam.take_debug().contains(&"INDI Camera: discarding blob with size 5, expected 3072".to_string()));
        assert_eq!(sent(&fake).last().unwrap(), "newSwitchVector CCD Simulator.CCD_VIDEO_STREAM STREAM_ON=On");
        cam.client.shutdown();
    }

    #[test]
    fn st4_pulses_wait_out_an_early_answer() {
        let fake = ccd(SIM);
        let mut cam = connected(&fake);
        let cancel = CancelToken::new();
        let t = Instant::now();
        cam.st4_pulse(Dir::North, 150, &cancel).unwrap();
        assert!(t.elapsed() >= Duration::from_millis(150));
        cam.st4_pulse(Dir::East, 50, &cancel).unwrap();
        assert_eq!(
            sent(&fake)[2..],
            [
                "newNumberVector CCD Simulator.TELESCOPE_TIMED_GUIDE_NS TIMED_GUIDE_N=150 TIMED_GUIDE_S=0",
                "newNumberVector CCD Simulator.TELESCOPE_TIMED_GUIDE_WE TIMED_GUIDE_W=0 TIMED_GUIDE_E=50",
            ]
        );
        // PulseMinWait is logged once.
        let debug = cam.take_debug();
        assert_eq!(debug.iter().filter(|l| l.contains("PulseMinWait")).count(), 1);
        // Without it the call returns on the driver's answer.
        cam.pulse.min_wait = false;
        let t = Instant::now();
        cam.st4_pulse(Dir::South, 1000, &cancel).unwrap();
        assert!(t.elapsed() < Duration::from_millis(500));
        cam.client.shutdown();
    }

    #[test]
    fn a_server_drop_disconnects_until_connected_again() {
        let fake = ccd(SIM);
        let mut cam = connected(&fake);
        fake.drop_conns();
        cam.client.wait(Duration::from_secs(3), None, |l| (l.generation() == 2).then_some(())).unwrap();
        assert!(!cam.is_connected());
        let e = cam.check_connection().unwrap();
        assert_eq!(e, CaptureError::Disconnected { msg: "INDI server disconnected".into(), reconnect: false });
        let (alert, reconnect) = e.alert(1000, &settings(15000)).unwrap();
        let text = "INDI server disconnected\nThe camera has been disconnected. Please resolve the problem and \
                    re-connect the camera.";
        assert_eq!((alert.as_str(), reconnect), (text, false));
        assert_eq!(cam.check_connection(), None);
        // Connected again: the BLOB is enabled again with its definition.
        cam.connect(&CancelToken::new()).unwrap();
        assert_eq!(capture(&mut cam, 20, None).unwrap().w, 64);
        let enables = sent(&fake).iter().filter(|s| s.ends_with("name=CCD1 Also")).count();
        assert_eq!(enables, 2);
        // The device switched off once ready: "INDI camera disconnected".
        fake.push(&fake::set_switch(DEV, "CONNECTION", "Ok", &[("CONNECT", false), ("DISCONNECT", true)]));
        let off = |l: &mut Link| l.device_connected(DEV) == Some(false);
        cam.client.wait(Duration::from_secs(2), None, |l| off(l).then_some(())).unwrap();
        let dropped = CaptureError::Disconnected { msg: "INDI camera disconnected".into(), reconnect: false };
        assert_eq!(capture(&mut cam, 20, None).unwrap_err(), dropped);
        cam.client.shutdown();
    }
}
