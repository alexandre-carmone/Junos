//! PHD2's image logger: guide frames saved as FITS so a bad moment can be
//! looked at afterwards — a lost star, a frame whose error stands out, a star
//! auto-selection, or the next N frames on request.
//!
//! A trigger opens an *event*: the two frames before it (kept in a ring as
//! frames go by), the trigger frame and the two after it are saved; another
//! trigger inside the event saves its frame and extends the event by two.
//! The files are `event%03d_%05d_<capture start>_<trigger>.fit` (event
//! number, frame number, `StarLost` / `LargeOffset` / `Series`) in one
//! folder per session under the log folder, `[N_]PHD2_CameraFrames_<log
//! stamp>`; auto-selection frames go there too, as
//! `AutoSelect[Fail]_<capture start>.fit`. A failed auto-selection is saved
//! even with the logger off, as PHD2's settings dialog says.
//!
//! Nothing touches the disk: each call returns the debug-log lines and the
//! files to write, in PHD2's order, and only the session folder's creation
//! goes through the caller ([`SaveCx::mkdir`]).
//!
//! The guider says when to call, at PHD2's call points, through its events:
//! `GuiderEvent::ImageReplaced` for [`ImageLogger::save_image`] (a frame
//! switched in by `UpdateGuideState`, `DisplayImage` — the empty placeholder
//! `LoadProfileSettings` shows included) and `GuiderEvent::ImageLog` for the
//! `LogImage*` calls inside `UpdateCurrentPosition`, among the tracker's
//! debug lines: before `RefineOffset`'s, before the throw's. AutoSelect's
//! second `UpdateGuideState(NULL)` logs the same frame a second time, with
//! no `SaveImage`, as PHD2's does.
//!
//! There is no guard on the number of files. PHD2 clears old folders at
//! start-up with `Debug.RemoveOldDirectories("PHD2_CameraFrames*", 30)`
//! (`logger.cpp:172`), which compares what follows the name's first `_`
//! (`CameraFrames_…`) with a date: a letter never sorts below a digit, so no
//! folder is ever removed, and the port has no clean-up either.
//!
//! Ported from PHD2 src/imagelogger.h, src/imagelogger.cpp, src/myframe.cpp (LoadImageLoggerSettings, SaveImageLoggerSettings) — BSD-3-Clause, see THIRD_PARTY.md

use std::sync::Arc;

use super::cfmt::fmt_f;
use super::debug_log::LogFileStamp;
use crate::engine::calibration::ProfileAccess;
use crate::engine::fits::{SaveHeader, write_fits};
use crate::engine::image::UsImage;
use crate::engine::multistar::GuidingView;
use crate::engine::star::FindResult;

/// The profile keys (`myframe.cpp:806`). The next-N series is not stored.
pub mod keys {
    pub const LOGGING_ENABLED: &str = "/ImageLogger/LoggingEnabled";
    pub const LOG_FRAMES_OVER_THRESH_REL: &str = "/ImageLogger/LogFramesOverThreshRel";
    pub const LOG_FRAMES_OVER_THRESH_PX: &str = "/ImageLogger/LogFramesOverThreshPx";
    pub const LOG_FRAMES_DROPPED: &str = "/ImageLogger/LogFramesDropped";
    pub const LOG_AUTO_SELECT_FRAMES: &str = "/ImageLogger/LogAutoSelectFrames";
    pub const ERROR_THRESH_REL: &str = "/ImageLogger/ErrorThreshRel";
    pub const ERROR_THRESH_PX: &str = "/ImageLogger/ErrorThreshPx";
}

/// Frames saved before and after a trigger (`SAVE_IMAGES`).
const SAVE_IMAGES: usize = 2;
/// Error averages over fewer frames than this trigger nothing.
const MIN_FRAMES_FOR_STATS: u32 = 10;

/// PHD2's `ImageLoggerSettings`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageLoggerSettings {
    pub logging_enabled: bool,
    /// Save a guiding frame whose error is over `guide_error_thresh_rel`
    /// times the smoothed error.
    pub log_frames_over_thresh_rel: bool,
    /// Save a guiding frame whose error is over `guide_error_thresh_px`.
    /// With both on, a frame must be over both.
    pub log_frames_over_thresh_px: bool,
    /// Save a frame whose star was lost while calibrating or guiding.
    pub log_frames_dropped: bool,
    /// Save the frame of a successful auto-selection too.
    pub log_auto_select_frames: bool,
    /// Save the next `log_next_n_frames_count` frames.
    pub log_next_n_frames: bool,
    pub guide_error_thresh_rel: f64,
    pub guide_error_thresh_px: f64,
    pub log_next_n_frames_count: u32,
}

impl Default for ImageLoggerSettings {
    /// What [`ImageLoggerSettings::load`] gives on an empty profile. PHD2's
    /// constructor leaves the thresholds and the count unset, but the
    /// settings are always loaded before the first frame.
    fn default() -> Self {
        ImageLoggerSettings {
            logging_enabled: false,
            log_frames_over_thresh_rel: false,
            log_frames_over_thresh_px: false,
            log_frames_dropped: false,
            log_auto_select_frames: false,
            log_next_n_frames: false,
            guide_error_thresh_rel: 4.0,
            guide_error_thresh_px: 4.0,
            log_next_n_frames_count: 1,
        }
    }
}

impl ImageLoggerSettings {
    /// `LoadImageLoggerSettings` (`myframe.cpp:806`), run with the profile's
    /// other settings: the series always comes back off, count 1.
    pub fn load(p: &dyn ProfileAccess) -> Self {
        let flag = |key: &str| p.get_bool(key, false);
        ImageLoggerSettings {
            logging_enabled: flag(keys::LOGGING_ENABLED),
            log_frames_over_thresh_rel: flag(keys::LOG_FRAMES_OVER_THRESH_REL),
            log_frames_over_thresh_px: flag(keys::LOG_FRAMES_OVER_THRESH_PX),
            log_frames_dropped: flag(keys::LOG_FRAMES_DROPPED),
            log_auto_select_frames: flag(keys::LOG_AUTO_SELECT_FRAMES),
            log_next_n_frames: false,
            guide_error_thresh_rel: p.get_f64(keys::ERROR_THRESH_REL, 4.0),
            guide_error_thresh_px: p.get_f64(keys::ERROR_THRESH_PX, 4.0),
            log_next_n_frames_count: 1,
        }
    }

    /// The settings dialog's OK (`myframe.cpp:3382`): from `current`
    /// (`GetSettings`), only the switch when `ui` turns logging off — the
    /// other fields keep their values — else all of `ui`, the thresholds
    /// held to the spin controls' 1–10 and the count to 1–100. The caller
    /// then applies and saves the result.
    pub fn from_dialog(current: ImageLoggerSettings, ui: ImageLoggerSettings) -> Self {
        if !ui.logging_enabled {
            return ImageLoggerSettings { logging_enabled: false, ..current };
        }
        ImageLoggerSettings {
            guide_error_thresh_rel: ui.guide_error_thresh_rel.clamp(1.0, 10.0),
            guide_error_thresh_px: ui.guide_error_thresh_px.clamp(1.0, 10.0),
            log_next_n_frames_count: ui.log_next_n_frames_count.clamp(1, 100),
            ..ui
        }
    }

    /// `SaveImageLoggerSettings` (`myframe.cpp:823`): everything but the series.
    pub fn save(&self, p: &mut dyn ProfileAccess) {
        p.set_bool(keys::LOGGING_ENABLED, self.logging_enabled);
        p.set_bool(keys::LOG_FRAMES_OVER_THRESH_REL, self.log_frames_over_thresh_rel);
        p.set_bool(keys::LOG_FRAMES_OVER_THRESH_PX, self.log_frames_over_thresh_px);
        p.set_bool(keys::LOG_FRAMES_DROPPED, self.log_frames_dropped);
        p.set_bool(keys::LOG_AUTO_SELECT_FRAMES, self.log_auto_select_frames);
        p.set_f64(keys::ERROR_THRESH_REL, self.guide_error_thresh_rel);
        p.set_f64(keys::ERROR_THRESH_PX, self.guide_error_thresh_px);
    }
}

/// What a call asks of the caller, in PHD2's order.
#[derive(Clone, Debug, PartialEq)]
pub enum ImageLogOut {
    /// A debug-log line.
    Debug(String),
    /// `usImage::Save`: write `bytes` to `path`, relative to
    /// [`SaveCx::log_dir`] (`<session folder>/<file>`), replacing any file
    /// there. A file the caller cannot create is its to debug-log as PHD2
    /// does, `CFITSIO error (105) saving image: couldn't create the named
    /// file`; a failure while writing gives no line (`PHD_fits_close_file`
    /// drops its status, fitsiowrap.cpp:119).
    Save { path: String, bytes: Vec<u8> },
}

/// What saving a frame takes from the rest of PHD2.
pub struct SaveCx<'a> {
    /// `Debug.GetLogDir()`. Its first value, and any change, names the
    /// session folder again.
    pub log_dir: &'a str,
    /// Names the session folder: the instance and the log-file time of the
    /// moment the folder is named. A later rollover does not rename it.
    pub stamp: &'a LogFileStamp,
    /// The cards `usImage::Save` takes from the application, `DATE` being
    /// now. PHD2 reads them when it writes, so the two earlier frames of an
    /// event carry the current lock position and pointing too.
    pub header: &'a SaveHeader,
    /// `wxFileName::Mkdir(path, wxS_DIR_DEFAULT, wxPATH_MKDIR_FULL)`: create
    /// the session folder (`<log_dir>/<folder>`) and its parents; true when
    /// it exists afterwards. Called only when the folder is named.
    pub mkdir: &'a mut dyn FnMut(&str) -> bool,
}

/// What `LogImage(img, distance)` reads from the guider and the controller,
/// inside `UpdateCurrentPosition`: before this frame's
/// `UpdateCurrentDistance` and before the state changes (a CALIBRATED frame
/// is not yet guiding). The guider's `GuidingView` for the frame holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ErrorStats {
    /// `IsGuiding() && !IsPaused() && !PhdController::IsSettling()`.
    pub guiding: bool,
    /// `CurrentErrorFrameCount()`.
    pub frame_count: u32,
    /// `CurrentGuideErrorSmoothed()`: the slow error average, RA only when
    /// the scope's Dec guide mode is None; 100 when no star was ever found,
    /// and once the star has been lost for 20 s (guider.cpp:1122).
    pub smoothed_error: f64,
}

impl From<&GuidingView> for ErrorStats {
    fn from(v: &GuidingView) -> Self {
        ErrorStats { guiding: v.guiding && !v.paused && !v.settling, frame_count: v.error_frame_count, smoothed_error: v.error_smoothed }
    }
}

/// Which `LogImage*` `UpdateCurrentPosition` calls, with what it reads then.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageLogCall {
    /// `LogImage(img, distance)`: the star was found.
    Distance { distance: f64, stats: ErrorStats },
    /// `LogImage(img, FrameDroppedInfo)`: `active` is
    /// `IsCalibratingOrGuiding() && !IsPaused()`.
    Dropped { star_error: FindResult, active: bool },
    /// `LogImageStarDeselected`: no star selected.
    Deselected,
}

/// PHD2's `ImageLogger` (its static `IL`).
#[derive(Debug)]
pub struct ImageLogger {
    /// The frames before the current one, oldest first (`saved_image`).
    saved: [Option<Arc<UsImage>>; SAVE_IMAGES],
    /// Frames still to save in the current event (`imagesToLog`).
    images_to_log: u32,
    event_number: u32,
    /// The current event's trigger, the last part of the file names.
    trigger: &'static str,
    settings: ImageLoggerSettings,
    /// The log folder the session folder was named under, empty until then
    /// or after a failed creation (`debugLogDir`).
    debug_log_dir: String,
    /// The session folder's name (PHD2 keeps the full path).
    subdir: String,
}

impl Default for ImageLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageLogger {
    /// `ImageLogger::Init`, at start-up; [`apply_settings`](Self::apply_settings)
    /// with the profile's settings follows.
    pub fn new() -> Self {
        ImageLogger {
            saved: [None, None],
            images_to_log: 0,
            event_number: 1,
            trigger: "",
            settings: ImageLoggerSettings::default(),
            debug_log_dir: String::new(),
            subdir: String::new(),
        }
    }

    /// Whether a frame handed in now could be saved: logging on, or an
    /// event's frames still to come.
    pub fn may_save(&self) -> bool {
        self.settings.logging_enabled || self.images_to_log > 0
    }

    /// `GetSettings`: while a series runs, its count is the frames left
    /// (0 once it is done, the series still on).
    pub fn settings(&self) -> ImageLoggerSettings {
        let mut s = self.settings;
        if s.log_next_n_frames {
            s.log_next_n_frames_count = self.images_to_log;
        }
        s
    }

    /// `ApplySettings`; returns its debug-log line. Turning a series on
    /// starts it (or lengthens the event under way); settings without a
    /// series, as every profile load gives, end the event under way without
    /// moving to the next event number, so the next trigger reuses it.
    pub fn apply_settings(&mut self, s: ImageLoggerSettings) -> String {
        let line = format!(
            "ImgLogger: Settings LogEnabled={} Log Rel={}, {} Log Px={}, {} LogFrameDrop={} LogAutoSel={} NextN={}",
            s.logging_enabled as i32,
            s.log_frames_over_thresh_rel as i32,
            fmt_f(if s.log_frames_over_thresh_rel { s.guide_error_thresh_rel } else { 0.0 }, 2),
            s.log_frames_over_thresh_px as i32,
            fmt_f(if s.log_frames_over_thresh_px { s.guide_error_thresh_px } else { 0.0 }, 2),
            s.log_frames_dropped as i32,
            s.log_auto_select_frames as i32,
            // `%d` of an unsigned.
            if s.log_next_n_frames { s.log_next_n_frames_count as i32 } else { 0 },
        );
        self.settings = s;
        if s.logging_enabled && s.log_next_n_frames && self.images_to_log < s.log_next_n_frames_count {
            self.images_to_log = s.log_next_n_frames_count;
            self.trigger = "Series";
        }
        if !s.logging_enabled || !s.log_next_n_frames {
            self.images_to_log = 0;
        }
        line
    }

    /// `SaveImage`: the guider's current image was replaced, on its
    /// `ImageReplaced` event. Accepted deviation: the ring can take the
    /// guider's empty placeholder (frame 0), which an early trigger saves
    /// named with the 1970 epoch, where PHD2 formats an invalid date.
    pub fn save_image(&mut self, prev: Arc<UsImage>) {
        self.saved.rotate_left(1);
        self.saved[SAVE_IMAGES - 1] = Some(prev);
    }

    /// `LogImageStarDeselected`: the frame was looked at without a star
    /// selected (the tracker's "No star selected", a `PositionLost` without
    /// `reset_auto_exposure`). It only continues an event.
    pub fn log_star_deselected(&mut self, img: &UsImage, cx: &mut SaveCx) -> Vec<ImageLogOut> {
        let mut out = Vec::new();
        self.continue_logging(img, cx, &mut out);
        out
    }

    /// `LogImage(img, FrameDroppedInfo)`: the star was lost (`star_error`,
    /// any other `PositionLost`). `active` is `IsCalibratingOrGuiding() &&
    /// !IsPaused()` inside `UpdateCurrentPosition`.
    pub fn log_dropped(
        &mut self,
        img: &UsImage,
        star_error: FindResult,
        active: bool,
        cx: &mut SaveCx,
    ) -> Vec<ImageLogOut> {
        let mut out = Vec::new();
        if self.settings.logging_enabled && self.settings.log_frames_dropped && active {
            out.push(ImageLogOut::Debug(format!(
                "ImgLogger: star lost ({}) frame {} event {}",
                star_error as i32, img.frame_num, self.event_number
            )));
            self.begin_logging(img, "StarLost", cx, &mut out);
        } else {
            self.continue_logging(img, cx, &mut out);
        }
        out
    }

    /// `LogImage(img, distance)`: the star was found `distance` px from the
    /// lock position (the tracker's `raw_distance`).
    pub fn log_distance(
        &mut self,
        img: &UsImage,
        distance: f64,
        stats: ErrorStats,
        cx: &mut SaveCx,
    ) -> Vec<ImageLogOut> {
        let mut out = Vec::new();
        let s = self.settings;
        if s.logging_enabled
            && (s.log_frames_over_thresh_rel || s.log_frames_over_thresh_px)
            && stats.guiding
            && stats.frame_count >= MIN_FRAMES_FOR_STATS
        {
            // wxMax: a > b ? a : b, so a NaN error gives the floor too.
            let cur_err = if stats.smoothed_error > 0.001 { stats.smoothed_error } else { 0.001 };
            let rel_err = distance / cur_err;
            let thresh_px = if s.log_frames_over_thresh_px { s.guide_error_thresh_px } else { 99.0 };
            let thresh_rel = if s.log_frames_over_thresh_rel { s.guide_error_thresh_rel } else { 99.0 };
            let logit = if s.log_frames_over_thresh_px && s.log_frames_over_thresh_rel {
                distance > thresh_px && rel_err > thresh_rel
            } else if s.log_frames_over_thresh_px {
                distance > thresh_px
            } else {
                rel_err > thresh_rel
            };
            if logit {
                out.push(ImageLogOut::Debug(format!(
                    "ImgLogger: large offset frame {} event {} dist px {} vs {} rel {} vs {} cur {}",
                    img.frame_num,
                    self.event_number,
                    fmt_f(distance, 2),
                    fmt_f(thresh_px, 2),
                    fmt_f(rel_err, 2),
                    fmt_f(thresh_rel, 2),
                    fmt_f(cur_err, 2),
                )));
                self.begin_logging(img, "LargeOffset", cx, &mut out);
                return out;
            }
        }
        self.continue_logging(img, cx, &mut out);
        out
    }

    /// `LogAutoSelectImage`: an auto-selection ran on `img` (the guider's
    /// current image after `auto_select`; `succeeded` when it gave a star).
    /// A success is saved only when asked for, a failure always. It is
    /// outside the events: no event number, no effect on them.
    pub fn log_auto_select(&mut self, img: &UsImage, succeeded: bool, cx: &mut SaveCx) -> Vec<ImageLogOut> {
        let mut out = Vec::new();
        // AutoSelect's own `image->ImageData` check (guider_multistar.cpp:521).
        if img.data.is_empty() {
            return out;
        }
        if succeeded && !(self.settings.logging_enabled && self.settings.log_auto_select_frames) {
            return out;
        }
        let base = if succeeded { "AutoSelect" } else { "AutoSelectFail" };
        let file = format!("{base}_{}.fit", img.img_start_time.format("%Y-%m-%d_%H%M%S"));
        out.push(ImageLogOut::Debug(format!("ImgLogger: saving auto-select image {file}")));
        self.save_to(img, file, cx, &mut out);
        out
    }

    // ------------------------------------------------------------ events

    fn begin_logging(&mut self, img: &UsImage, trigger: &'static str, cx: &mut SaveCx, out: &mut Vec<ImageLogOut>) {
        self.trigger = trigger;
        if self.images_to_log == 0 {
            // The frames before this one, under the new trigger. Inside an
            // event they are skipped as saved already — also when it is a
            // series that has not saved them yet.
            for prev in self.saved.clone().iter().flatten() {
                self.log_image(prev, cx, out);
            }
        }
        self.log_image(img, cx, out);
        if self.images_to_log < SAVE_IMAGES as u32 {
            self.images_to_log = SAVE_IMAGES as u32;
        }
    }

    fn continue_logging(&mut self, img: &UsImage, cx: &mut SaveCx, out: &mut Vec<ImageLogOut>) {
        if self.images_to_log > 0 {
            self.log_image(img, cx, out);
            self.images_to_log -= 1;
            if self.images_to_log == 0 {
                self.event_number += 1;
            }
        }
    }

    /// `IL::LogImage(img)`: an event's file.
    fn log_image(&mut self, img: &UsImage, cx: &mut SaveCx, out: &mut Vec<ImageLogOut>) {
        out.push(ImageLogOut::Debug(format!(
            "ImgLogger: LogImage event {} frame {}",
            self.event_number, img.frame_num
        )));
        let file = format!(
            "event{:03}_{:05}_{}_{}.fit",
            self.event_number,
            img.frame_num,
            img.img_start_time.format("%Y-%m-%d_%H%M%S"),
            self.trigger
        );
        self.save_to(img, file, cx, out);
    }

    /// `IL::LogImage(img, filename)`: names and creates the session folder
    /// when the log folder is new, then saves.
    fn save_to(&mut self, img: &UsImage, file: String, cx: &mut SaveCx, out: &mut Vec<ImageLogOut>) {
        if cx.log_dir != self.debug_log_dir {
            self.debug_log_dir = cx.log_dir.to_string();
            // Separate folders for several instances, the qualifier in front.
            let qualifier = if cx.stamp.instance > 1 { format!("{}_", cx.stamp.instance) } else { String::new() };
            self.subdir = format!("{qualifier}{}", cx.stamp.time.format("PHD2_CameraFrames_%Y-%m-%d-%H%M%S"));
            let full = format!("{}/{}", cx.log_dir, self.subdir);
            if !(cx.mkdir)(&full) {
                out.push(ImageLogOut::Debug(format!("Error: Could not create frame logging directory {full}")));
                // So the next frame names and tries it again.
                self.debug_log_dir.clear();
                return;
            }
        }
        match write_fits(img, cx.header) {
            Ok(bytes) => out.push(ImageLogOut::Save { path: format!("{}/{file}", self.subdir), bytes }),
            // cfitsio's BAD_F2C, the only error write_fits has. PHD2 is left
            // with a file whose header stops at the bad card; none here.
            Err(e) => out.push(ImageLogOut::Debug(format!("CFITSIO error (402) saving image: {e}"))),
        }
    }
}

// The ImgLogger debug lines and the file names below are what PHD2's own
// imagelogger.cpp printed for the same calls in a throwaway harness.
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::{DateTime, Local, TimeDelta, TimeZone, Utc};

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

    fn local(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 11, 11, h, m, s).unwrap()
    }

    /// Frame `n`, started at 22:15:00 + n s.
    fn frame(n: u32) -> Arc<UsImage> {
        let mut img = UsImage::new(2, 2);
        img.data = vec![100, 200, 300, 400];
        img.frame_num = n;
        img.img_exp_dur = 1500;
        img.img_start_time = local(22, 15, 0) + TimeDelta::seconds(n as i64);
        Arc::new(img)
    }

    fn header() -> SaveHeader {
        SaveHeader {
            date: Utc.with_ymd_and_hms(2026, 11, 11, 21, 15, 30).unwrap(),
            creator: "PHD2 2.6.14".into(),
            profile: "Simulator".into(),
            pixel_scale: 1.5,
            ..Default::default()
        }
    }

    struct Bench {
        il: ImageLogger,
        log_dir: String,
        stamp: LogFileStamp,
        header: SaveHeader,
        mkdir_ok: bool,
        mkdirs: Vec<String>,
    }

    impl Bench {
        fn new(s: ImageLoggerSettings) -> Bench {
            let mut il = ImageLogger::new();
            il.apply_settings(s);
            Bench {
                il,
                log_dir: "/logs".into(),
                stamp: LogFileStamp { instance: 1, time: local(21, 30, 5) },
                header: header(),
                mkdir_ok: true,
                mkdirs: Vec::new(),
            }
        }

        /// One call; the debug lines as written, a save as `SAVE <path>`.
        fn run(&mut self, f: impl FnOnce(&mut ImageLogger, &mut SaveCx) -> Vec<ImageLogOut>) -> Vec<String> {
            let Bench { il, log_dir, stamp, header, mkdir_ok, mkdirs } = self;
            let ok = *mkdir_ok;
            let mut mkdir = |d: &str| {
                mkdirs.push(d.to_string());
                ok
            };
            let mut cx = SaveCx { log_dir, stamp, header, mkdir: &mut mkdir };
            f(il, &mut cx)
                .into_iter()
                .map(|o| match o {
                    ImageLogOut::Debug(line) => line,
                    ImageLogOut::Save { path, bytes } => {
                        assert!(bytes.starts_with(b"SIMPLE  ="));
                        format!("SAVE {path}")
                    }
                })
                .collect()
        }

        /// The previous frame goes to the ring, then `n` is looked at with
        /// the star found `distance` px away.
        fn found(&mut self, n: u32, distance: f64, stats: ErrorStats) -> Vec<String> {
            self.il.save_image(frame(n - 1));
            self.run(|il, cx| il.log_distance(&frame(n), distance, stats, cx))
        }

        fn lost(&mut self, n: u32, active: bool) -> Vec<String> {
            self.il.save_image(frame(n - 1));
            self.run(|il, cx| il.log_dropped(&frame(n), FindResult::LowMass, active, cx))
        }
    }

    const NOT_GUIDING: ErrorStats = ErrorStats { guiding: false, frame_count: 0, smoothed_error: 0.0 };

    fn guiding(frame_count: u32, smoothed_error: f64) -> ErrorStats {
        ErrorStats { guiding: true, frame_count, smoothed_error }
    }

    fn enabled() -> ImageLoggerSettings {
        ImageLoggerSettings { logging_enabled: true, ..Default::default() }
    }

    const DIR: &str = "PHD2_CameraFrames_2026-11-11-213005";

    fn saved(event: u32, frame: u32, trigger: &str) -> [String; 2] {
        [
            format!("ImgLogger: LogImage event {event} frame {frame}"),
            format!("SAVE {DIR}/event{event:03}_{frame:05}_2026-11-11_2215{frame:02}_{trigger}.fit"),
        ]
    }

    #[test]
    fn settings_load_their_defaults_and_save_all_but_the_series() {
        let mut p = Profile::default();
        let s = ImageLoggerSettings::load(&p);
        assert_eq!(s, ImageLoggerSettings::default());
        assert_eq!((s.guide_error_thresh_rel, s.guide_error_thresh_px, s.log_next_n_frames_count), (4.0, 4.0, 1));
        assert!(!s.logging_enabled && !s.log_frames_dropped && !s.log_next_n_frames);

        let mine = ImageLoggerSettings {
            logging_enabled: true,
            log_frames_over_thresh_rel: true,
            log_frames_over_thresh_px: false,
            log_frames_dropped: true,
            log_auto_select_frames: true,
            log_next_n_frames: true,
            guide_error_thresh_rel: 2.5,
            guide_error_thresh_px: 6.0,
            log_next_n_frames_count: 20,
        };
        mine.save(&mut p);
        assert_eq!(p.i.len() + p.f.len(), 7);
        assert_eq!(p.i[keys::LOGGING_ENABLED], 1);
        assert_eq!(p.i[keys::LOG_FRAMES_OVER_THRESH_PX], 0);
        assert_eq!(p.f[keys::ERROR_THRESH_REL], 2.5);
        // The series comes back off, count 1.
        let back = ImageLoggerSettings::load(&p);
        assert_eq!(back, ImageLoggerSettings { log_next_n_frames: false, log_next_n_frames_count: 1, ..mine });
    }

    #[test]
    fn apply_settings_debug_line() {
        let mut il = ImageLogger::new();
        assert_eq!(
            il.apply_settings(ImageLoggerSettings::default()),
            "ImgLogger: Settings LogEnabled=0 Log Rel=0, 0.00 Log Px=0, 0.00 LogFrameDrop=0 LogAutoSel=0 NextN=0"
        );
        let s = ImageLoggerSettings {
            log_frames_over_thresh_rel: true,
            log_frames_over_thresh_px: true,
            log_frames_dropped: true,
            log_auto_select_frames: true,
            log_next_n_frames: true,
            guide_error_thresh_rel: 3.5,
            // Just under 2.675 in binary: C's %.2f gives 2.67.
            guide_error_thresh_px: 2.675,
            log_next_n_frames_count: 5,
            ..enabled()
        };
        assert_eq!(
            il.apply_settings(s),
            "ImgLogger: Settings LogEnabled=1 Log Rel=1, 3.50 Log Px=1, 2.67 LogFrameDrop=1 LogAutoSel=1 NextN=5"
        );
        // A threshold that is off prints 0.
        let s = ImageLoggerSettings { log_frames_over_thresh_px: false, log_next_n_frames: false, ..s };
        assert_eq!(
            il.apply_settings(s),
            "ImgLogger: Settings LogEnabled=1 Log Rel=1, 3.50 Log Px=0, 0.00 LogFrameDrop=1 LogAutoSel=1 NextN=0"
        );
    }

    #[test]
    fn a_lost_star_saves_two_frames_before_and_two_after() {
        let mut b = Bench::new(ImageLoggerSettings { log_frames_dropped: true, ..enabled() });
        b.il.save_image(frame(1));
        assert!(b.found(3, 0.5, NOT_GUIDING).is_empty()); // ring: 1, 2

        let mut want = vec!["ImgLogger: star lost (3) frame 4 event 1".to_string()];
        for n in 2..=4 {
            want.extend(saved(1, n, "StarLost"));
        }
        assert_eq!(b.lost(4, true), want);
        assert_eq!(b.mkdirs, [format!("/logs/{DIR}")]);

        // The two after it, whatever happens to them; then the next event.
        assert_eq!(b.found(5, 0.5, NOT_GUIDING), saved(1, 5, "StarLost"));
        b.il.save_image(frame(5));
        assert_eq!(b.run(|il, cx| il.log_star_deselected(&frame(6), cx)), saved(1, 6, "StarLost"));
        assert!(b.found(7, 0.5, NOT_GUIDING).is_empty());

        // Frame 6, saved in event 1, is saved again in event 2.
        let mut want = vec!["ImgLogger: star lost (3) frame 8 event 2".to_string()];
        for n in 6..=8 {
            want.extend(saved(2, n, "StarLost"));
        }
        assert_eq!(b.lost(8, true), want);
        assert_eq!(b.mkdirs.len(), 1);
    }

    #[test]
    fn a_lost_star_is_saved_only_while_calibrating_or_guiding_and_when_asked() {
        let mut b = Bench::new(ImageLoggerSettings { log_frames_dropped: true, ..enabled() });
        assert!(b.lost(4, false).is_empty());
        let mut b = Bench::new(ImageLoggerSettings { log_frames_dropped: true, ..Default::default() });
        assert!(b.lost(4, true).is_empty());
        let mut b = Bench::new(enabled());
        assert!(b.lost(4, true).is_empty());
        assert!(b.mkdirs.is_empty());
    }

    /// The first line of `log_distance` on a fresh logger, or "" when the
    /// frame is not saved.
    fn large_offset(s: ImageLoggerSettings, distance: f64, stats: ErrorStats) -> String {
        let mut b = Bench::new(s);
        b.found(12, distance, stats).into_iter().next().unwrap_or_default()
    }

    #[test]
    fn large_offset_over_the_pixel_threshold() {
        let px = ImageLoggerSettings { log_frames_over_thresh_px: true, ..enabled() };
        assert_eq!(
            large_offset(px, 4.01, guiding(10, 0.5)),
            "ImgLogger: large offset frame 12 event 1 dist px 4.01 vs 4.00 rel 8.02 vs 99.00 cur 0.50"
        );
        assert_eq!(large_offset(px, 4.0, guiding(10, 0.5)), "");
        // Fewer than 10 frames of error statistics.
        assert_eq!(large_offset(px, 9.0, guiding(9, 0.5)), "");
        // Not guiding, paused or settling.
        assert_eq!(large_offset(px, 9.0, ErrorStats { guiding: false, ..guiding(50, 0.5) }), "");
        // Off.
        assert_eq!(large_offset(ImageLoggerSettings { logging_enabled: false, ..px }, 9.0, guiding(50, 0.5)), "");
        assert_eq!(large_offset(enabled(), 9.0, guiding(50, 0.5)), "");
    }

    #[test]
    fn large_offset_over_the_relative_threshold() {
        let rel = ImageLoggerSettings { log_frames_over_thresh_rel: true, guide_error_thresh_rel: 3.0, ..enabled() };
        assert_eq!(
            large_offset(rel, 1.6, guiding(10, 0.5)),
            "ImgLogger: large offset frame 12 event 1 dist px 1.60 vs 99.00 rel 3.20 vs 3.00 cur 0.50"
        );
        assert_eq!(large_offset(rel, 1.5, guiding(10, 0.5)), "");
        // The smoothed error is floored at 0.001.
        assert_eq!(
            large_offset(rel, 0.004, guiding(10, 0.0)),
            "ImgLogger: large offset frame 12 event 1 dist px 0.00 vs 99.00 rel 4.00 vs 3.00 cur 0.00"
        );
        assert_eq!(large_offset(rel, 0.004, guiding(10, f64::NAN)), large_offset(rel, 0.004, guiding(10, 0.0)));
        // Both on: a frame must be over both.
        let both = ImageLoggerSettings { log_frames_over_thresh_px: true, ..rel };
        assert_eq!(large_offset(both, 5.0, guiding(10, 2.0)), "");
        assert_eq!(large_offset(both, 3.0, guiding(10, 0.5)), "");
        assert_eq!(
            large_offset(both, 5.0, guiding(10, 1.0)),
            "ImgLogger: large offset frame 12 event 1 dist px 5.00 vs 4.00 rel 5.00 vs 3.00 cur 1.00"
        );
    }

    #[test]
    fn a_trigger_inside_an_event_extends_it_under_its_own_name() {
        let mut b =
            Bench::new(ImageLoggerSettings { log_frames_over_thresh_px: true, log_frames_dropped: true, ..enabled() });
        b.il.save_image(frame(1));
        b.il.save_image(frame(2));
        let out = b.run(|il, cx| il.log_distance(&frame(3), 5.0, guiding(10, 1.0), cx));
        assert_eq!(out[0], "ImgLogger: large offset frame 3 event 1 dist px 5.00 vs 4.00 rel 5.00 vs 99.00 cur 1.00");
        assert_eq!(
            out[1..],
            [saved(1, 1, "LargeOffset"), saved(1, 2, "LargeOffset"), saved(1, 3, "LargeOffset")].concat()
        );
        assert_eq!(b.found(4, 0.5, guiding(11, 1.0)), saved(1, 4, "LargeOffset"));
        // The star lost with one frame to go: no earlier frames, two more after.
        let out = b.lost(5, true);
        assert_eq!(
            out,
            [vec!["ImgLogger: star lost (3) frame 5 event 1".to_string()], saved(1, 5, "StarLost").to_vec()].concat()
        );
        assert_eq!(b.found(6, 0.5, guiding(12, 1.0)), saved(1, 6, "StarLost"));
        assert_eq!(b.found(7, 0.5, guiding(13, 1.0)), saved(1, 7, "StarLost"));
        assert!(b.found(8, 0.5, guiding(14, 1.0)).is_empty());
        assert_eq!(b.found(9, 5.0, guiding(15, 1.0))[1], saved(2, 7, "LargeOffset")[0]);
    }

    #[test]
    fn a_series_saves_the_next_n_frames() {
        let series = ImageLoggerSettings { log_next_n_frames: true, log_next_n_frames_count: 3, ..enabled() };
        let mut b = Bench::new(series);
        assert_eq!(b.il.settings().log_next_n_frames_count, 3);
        assert_eq!(b.found(1, 0.0, NOT_GUIDING), saved(1, 1, "Series"));
        assert_eq!(b.il.settings().log_next_n_frames_count, 2);
        b.il.save_image(frame(1));
        assert_eq!(b.run(|il, cx| il.log_star_deselected(&frame(2), cx)), saved(1, 2, "Series"));
        // A shorter series does not cut the one under way.
        b.il.apply_settings(ImageLoggerSettings { log_next_n_frames_count: 1, ..series });
        assert_eq!(b.il.settings().log_next_n_frames_count, 1);
        assert_eq!(b.lost(3, false), saved(1, 3, "Series"));
        // Done: the count reads 0, the series still on.
        assert_eq!(b.il.settings().log_next_n_frames_count, 0);
        assert!(b.il.settings().log_next_n_frames);
        assert!(b.found(4, 0.0, NOT_GUIDING).is_empty());
        // Applied again, it starts over as event 2.
        b.il.apply_settings(series);
        assert_eq!(b.found(5, 0.0, NOT_GUIDING), saved(2, 5, "Series"));
        // Without logging enabled there is no series.
        let mut b = Bench::new(ImageLoggerSettings { logging_enabled: false, ..series });
        assert_eq!(b.il.settings().log_next_n_frames_count, 0);
        assert!(b.found(1, 0.0, NOT_GUIDING).is_empty());
    }

    #[test]
    fn settings_without_a_series_end_the_event_and_keep_its_number() {
        let s = ImageLoggerSettings { log_frames_dropped: true, ..enabled() };
        let mut b = Bench::new(s);
        b.il.save_image(frame(1));
        b.lost(3, true);
        // A profile load in the middle of the event.
        b.il.apply_settings(s);
        assert!(b.found(4, 0.5, NOT_GUIDING).is_empty());
        assert_eq!(b.lost(5, true)[1..3], saved(1, 3, "StarLost"));
    }

    #[test]
    fn auto_select_frames() {
        let mut b = Bench::new(ImageLoggerSettings::default());
        assert!(b.run(|il, cx| il.log_auto_select(&frame(1), true, cx)).is_empty());
        // A failure is saved even with the logger off.
        assert_eq!(
            b.run(|il, cx| il.log_auto_select(&frame(1), false, cx)),
            [
                "ImgLogger: saving auto-select image AutoSelectFail_2026-11-11_221501.fit".to_string(),
                format!("SAVE {DIR}/AutoSelectFail_2026-11-11_221501.fit"),
            ]
        );
        // Not on an empty image (AutoSelect's own check).
        assert!(b.run(|il, cx| il.log_auto_select(&UsImage::default(), false, cx)).is_empty());
        // Asked for while a series runs: no event number, the series goes on.
        let s = ImageLoggerSettings {
            log_auto_select_frames: true,
            log_next_n_frames: true,
            log_next_n_frames_count: 2,
            ..enabled()
        };
        let mut b = Bench::new(s);
        assert_eq!(
            b.run(|il, cx| il.log_auto_select(&frame(2), true, cx))[1],
            format!("SAVE {DIR}/AutoSelect_2026-11-11_221502.fit")
        );
        assert_eq!(b.il.settings().log_next_n_frames_count, 2);
        let mut b = Bench::new(ImageLoggerSettings { log_auto_select_frames: false, ..s });
        assert!(b.run(|il, cx| il.log_auto_select(&frame(2), true, cx)).is_empty());
    }

    #[test]
    fn session_folder_naming() {
        let s = ImageLoggerSettings { log_next_n_frames: true, log_next_n_frames_count: 10, ..enabled() };
        let mut b = Bench::new(s);
        b.stamp.instance = 2;
        assert_eq!(
            b.found(1, 0.0, NOT_GUIDING)[1],
            "SAVE 2_PHD2_CameraFrames_2026-11-11-213005/event001_00001_2026-11-11_221501_Series.fit"
        );
        // A rollover does not rename the folder.
        b.stamp.time = local(9, 0, 0) + TimeDelta::days(1);
        assert_eq!(
            b.found(2, 0.0, NOT_GUIDING)[1],
            "SAVE 2_PHD2_CameraFrames_2026-11-11-213005/event001_00002_2026-11-11_221502_Series.fit"
        );
        assert_eq!(b.mkdirs, ["/logs/2_PHD2_CameraFrames_2026-11-11-213005"]);
        // A new log folder does, with the stamp of the moment.
        b.log_dir = "/data/logs".into();
        assert_eq!(
            b.found(3, 0.0, NOT_GUIDING)[1],
            "SAVE 2_PHD2_CameraFrames_2026-11-12-090000/event001_00003_2026-11-11_221503_Series.fit"
        );
        assert_eq!(b.mkdirs[1], "/data/logs/2_PHD2_CameraFrames_2026-11-12-090000");

        // A folder that can't be created: nothing saved, tried again next frame.
        b.log_dir = "/ro".into();
        b.mkdir_ok = false;
        assert_eq!(
            b.found(4, 0.0, NOT_GUIDING),
            [
                "ImgLogger: LogImage event 1 frame 4",
                "Error: Could not create frame logging directory /ro/2_PHD2_CameraFrames_2026-11-12-090000",
            ]
        );
        b.mkdir_ok = true;
        assert_eq!(
            b.found(5, 0.0, NOT_GUIDING)[1],
            "SAVE 2_PHD2_CameraFrames_2026-11-12-090000/event001_00005_2026-11-11_221505_Series.fit"
        );
        assert_eq!(b.mkdirs.len(), 4);
    }

    #[test]
    fn the_file_is_usimage_save_with_the_callers_header() {
        let s = ImageLoggerSettings { log_next_n_frames: true, log_next_n_frames_count: 2, ..enabled() };
        let mut b = Bench::new(s);
        let img = frame(1);
        let out = {
            let Bench { il, log_dir, stamp, header, .. } = &mut b;
            let mut mkdir = |_: &str| true;
            il.log_distance(&img, 0.0, NOT_GUIDING, &mut SaveCx { log_dir, stamp, header, mkdir: &mut mkdir })
        };
        let ImageLogOut::Save { bytes, .. } = &out[1] else { panic!("{out:?}") };
        assert_eq!(*bytes, write_fits(&img, &header()).unwrap());
        // A float card cfitsio refuses: the error line, no file.
        b.header.pixel_scale = f64::NAN;
        assert_eq!(b.found(2, 0.0, NOT_GUIDING)[1], "CFITSIO error (402) saving image: bad float to string conversion");
    }

    #[test]
    fn the_dialog_keeps_the_rest_when_logging_goes_off_and_clamps_its_spins() {
        let current = ImageLoggerSettings {
            logging_enabled: true,
            log_frames_dropped: true,
            guide_error_thresh_px: 2.5,
            log_next_n_frames_count: 7,
            ..ImageLoggerSettings::default()
        };
        let ui = ImageLoggerSettings {
            logging_enabled: false,
            log_frames_dropped: false,
            guide_error_thresh_px: 50.0,
            log_next_n_frames_count: 0,
            ..ImageLoggerSettings::default()
        };
        assert_eq!(ImageLoggerSettings::from_dialog(current, ui), ImageLoggerSettings { logging_enabled: false, ..current });
        let on = ImageLoggerSettings { logging_enabled: true, guide_error_thresh_rel: 0.2, log_next_n_frames: true, ..ui };
        let s = ImageLoggerSettings::from_dialog(current, on);
        assert_eq!(
            (s.log_frames_dropped, s.guide_error_thresh_rel, s.guide_error_thresh_px, s.log_next_n_frames_count),
            (false, 1.0, 10.0, 1)
        );
        let s = ImageLoggerSettings::from_dialog(current, ImageLoggerSettings { log_next_n_frames_count: 500, ..on });
        assert_eq!(s.log_next_n_frames_count, 100);
        // Past the dialog, PHD2's %d of the count goes negative.
        let big = ImageLoggerSettings { log_next_n_frames_count: 3_000_000_000, ..on };
        assert!(ImageLogger::new().apply_settings(big).ends_with("NextN=-1294967296"));
    }
}
