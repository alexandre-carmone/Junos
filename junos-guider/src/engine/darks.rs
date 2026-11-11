//! The dark library and the bad-pixel (defect) map: building a master dark
//! from `N` dark frames and the dark dialog's build sequence around it,
//! keeping one per exposure and picking the one to subtract, the defect-map
//! builder (a 15 px median-filtered dark, hot and cold pixels by their
//! deviation from it), the defect map's text file and the per-pixel repair,
//! and the frame-size check that drops darks and maps that no longer fit
//! the camera.
//!
//! Capture pipeline (cam_indi.cpp:1130, worker_thread.cpp:224): the raw frame
//! goes through [`DarkLibrary::subtract_dark`] — the defect map when one is
//! loaded, else the selected dark — then the Bayer `quick_l_recon`, the
//! `square_pixels`, the noise reduction, and only then `calc_stats`. Dark
//! frames (`CAPTURE_DARK`) skip all of it.
//!
//! No I/O: files come in and go out as bytes and strings (FITS through
//! [`super::fits`]); the caller owns the paths, named as PHD2 names them.
//!
//! Ported from PHD2 src/camera.cpp, src/image_math.h, src/image_math.cpp, src/darks_dialog.cpp, src/Refine_DefMap.cpp, src/myframe.cpp, src/myframe_events.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::BTreeMap;

use chrono::{DateTime, Local};

use super::fits;
use super::image::{UsImage, subtract, window_median};
use super::log::cfmt::{fmt_f, fmt_g};
use super::traits::Point;

// ---------------------------------------------------------------- settings

/// `/camera/darks_num_frames` default (`DefDarkCount`, darks_dialog.cpp:43)
/// and the range of its spin control.
pub const DARKS_NUM_FRAMES_DEFAULT: i32 = 5;
pub const DARKS_NUM_FRAMES_RANGE: (i32, i32) = (1, 20);
/// `/camera/dmap_exptime` default in seconds (`DefDMExpTime`) and range.
pub const DMAP_EXPTIME_DEFAULT: i32 = 15;
pub const DMAP_EXPTIME_RANGE: (i32, i32) = (5, 15);
/// `/camera/dmap_num_frames` default (`DefDMCount`) and range.
pub const DMAP_NUM_FRAMES_DEFAULT: i32 = 25;
pub const DMAP_NUM_FRAMES_RANGE: (i32, i32) = (5, 25);
/// `/camera/dmap_hot_factor` and `/camera/dmap_cold_factor` default
/// (`DefDMSigmaX`, Refine_DefMap.cpp:51): the aggressiveness the refine
/// dialog starts from. The builder's own default is 100.
pub const DMAP_FACTOR_DEFAULT: i32 = 75;
/// `/camera/darks_note` is capped at 65 characters (`MaxNoteLength`).
pub const DARKS_NOTE_MAX_LEN: usize = 65;
/// `/camera/AutoLoadDarks` default (gear_dialog.cpp:983): load the dark
/// library when the camera connects, unless a defect map was just loaded.
/// `LoadDarkHandler` writes it, true or false, on every use.
pub const AUTO_LOAD_DARKS_DEFAULT: bool = true;
/// `/camera/AutoLoadDefectMap` default (gear_dialog.cpp:974): load the
/// defect map when the camera connects. `LoadDefectMapHandler` writes it.
pub const AUTO_LOAD_DEFECT_MAP_DEFAULT: bool = true;
/// `/camera/dmap_show_details` default (Refine_DefMap.cpp:280): the refine
/// dialog's "Show details" box, saved when the dialog closes.
pub const DMAP_SHOW_DETAILS_DEFAULT: bool = true;

/// `MyFrame::ExposureDurationLabel` (myframe.cpp:779): `%g s` from 10 s up,
/// else `%.2f s` under 100 ms and `%.1f s` otherwise. The dark-library
/// exposure range is stored in the profile as these labels.
pub fn exposure_duration_label(ms: i32) -> String {
    if ms >= 10000 {
        return format!("{} s", fmt_g(ms as f64 / 1000.));
    }
    let digits = if ms < 100 { 2 } else { 1 };
    format!("{} s", fmt_f(ms as f64 / 1000., digits))
}

/// `/camera/darks_min_exptime` default (`MinExposureDefault`): 0.1 s when
/// the primary mount is an AO, else 1.0 s.
pub fn darks_min_exptime_default(step_guider: bool) -> String {
    exposure_duration_label(if step_guider { 100 } else { 1000 })
}

/// `/camera/darks_max_exptime` default (`MaxExposureDefault`): 2.5 s with an
/// AO, else 6.0 s.
pub fn darks_max_exptime_default(step_guider: bool) -> String {
    exposure_duration_label(if step_guider { 2500 } else { 6000 })
}

/// The exposures a dark library is built for (`DarksDialog::OnStart`): the
/// exposure list sorted, from the entry labelled `min_label` to the one
/// labelled `max_label`, both included — the indices of the dialog's two
/// read-only combo boxes. Empty when the range is reversed. A label not in
/// the list leaves PHD2's combo with no selection and indexes the list at
/// −1; here it gives an empty list.
pub fn dark_lib_exposures(durations: &[i32], min_label: &str, max_label: &str) -> Vec<i32> {
    let mut d = durations.to_vec();
    d.sort();
    let find = |label: &str| d.iter().position(|&ms| exposure_duration_label(ms) == label);
    match (find(min_label), find(max_label)) {
        (Some(lo), Some(hi)) if lo <= hi => d[lo..=hi].to_vec(),
        _ => Vec::new(),
    }
}

/// `_N` after the base name for instance N > 1, as every PHD2 file name has it.
fn instance_suffix(instance: i32) -> String {
    if instance > 1 { format!("_{instance}") } else { String::new() }
}

/// `MyFrame::DarkLibFileName`: `PHD2_dark_lib[_N]_<profile>.fit`.
pub fn dark_lib_file_name(instance: i32, profile_id: i32) -> String {
    format!("PHD2_dark_lib{}_{profile_id}.fit", instance_suffix(instance))
}

/// `DefectMap::DefectMapFileName`: `PHD2_defect_map[_N]_<profile>.txt`.
pub fn defect_map_file_name(instance: i32, profile_id: i32) -> String {
    format!("PHD2_defect_map{}_{profile_id}.txt", instance_suffix(instance))
}

/// `DefectMapMasterPath`: the master dark the map is built from.
pub fn defect_map_master_file_name(instance: i32, profile_id: i32) -> String {
    format!("PHD2_defect_map_master{}_{profile_id}.fit", instance_suffix(instance))
}

/// `DefectMapFilterPath`: its median-filtered copy.
pub fn defect_map_filter_file_name(instance: i32, profile_id: i32) -> String {
    format!("PHD2_defect_map_master_filt{}_{profile_id}.fit", instance_suffix(instance))
}

// ---------------------------------------------------------------- dark library

/// The camera's dark state (`GuideCamera::Darks`, `CurrentDarkFrame`,
/// `CurrentDefectMap`).
#[derive(Clone, Debug, Default)]
pub struct DarkLibrary {
    /// Master darks keyed by their exposure in ms (`ImgExpDur`).
    pub darks: BTreeMap<i32, UsImage>,
    /// The key of the dark in use, set by [`DarkLibrary::select_dark`].
    current: Option<i32>,
    /// The bad-pixel map in use; while set it replaces the dark subtraction.
    pub defect_map: Option<DefectMap>,
}

impl DarkLibrary {
    /// `AddDark`: keyed by the dark's exposure, replacing any dark of the
    /// same exposure (the selection then follows the new one).
    pub fn add_dark(&mut self, dark: UsImage) {
        self.darks.insert(dark.img_exp_dur, dark);
    }

    /// `SelectDark` (camera.cpp:1411): the dark with the smallest exposure ≥
    /// `exposure_ms`, else the one with the longest; none when empty.
    pub fn select_dark(&mut self, exposure_ms: i32) {
        self.current = self.darks.range(exposure_ms..).next().or(self.darks.last_key_value()).map(|(&k, _)| k);
    }

    /// The dark in use (`CurrentDarkFrame`).
    pub fn current_dark(&self) -> Option<&UsImage> {
        self.darks.get(&self.current?)
    }

    /// `ClearDarks`: every dark dropped, none selected.
    pub fn clear_darks(&mut self) {
        self.darks.clear();
        self.current = None;
    }

    /// `GetDarkLibraryProperties`: (count, min, max) of the exposures in ms.
    /// Quirk kept: the minimum starts at 9999.0, so a library whose darks are
    /// all 10 s or longer reports 9999 (the dialog then says 9.999 s); an
    /// empty one reports (0, 9999, −9999).
    pub fn properties(&self) -> (usize, f64, f64) {
        let (mut min, mut max) = (9999.0, -9999.0);
        for &k in self.darks.keys() {
            min = if (k as f64) < min { k as f64 } else { min };
            max = if (k as f64) > max { k as f64 } else { max };
        }
        (self.darks.len(), min, max)
    }

    /// `SubtractDark` (camera.cpp:1482): repair the defect map's pixels when
    /// a map is loaded, else subtract the selected dark, else nothing. `Err`
    /// is `subtract`'s refusal (size mismatch), which PHD2 ignores.
    pub fn subtract_dark(&self, img: &mut UsImage) -> Result<(), &'static str> {
        if let Some(map) = &self.defect_map {
            map.remove_defects(img);
        } else if let Some(dark) = self.current_dark() {
            subtract(img, dark)?;
        }
        Ok(())
    }

    /// `save_multi_darks` (`MyFrame::SaveDarkLibrary`): the library file, the
    /// darks in ascending exposure, `note` as each HDU's `USERNOTE`.
    pub fn to_fits(&self, note: &str) -> Vec<u8> {
        fits::write_multi(&self.darks.values().collect::<Vec<_>>(), note)
    }

    /// `MyFrame::LoadDarkLibrary` once the file is read: every frame of the
    /// file is added on top of the darks already held (`load_multi_darks`
    /// does not clear them), a frame without `EXPOSURE` taking
    /// `requested_exp_ms` (`RequestedExposureDuration`); then the dark for
    /// `exposure_ms` (`m_exposureDuration`) is selected. `Err` is the read's
    /// alert ("Darks not loaded"): the frames before it stay added, as in
    /// PHD2, and the selection is left as it was — following a replaced dark
    /// to the frame that replaced it, as with [`DarkLibrary::add_dark`].
    pub fn load_fits(
        &mut self,
        bytes: &[u8],
        name: &str,
        requested_exp_ms: i32,
        exposure_ms: i32,
    ) -> Result<(), String> {
        let file = fits::read_multi(bytes, name, requested_exp_ms);
        for (img, _) in file.frames {
            self.add_dark(img);
        }
        if let Some(e) = file.error {
            return Err(e);
        }
        self.select_dark(exposure_ms);
        Ok(())
    }
}

/// `DarksDialog::CreateMasterDarkFrame` (darks_dialog.cpp:498): `count` raw
/// dark frames (`CAPTURE_DARK`: no subtraction, no recon) summed pixel by
/// pixel in `u32`, each sum then divided by `count`. The library builds one
/// per exposure of [`dark_lib_exposures`]; the defect map one of
/// `/camera/dmap_exptime` × 1000 ms over `/camera/dmap_num_frames` frames.
#[derive(Clone, Debug)]
pub struct MasterDarkBuilder {
    exp_ms: i32,
    count: i32,
    taken: i32,
    sum: Vec<u32>,
    last: Option<UsImage>,
}

impl MasterDarkBuilder {
    pub fn new(exp_ms: i32, count: i32) -> Self {
        Self { exp_ms, count, taken: 0, sum: Vec::new(), last: None }
    }

    /// Frames taken so far. PHD2's "Taking dark frame j/N" status, shown
    /// before each capture, has `j = taken() + 1` (darks_dialog.cpp:513).
    pub fn taken(&self) -> i32 {
        self.taken
    }

    /// True until `count` frames have been added.
    pub fn needs_more(&self) -> bool {
        self.taken < self.count
    }

    /// Add one captured dark. PHD2 computes its stats (for the debug log)
    /// before summing it. A frame of another size is refused: PHD2 would run
    /// off its sum buffer.
    pub fn add(&mut self, mut frame: UsImage) -> Result<(), &'static str> {
        frame.calc_stats();
        if self.sum.is_empty() {
            self.sum = vec![0; frame.data.len()];
        } else if frame.data.len() != self.sum.len() {
            return Err("dark frame size changed");
        }
        for (s, &v) in self.sum.iter_mut().zip(&frame.data) {
            *s += v as u32;
        }
        self.last = Some(frame);
        self.taken += 1;
        Ok(())
    }

    /// The master dark, once all frames are in: the last frame with each
    /// pixel replaced by the average, `img_exp_dur = exp_ms` and
    /// `img_stack_cnt = count`. Quirk kept: its min / max / median are the
    /// last frame's, not recomputed (PHD2 reloads the saved library, which
    /// computes them, before using it).
    pub fn finish(self) -> Option<UsImage> {
        if self.count <= 0 || self.taken != self.count {
            return None;
        }
        let mut img = self.last?;
        img.img_exp_dur = self.exp_ms;
        img.img_stack_cnt = self.count;
        for (p, &s) in img.data.iter_mut().zip(&self.sum) {
            *p = (s / self.count as u32) as u16;
        }
        Some(img)
    }
}

// ---------------------------------------------------------------- build sequence

/// `DarksDialog::OnStart`'s message box before the first frame when the
/// camera has no shutter (`HasShutter`).
pub const COVER_GUIDE_SCOPE: &str = "Cover guide scope";

/// Where a [`DarksBuild`] stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DarksBuildState {
    /// Frames still to take.
    Running,
    /// Every master dark built.
    Done,
    Cancelled,
    /// A capture failed.
    Failed,
}

/// `DarksDialog::OnStart` (darks_dialog.cpp:259) from the Start button on:
/// the master darks of a dark library, one per exposure, or the defect
/// map's single master, a frame at a time, with the dialog's status line
/// and progress. The caller captures the frames [`DarksBuild::next_capture`]
/// asks for (`CAPTURE_DARK`, with [`super::camera::CameraSettings::shutter_closed`]
/// set from the start and cleared when the build ends `Done` or `Failed`; a
/// `Cancelled` build leaves it set, as PHD2 does) and does the file work:
///
/// - first, the [`COVER_GUIDE_SCOPE`] box when the camera has no shutter;
/// - a dark library `Done`: save it ([`DarkLibrary::to_fits`] with the
///   note), then `LoadDarkHandler(true)`, which also unloads the defect map;
/// - a dark library `Cancelled` or `Failed`: the masters built so far stay
///   in the library; when the file is usable (`DarkLibExists`, no alert),
///   `LoadDarkHandler(true)` loads it on top of them;
/// - a defect map `Done`: [`build_filtered_dark`] of
///   [`DarksBuild::take_master`] and save both (`DefectMapDarks::SaveDarks`);
/// - at any end, `SetDarkMenuState` ([`DarkGeometry::refresh`]); once the
///   dark-library dialog closes, built or not,
///   `SelectDark(RequestedExposureDuration())` (myframe_events.cpp:718).
#[derive(Clone, Debug)]
pub struct DarksBuild {
    /// One master per exposure (ms); a single one for the defect map.
    exposures: Vec<i32>,
    count: i32,
    defect_map: bool,
    /// The master being built is `exposures[idx]`'s.
    idx: usize,
    master: MasterDarkBuilder,
    built: Option<UsImage>,
    state: DarksBuildState,
    /// `ShowStatus`'s preamble and the line it shows.
    preamble: String,
    status: String,
    progress_ms: i32,
    total_ms: i32,
}

impl DarksBuild {
    /// A dark library over `exposures` ([`dark_lib_exposures`]), `count`
    /// frames each (`/camera/darks_num_frames`, held to its spin range).
    /// `new_library` is "Create entirely new dark library": the darks held
    /// are cleared first (`ClearDarks`).
    pub fn dark_library(lib: &mut DarkLibrary, exposures: Vec<i32>, count: i32, new_library: bool) -> Self {
        if new_library {
            lib.clear_darks();
        }
        Self::new(exposures, count.clamp(DARKS_NUM_FRAMES_RANGE.0, DARKS_NUM_FRAMES_RANGE.1), false)
    }

    /// The defect map's master dark: `count` frames (`/camera/dmap_num_frames`)
    /// of `exp_s` seconds (`/camera/dmap_exptime`), each held to its spin
    /// range.
    pub fn defect_map(exp_s: i32, count: i32) -> Self {
        let exp_ms = exp_s.clamp(DMAP_EXPTIME_RANGE.0, DMAP_EXPTIME_RANGE.1) * 1000;
        Self::new(vec![exp_ms], count.clamp(DMAP_NUM_FRAMES_RANGE.0, DMAP_NUM_FRAMES_RANGE.1), true)
    }

    fn new(exposures: Vec<i32>, count: i32, defect_map: bool) -> Self {
        let total_ms = exposures.iter().map(|&e| e * count).sum();
        let mut b = Self {
            exposures,
            count,
            defect_map,
            idx: 0,
            master: MasterDarkBuilder::new(0, count),
            built: None,
            state: DarksBuildState::Running,
            preamble: String::new(),
            status: String::new(),
            progress_ms: 0,
            total_ms,
        };
        b.start_master();
        b
    }

    /// `ShowStatus(msg, false)`: a new line, which later ones append to.
    fn show(&mut self, msg: &str) {
        self.preamble = msg.to_string();
        self.status = msg.to_string();
    }

    /// `ShowStatus(msg, true)`: the preamble, a space, `msg`.
    fn append(&mut self, msg: &str) {
        self.status = format!("{} {msg}", self.preamble);
    }

    /// The next master's status line, or the end of the build after the
    /// last one. An empty library range ends at once and saves the library
    /// as it is, as PHD2 does.
    fn start_master(&mut self) {
        let Some(&exp) = self.exposures.get(self.idx) else {
            self.state = DarksBuildState::Done;
            // PHD2 shows "Analyzing master dark..." while the caller
            // filters and saves the defect map's darks.
            self.show(if self.defect_map { "Master dark data files built" } else { "dark library built" });
            return;
        };
        self.master = MasterDarkBuilder::new(exp, self.count);
        if self.defect_map {
            // The trailing space gives two before each appended line.
            self.show("Taking darks to compute defect map: ");
        } else if exp >= 1000 {
            self.show(&format!("Building master dark at {} sec:", fmt_f(exp as f64 / 1000.0, 1)));
        } else {
            self.show(&format!("Building master dark at {exp} mSec:"));
        }
        self.append(&format!("Taking dark frame 1/{}", self.count));
    }

    /// The exposure in ms of the next frame to capture; `None` once the
    /// build has ended.
    pub fn next_capture(&self) -> Option<i32> {
        (self.state == DarksBuildState::Running).then(|| self.exposures[self.idx])
    }

    /// One captured dark frame. Its exposure's last frame completes the
    /// master: a dark library's goes into `lib` at once (`AddDark`, so it
    /// stays there after a later cancel), the defect map's waits for
    /// [`DarksBuild::take_master`]. A frame of another size fails the build.
    pub fn add_frame(&mut self, lib: &mut DarkLibrary, frame: UsImage) {
        if self.state != DarksBuildState::Running {
            return;
        }
        if self.master.add(frame).is_err() {
            self.capture_failed();
            return;
        }
        self.progress_ms += self.exposures[self.idx];
        if self.master.needs_more() {
            self.append(&format!("Taking dark frame {}/{}", self.master.taken() + 1, self.count));
            return;
        }
        self.append("Dark frames complete");
        let master = std::mem::replace(&mut self.master, MasterDarkBuilder::new(0, 0)).finish();
        if self.defect_map {
            self.built = master;
        } else if let Some(m) = master {
            lib.add_dark(m);
        }
        self.idx += 1;
        self.start_master();
    }

    /// The capture failed: "%.1f s dark FAILED" after the preamble, then
    /// for a dark library "Operation failed - no changes have been made".
    pub fn capture_failed(&mut self) {
        if self.state != DarksBuildState::Running {
            return;
        }
        let exp = self.exposures[self.idx];
        self.append(&format!("{} s dark FAILED", fmt_f(exp as f64 / 1000.0, 1)));
        if !self.defect_map {
            self.show("Operation failed - no changes have been made");
        }
        self.end(DarksBuildState::Failed);
    }

    /// Stop pressed (`OnStop`). PHD2 lets the capture in flight finish and
    /// then drops the master being built; here the build ends at once and
    /// that frame is not wanted.
    pub fn cancel(&mut self) {
        if self.state != DarksBuildState::Running {
            return;
        }
        self.show(if self.defect_map {
            "Operation cancelled"
        } else {
            "Operation cancelled - no changes have been made"
        });
        self.end(DarksBuildState::Cancelled);
    }

    /// A cancel or a failure also resets the dialog's gauge.
    fn end(&mut self, state: DarksBuildState) {
        self.state = state;
        self.progress_ms = 0;
    }

    pub fn state(&self) -> DarksBuildState {
        self.state
    }

    /// The dialog's status line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The gauge: the exposure time of the frames taken, out of the sum of
    /// every exposure × `count`. PHD2 also adds one exposure after each
    /// master, which runs its gauge past the range; left out.
    pub fn progress(&self) -> (i32, i32) {
        (self.progress_ms, self.total_ms)
    }

    /// The defect map's master dark once `Done` (`DefectMapDarks::masterDark`).
    pub fn take_master(&mut self) -> Option<UsImage> {
        self.built.take()
    }

    /// The message box that ends a `Done` build: "Operation complete: %s",
    /// "Uncover guide scope" first when the camera has no shutter.
    pub fn wrapup(&self, has_shutter: bool) -> Option<String> {
        if self.state != DarksBuildState::Done {
            return None;
        }
        let msg = if self.defect_map { "Master dark data files built" } else { "dark library built" };
        let msg = if has_shutter { msg.to_string() } else { format!("Uncover guide scope\n\n{msg}") };
        Some(format!("Operation complete: {msg}"))
    }
}

// ---------------------------------------------------------------- geometry check

/// The dark library file or the defect map's master dark, as `DarkLibExists`
/// / `DefectMapExists` see it (myframe.cpp:2532, image_math.cpp:1198).
/// The dark library: `Missing` without the file, else
/// [`FileGeometry::from_fits`] of its bytes. The defect map: its `.txt`
/// decides presence, so `Missing` without it; with it, `from_fits` of the
/// master dark's bytes, or `Unreadable` when the master is missing or does
/// not read — cfitsio's open fails, no alert, and the map still passes
/// while the frame size is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGeometry {
    Missing,
    /// The FITS file does not open (no alert).
    Unreadable,
    /// `NAXIS1` × `NAXIS2` of its primary HDU.
    Size(i64, i64),
}

impl FileGeometry {
    /// A present file's bytes as PHD2 checks them: only the primary header
    /// ([`fits::primary_size`]), so a 3-axis file passes when its first two
    /// axes match. PHD2 alerts when the size read fails after the open, but
    /// the primary HDU is always an image, so it cannot.
    pub fn from_fits(bytes: &[u8]) -> Self {
        fits::primary_size(bytes).map_or(FileGeometry::Unreadable, |(w, h)| FileGeometry::Size(w, h))
    }

    /// `DarkLibExists` / `DefectMapExists`: the file is there and its first
    /// image has the camera's frame size — or that size is still unknown,
    /// `(0, 0)` (`UNDEFINED_FRAME_SIZE`), and then any present file passes.
    pub fn usable(self, frame_size: (i32, i32)) -> bool {
        match self {
            FileGeometry::Missing => false,
            _ if frame_size == (0, 0) => true,
            FileGeometry::Unreadable => false,
            FileGeometry::Size(w, h) => (w, h) == (frame_size.0 as i64, frame_size.1 as i64),
        }
    }

    /// The case PHD2 alerts on when asked to: the file opened and its size
    /// is not the camera's.
    fn mismatch(self, frame_size: (i32, i32)) -> bool {
        matches!(self, FileGeometry::Size(..)) && !self.usable(frame_size)
    }
}

/// `DarkLibExists`'s alert.
pub const DARK_LIB_MISMATCH: &str = "Dark library does not match the camera in this profile. \
    Check that you are connected to the camera you want to use for guiding.";
/// `DefectMapExists`'s alert.
pub const DEFECT_MAP_MISMATCH: &str = "Bad-pixel map does not match the camera in this profile - it needs to be replaced.";
/// `CheckDarkFrameGeometry`'s alert when both are dropped.
pub const DARKS_AND_DEFECT_MAP_MISMATCH: &str = "Dark library and bad-pixel maps don't match the current camera. \
    Check that you are connected to the camera you want to use for guiding.";

/// The state `MyFrame` keeps about darks between frames: whether the "Use
/// Dark Library" and "Use Bad-pixel Map" menu items are enabled (a matching
/// file exists) and checked, and the camera frame size (`DarkFrameSize`)
/// the last check ran against — PHD2 checks again whenever a frame comes in
/// with another size (myframe_events.cpp:569), and when capture starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DarkGeometry {
    pub darks_enabled: bool,
    pub defect_map_enabled: bool,
    /// The items' check marks (`IsChecked`). They are not "loaded":
    /// [`DarkGeometry::refresh`] clears the mark of an item it disables and
    /// unloads nothing, so darks loaded on connect (`AutoLoadDarks`, no size
    /// check) can stay in use unchecked. The load handlers keep them
    /// (myframe_events.cpp:722-800): `LoadDarkHandler(true)` checks the dark
    /// item and unchecks it again when the library fails to load;
    /// `LoadDefectMapHandler(true)` checks the map item and unchecks the dark
    /// item once the map loads (a failed map load changes neither); each
    /// handler's `false` form unchecks its item. Loading one unloads the
    /// other through that `false` form.
    pub darks_checked: bool,
    pub defect_map_checked: bool,
    pub prev_frame_size: (i32, i32),
}

/// What [`DarkGeometry::check`] asks of the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GeometryActions {
    /// `LoadDefectMapHandler(false)`: set `/camera/AutoLoadDefectMap` to
    /// false and drop the map in use, if any.
    pub unload_defect_map: bool,
    /// `LoadDarkHandler(false)`: set `/camera/AutoLoadDarks` to false, then
    /// clear the darks when one is selected (`CurrentDarkFrame`; otherwise
    /// they stay held).
    pub unload_darks: bool,
    /// The alerts, in PHD2's order.
    pub alerts: Vec<&'static str>,
}

impl DarkGeometry {
    /// `CheckDarkFrameGeometry` (myframe.cpp:2580): a library or map that no
    /// longer fits the camera is unloaded (when its item is checked) and
    /// disabled, one that fits again is enabled. A mismatch alerts only
    /// while its item was enabled; when both are dropped a third alert says
    /// so.
    pub fn check(
        &mut self,
        frame_size: (i32, i32),
        dark_lib: FileGeometry,
        defect_map: FileGeometry,
    ) -> GeometryActions {
        let mut out = GeometryActions::default();
        // The two existence tests run first, each alerting on its own.
        if self.defect_map_enabled && defect_map.mismatch(frame_size) {
            out.alerts.push(DEFECT_MAP_MISMATCH);
        }
        let have_defect_map = defect_map.usable(frame_size);
        if self.darks_enabled && dark_lib.mismatch(frame_size) {
            out.alerts.push(DARK_LIB_MISMATCH);
        }
        let have_dark_lib = dark_lib.usable(frame_size);

        let mut defect_map_ok = true;
        if self.defect_map_enabled {
            if !have_defect_map {
                // The handler's `false` form unchecks the item.
                out.unload_defect_map = std::mem::take(&mut self.defect_map_checked);
                self.defect_map_enabled = false;
                defect_map_ok = false;
            }
        } else if have_defect_map {
            self.defect_map_enabled = true;
        }

        if self.darks_enabled {
            if !have_dark_lib {
                out.unload_darks = std::mem::take(&mut self.darks_checked);
                self.darks_enabled = false;
                if !defect_map_ok {
                    out.alerts.push(DARKS_AND_DEFECT_MAP_MISMATCH);
                }
            }
        } else if have_dark_lib {
            self.darks_enabled = true;
        }

        self.prev_frame_size = frame_size;
        out
    }

    /// `SetDarkMenuState` (myframe.cpp:2624), run after the files change: both
    /// items enabled from the files, each mismatch alerting (dark library
    /// first). An item it disables is unchecked, but nothing is unloaded.
    pub fn refresh(
        &mut self,
        frame_size: (i32, i32),
        dark_lib: FileGeometry,
        defect_map: FileGeometry,
    ) -> Vec<&'static str> {
        let mut alerts = Vec::new();
        if dark_lib.mismatch(frame_size) {
            alerts.push(DARK_LIB_MISMATCH);
        }
        if defect_map.mismatch(frame_size) {
            alerts.push(DEFECT_MAP_MISMATCH);
        }
        self.darks_enabled = dark_lib.usable(frame_size);
        self.darks_checked &= self.darks_enabled;
        self.defect_map_enabled = defect_map.usable(frame_size);
        self.defect_map_checked &= self.defect_map_enabled;
        alerts
    }
}

// ---------------------------------------------------------------- defect map

/// First line of the defect map file.
pub const DEFECT_MAP_HEADER: &str = "# PHD2 Defect Map v1";

/// A bad-pixel map (`DefectMap`, a `std::vector<wxPoint>`): the pixels to
/// repair, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefectMap {
    pub pixels: Vec<(i32, i32)>,
}

impl DefectMap {
    /// `DefectMap::LoadDefectMap` on the file's text: one `x y` per line;
    /// blank lines and lines starting with `#` (after leading blanks) are
    /// skipped, a line whose first two tokens are not both integers is junk
    /// and ignored, tokens after the second are ignored.
    pub fn parse(text: &str) -> DefectMap {
        let mut pixels = Vec::new();
        // wxTextInputStream::ReadLine ends a line at \n, \r\n or a lone \r;
        // the empty pieces this split adds are skipped like blank lines.
        for line in text.split(['\n', '\r']) {
            // wxString::Trim(false): ASCII white space.
            let line = line.trim_start_matches([' ', '\t', '\x0b', '\x0c']);
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // wxStringTokenizer's default delimiters, empty tokens dropped.
            let mut tok = line.split([' ', '\t']).filter(|t| !t.is_empty());
            let (s1, s2) = (tok.next().unwrap_or(""), tok.next().unwrap_or(""));
            // wxString::ToLong: the whole token, base 10, optional sign;
            // wxPoint then keeps the low 32 bits.
            if let (Ok(x), Ok(y)) = (s1.parse::<i64>(), s2.parse::<i64>()) {
                pixels.push((x as i32, y as i32));
            }
        }
        DefectMap { pixels }
    }

    /// `DefectMap::Save`: the header, each `info` line as a comment (from
    /// [`DefectMapBuilder::map_info`]), the count, then `x y` per pixel.
    pub fn to_text(&self, info: &[String]) -> String {
        let mut s = format!("{DEFECT_MAP_HEADER}\n");
        for l in info {
            s += &format!("# {l}\n");
        }
        s += &format!("# Defect count: {}\n", self.pixels.len() as u32);
        for (x, y) in &self.pixels {
            s += &format!("{x} {y}\n");
        }
        s
    }

    /// `FindDefect`.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.pixels.contains(&(x, y))
    }

    /// `RefineDefMap::OnAddDefect` + `DefectMap::AddDefect`: the locked star
    /// position rounded (`(int)(x + 0.5)`) is appended unless already listed.
    /// Returns the line PHD2 appends to the file (whose `Defect count`
    /// comment is left as it was), or `None` when nothing was added.
    pub fn add_defect(&mut self, pos: Point) -> Option<String> {
        let p = ((pos.x + 0.5) as i32, (pos.y + 0.5) as i32);
        if self.contains(p.0, p.1) {
            return None;
        }
        self.pixels.push(p);
        Some(format!("{} {}\n", p.0, p.1))
    }

    /// `RemoveDefects` (image_math.cpp:1116): each listed pixel replaced by
    /// the median of its neighbours, in list order and in place (a repaired
    /// pixel feeds the next repairs). With a subframe only the pixels inside
    /// it are repaired, otherwise those inside the frame.
    pub fn remove_defects(&self, light: &mut UsImage) {
        if light.data.is_empty() {
            return;
        }
        let roi = light.roi();
        for &(x, y) in &self.pixels {
            let inside = match roi {
                Some(sf) => sf.contains(x, y),
                None => x >= 0 && x < light.w && y >= 0 && y < light.h,
            };
            if inside {
                let v = median_bordering_pixels(light, x, y);
                *light.pixel_mut(x, y) = v;
            }
        }
    }
}

/// `MedianBorderingPixels` (image_math.cpp:507): the median of the 8
/// neighbours, of the 5 on an edge, of the 3 in a corner. The even count
/// takes the truncated mean of the two middle values (`median8`). Reads use
/// PHD2's flat index; one past the buffer (a frame 1 px wide or high, where
/// PHD2 reads out of bounds) reads 0.
fn median_bordering_pixels(img: &UsImage, x: i32, y: i32) -> u16 {
    let (xs, ys) = (img.w, img.h);
    // The neighbours' offsets, in PHD2's order.
    let offsets: &[(i32, i32)] = if x > 0 && y > 0 && x < xs - 1 && y < ys - 1 {
        &[(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)]
    } else if x == 0 && y > 0 && y < ys - 1 {
        &[(0, -1), (0, 1), (1, -1), (1, 0), (1, 1)]
    } else if x == xs - 1 && y > 0 && y < ys - 1 {
        &[(0, -1), (0, 1), (-1, -1), (-1, 0), (-1, 1)]
    } else if y == 0 && x > 0 && x < xs - 1 {
        &[(-1, 0), (-1, 1), (0, 1), (1, 0), (1, 1)]
    } else if y == ys - 1 && x > 0 && x < xs - 1 {
        &[(-1, 0), (-1, -1), (0, -1), (1, 0), (1, -1)]
    } else if x == 0 && y == 0 {
        &[(1, 0), (0, 1), (1, 1)]
    } else if x == 0 && y == ys - 1 {
        &[(1, 0), (0, -1), (1, -1)]
    } else if x == xs - 1 && y == ys - 1 {
        &[(-1, 0), (0, -1), (-1, -1)]
    } else if x == xs - 1 && y == 0 {
        &[(-1, 0), (0, 1), (-1, 1)]
    } else {
        // PHD2's "unreachable".
        return 0;
    };
    let mut a = [0u16; 8];
    for (v, &(dx, dy)) in a.iter_mut().zip(offsets) {
        *v = img.data.get((x + dx + (y + dy) * xs) as usize).copied().unwrap_or(0);
    }
    window_median(&mut a[..offsets.len()])
}

// ---------------------------------------------------------------- defect map builder

/// `DefectMapDarks::BuildFilteredDark`: the master dark through a median
/// filter of half-width 15 px (a 31 × 31 window clipped at the borders),
/// into a fresh frame of the same size.
pub fn build_filtered_dark(master: &UsImage) -> UsImage {
    let mut dst = UsImage::new(master.w, master.h);
    median_filter(&mut dst.data, master, 15);
    dst
}

/// `histo_median` (image_math.cpp:720): the element at index `n / 2` of the
/// window's sorted values, found coarse (high byte) then fine. The counts
/// are `unsigned short` in PHD2; the window never reaches 65536 pixels.
fn histo_median(h1: &[u16; 256], h2: &[u16], n: i32) -> u16 {
    let mut n = n / 2;
    let mut i = 0usize;
    while i < 256 {
        if h1[i] as i32 > n {
            break;
        }
        n -= h1[i] as i32;
        i += 1;
    }
    i <<= 8;
    while i < 65536 {
        if h2[i] as i32 > n {
            break;
        }
        n -= h2[i] as i32;
        i += 1;
    }
    // 65536 (never reached on real data) wraps to 0 as in the C cast.
    i as u16
}

/// `MedianFilter` (image_math.cpp:739): a sliding two-level histogram along
/// each row. Quirk kept: a row's first window spans columns 0..=half_width
/// whatever the frame's width, read through the flat index, so a frame
/// narrower than 16 px takes pixels from the next row (and 0 past the end,
/// where PHD2 reads out of bounds).
fn median_filter(dst: &mut [u16], src: &UsImage, half_width: i32) {
    let (width, height) = (src.w, src.h);
    if src.data.is_empty() {
        return;
    }
    let px = |x: i32, y: i32| src.data.get((y * width + x) as usize).copied().unwrap_or(0) as usize;
    let mut h1 = [0u16; 256];
    let mut h2 = vec![0u16; 65536];
    let mut d = 0;
    for y in 0..height {
        let top = (y - half_width).max(0);
        let bot = (y + half_width).min(height - 1);
        let (left, right) = (0, half_width);
        h1.fill(0);
        h2.fill(0);
        for j in top..=bot {
            for i in left..=right {
                let v = px(i, j);
                h1[v >> 8] = h1[v >> 8].wrapping_add(1);
                h2[v] = h2[v].wrapping_add(1);
            }
        }
        let mut n = (right - left + 1) * (bot - top + 1);
        dst[d] = histo_median(&h1, &h2, n);
        d += 1;
        for i in 1..width {
            let left = (i - half_width).max(0);
            let right = (i + half_width).min(width - 1);
            if left > 0 {
                // Drop the column that left the window.
                for j in top..=bot {
                    let v = px(left - 1, j);
                    h1[v >> 8] = h1[v >> 8].wrapping_sub(1);
                    h2[v] = h2[v].wrapping_sub(1);
                }
                n -= bot - top + 1;
            }
            if i + half_width <= width - 1 {
                // Add the column that entered it.
                for j in top..=bot {
                    let v = px(right, j);
                    h1[v >> 8] = h1[v >> 8].wrapping_add(1);
                    h2[v] = h2[v].wrapping_add(1);
                }
                n += bot - top + 1;
            }
            dst[d] = histo_median(&h1, &h2, n);
            d += 1;
        }
    }
}

/// `ImageStats` of the master dark.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ImageStats {
    pub mean: f64,
    /// Population standard deviation (n in the denominator).
    pub stdev: f64,
    /// The element at index n / 2 of the sorted pixels.
    pub median: u16,
    /// Median absolute deviation from `median`, same rule.
    pub mad: u16,
}

/// `GetImageStats` (image_math.cpp:819) over the whole frame: Welford's
/// running mean and sum of squares, then `nth_element` medians.
fn image_stats(img: &UsImage) -> ImageStats {
    let (mut a, mut q, mut k, mut km1) = (0.0f64, 0.0f64, 1.0f64, 0.0f64);
    for &p in &img.data {
        let x = p as f64;
        let a0 = a;
        a += (x - a) / k;
        q += (x - a0) * (x - a);
        km1 = k;
        k += 1.0;
    }
    let mut s = ImageStats { mean: a, stdev: (q / km1).sqrt(), median: 0, mad: 0 };
    if img.data.is_empty() {
        return s;
    }
    let mut tmp = img.data.clone();
    let mid = tmp.len() / 2;
    s.median = *tmp.select_nth_unstable(mid).1;
    for p in tmp.iter_mut() {
        *p = (*p as i32 - s.median as i32).unsigned_abs() as u16;
    }
    s.mad = *tmp.select_nth_unstable(mid).1;
    s
}

/// `AggrToSigma`: aggressiveness 0..100 to a threshold of 8σ down to 0.125σ.
pub fn aggr_to_sigma(val: i32) -> f64 {
    (3.0 - (6.0 / 100.0) * val as f64).exp2()
}

/// A built defect map with its cold and hot pixel counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefectMapBuild {
    pub map: DefectMap,
    pub cold: u32,
    pub hot: u32,
}

/// `DefectMapBuilder`: the master dark and its filtered copy, the dark's
/// statistics, and every pixel deviating from the filtered dark by more than
/// the 100-aggressiveness threshold (0.125σ), from which each build picks
/// those past its own thresholds.
#[derive(Clone, Debug)]
pub struct DefectMapBuilder {
    pub master: UsImage,
    pub filtered: UsImage,
    stats: ImageStats,
    /// `BadPxSet`s: a `std::set` ordered by deviation only, so one pixel per
    /// deviation value — the first in row-major order — is kept. Quirk kept:
    /// two hot pixels with the same deviation give one defect.
    cold: BTreeMap<i32, (u16, u16)>,
    hot: BTreeMap<i32, (u16, u16)>,
}

impl DefectMapBuilder {
    /// `DefectMapBuilder::Init` (image_math.cpp:956): `v = dark − filtered`
    /// per pixel; hot when `v > t`, cold when `−v > t`, with
    /// `t = (int)(0.125 · σ)` and σ the dark's standard deviation. `Err`
    /// when the two frames differ in size (PHD2 reads past the filtered one).
    pub fn new(master: UsImage, filtered: UsImage) -> Result<Self, &'static str> {
        if (master.w, master.h) != (filtered.w, filtered.h) {
            return Err("master and filtered darks differ in size");
        }
        let stats = image_stats(&master);
        let thresh = (aggr_to_sigma(100) * stats.stdev) as i32;
        let (mut cold, mut hot) = (BTreeMap::new(), BTreeMap::new());
        for y in 0..master.h {
            for x in 0..master.w {
                let v = master.pixel(x, y) as i32 - filtered.pixel(x, y) as i32;
                let at = (x as u16, y as u16);
                if v > thresh {
                    hot.entry(v).or_insert(at);
                } else if -v > thresh {
                    cold.entry(-v).or_insert(at);
                }
            }
        }
        Ok(Self { master, filtered, stats, cold, hot })
    }

    /// `GetImageStats`.
    pub fn stats(&self) -> &ImageStats {
        &self.stats
    }

    /// `FindThresh`: the cold and hot pixel thresholds in ADU,
    /// `(int)(AggrToSigma(aggr) · σ)`, aggressiveness clamped to 0..=100.
    fn thresholds(&self, cold_aggr: i32, hot_aggr: i32) -> (i32, i32) {
        let t = |a: i32| (aggr_to_sigma(a.clamp(0, 100)) * self.stats.stdev) as i32;
        (t(cold_aggr), t(hot_aggr))
    }

    /// `SetAggressiveness` then `GetColdPixelCnt` / `GetHotPixelCnt`:
    /// `(cold, hot)`, the pixels deviating by at least each threshold.
    pub fn counts(&self, cold_aggr: i32, hot_aggr: i32) -> (u32, u32) {
        let (tc, th) = self.thresholds(cold_aggr, hot_aggr);
        (self.cold.range(tc..).count() as u32, self.hot.range(th..).count() as u32)
    }

    /// `SetAggressiveness` then `BuildDefectMap`: the cold pixels then the
    /// hot ones, each from the smallest deviation to the largest.
    pub fn build(&self, cold_aggr: i32, hot_aggr: i32) -> DefectMapBuild {
        let (tc, th) = self.thresholds(cold_aggr, hot_aggr);
        let at = |(_, &(x, y)): (&i32, &(u16, u16))| (x as i32, y as i32);
        let mut map = DefectMap::default();
        map.pixels.extend(self.cold.range(tc..).map(at));
        let cold = map.pixels.len() as u32;
        map.pixels.extend(self.hot.range(th..).map(at));
        let hot = map.pixels.len() as u32 - cold;
        DefectMapBuild { map, cold, hot }
    }

    /// `GetMapInfo` after a build: the comment lines of the defect map file.
    /// `generated` is the build time, `camera` the camera's name.
    pub fn map_info(&self, cold_aggr: i32, hot_aggr: i32, camera: &str, generated: &DateTime<Local>) -> Vec<String> {
        let (cold_aggr, hot_aggr) = (cold_aggr.clamp(0, 100), hot_aggr.clamp(0, 100));
        let (mult_cold, mult_hot) = (aggr_to_sigma(cold_aggr), aggr_to_sigma(hot_aggr));
        let (delta_cold, delta_hot) = self.thresholds(cold_aggr, hot_aggr);
        let s = &self.stats;
        vec![
            // wxDateTime::FormatISOCombined(' ') of the local time.
            format!("Generated: {}", generated.format("%Y-%m-%d %H:%M:%S")),
            format!("Camera: {camera}"),
            format!("Dark Exposure Time: {} ms", self.master.img_exp_dur),
            format!("Dark Frame Count: {}", self.master.img_stack_cnt),
            format!("Aggressiveness, cold: {cold_aggr}"),
            format!("Aggressiveness, hot: {hot_aggr}"),
            format!("Sigma Thresh, cold: {}", fmt_f(mult_cold, 2)),
            format!("Sigma Thresh, hot: {}", fmt_f(mult_hot, 2)),
            format!("Mean: {}", fmt_f(s.mean, 0)),
            format!("Stdev: {}", fmt_f(s.stdev, 0)),
            format!("Median: {}", s.median),
            format!("MAD: {}", s.mad),
            format!("DeltaCold: {:+}", -delta_cold),
            format!("DeltaHot: {delta_hot:+}"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::traits::Rect;
    use chrono::TimeZone;

    fn image(w: i32, h: i32, px: &[u16]) -> UsImage {
        let mut img = UsImage::new(w, h);
        img.data.copy_from_slice(px);
        img
    }

    fn flat(w: i32, h: i32, v: u16, exp_ms: i32) -> UsImage {
        let mut img = UsImage::new(w, h);
        img.data.fill(v);
        img.img_exp_dur = exp_ms;
        img
    }

    /// The harness's synthetic dark: a gradient, a little deterministic
    /// noise, five hot and three cold pixels (two of them in corners).
    fn synthetic_dark() -> UsImage {
        let (w, h) = (48, 40);
        let mut img = UsImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let noise = ((x * 7919 + y * 104729 + x * y * 31) % 17) - 8;
                *img.pixel_mut(x, y) = (1000 + 3 * x + 2 * y + noise) as u16;
            }
        }
        for (x, y, d) in [(10, 10, 2000), (25, 5, 500), (40, 30, 120), (0, 0, 900), (47, 20, 300)] {
            *img.pixel_mut(x, y) += d;
        }
        for (x, y, d) in [(15, 30, 300), (33, 12, 80), (47, 39, 200)] {
            *img.pixel_mut(x, y) -= d;
        }
        img.img_exp_dur = 15000;
        img.img_stack_cnt = 25;
        img
    }

    // From PHD2's C++ (image_math.cpp MedianFilter, GetImageStats,
    // DefectMapBuilder, RemoveDefects compiled in a throwaway harness on the
    // synthetic dark above).
    const COLD_100: [(i32, i32); 35] = [
        (24, 0), (18, 0), (39, 0), (25, 0), (12, 0), (14, 1), (13, 0), (14, 0), (15, 0), (21, 0), (27, 0), (6, 0),
        (22, 0), (7, 0), (17, 0), (5, 1), (8, 0), (9, 0), (10, 0), (2, 1), (1, 0), (10, 1), (3, 2), (2, 0), (7, 1),
        (3, 0), (4, 0), (5, 0), (4, 1), (1, 1), (2, 2), (0, 2), (33, 12), (47, 39), (15, 30),
    ];
    const HOT_100: [(i32, i32); 38] = [
        (45, 1), (47, 3), (44, 2), (47, 0), (46, 0), (45, 3), (47, 4), (45, 11), (47, 1), (46, 7), (41, 14), (46, 5),
        (47, 12), (47, 9), (45, 10), (46, 12), (44, 14), (47, 13), (45, 15), (46, 17), (47, 29), (47, 18), (47, 26),
        (46, 31), (46, 29), (47, 30), (45, 32), (47, 34), (46, 36), (45, 37), (46, 34), (47, 35), (46, 39), (40, 30),
        (47, 20), (25, 5), (0, 0), (10, 10),
    ];

    #[test]
    fn filtered_dark_matches_phd2() {
        let f = build_filtered_dark(&synthetic_dark());
        assert_eq!((f.w, f.h), (48, 40));
        let sum: u64 = f.data.iter().map(|&v| v as u64).sum();
        let wsum: u64 = f.data.iter().enumerate().map(|(i, &v)| v as u64 * (i as u64 + 1)).sum();
        assert_eq!((sum, wsum), (2130353, 2063294348));
        let at = [
            ((0, 0), 1037),
            ((5, 7), 1053),
            ((10, 10), 1062),
            ((20, 20), 1100),
            ((30, 10), 1115),
            ((47, 39), 1182),
            ((47, 0), 1133),
            ((0, 39), 1086),
            ((15, 30), 1098),
        ];
        for ((x, y), v) in at {
            assert_eq!(f.pixel(x, y), v, "({x},{y})");
        }
        // A fresh frame: none of the master's metadata.
        assert_eq!((f.img_exp_dur, f.img_stack_cnt), (0, 1));
    }

    #[test]
    fn narrow_frame_first_window_reads_the_next_rows() {
        // From PHD2's C++ MedianFilter on a 10 × 40 frame: each row's first
        // window spans columns 0..=15 through the flat index, and no column
        // ever enters or leaves it, so a whole row takes one value. Rows
        // from 24 on read past PHD2's buffer and are left out.
        let mut dark = UsImage::new(10, 40);
        for y in 0..40 {
            for x in 0..10 {
                *dark.pixel_mut(x, y) = (500 + (x * 13 + y * 29 + x * y * 7) % 97) as u16;
            }
        }
        let f = build_filtered_dark(&dark);
        let col0: Vec<u16> = (0..=23).map(|y| f.pixel(0, y)).collect();
        assert_eq!(
            col0,
            [
                551, 551, 551, 551, 551, 551, 551, 551, 550, 549, 549, 549, 549, 550, 550, 549, 550, 549, 549, 549, 548,
                548, 548, 548
            ]
        );
        assert!((0..=23).all(|y| (0..10).all(|x| f.pixel(x, y) == f.pixel(0, y))));
    }

    #[test]
    fn defect_lists_match_phd2() {
        let dark = synthetic_dark();
        let filt = build_filtered_dark(&dark);
        let b = DefectMapBuilder::new(dark, filt).unwrap();
        let s = *b.stats();
        assert_eq!(s.mean, 1111.1822916666663);
        assert_eq!(s.stdev, 69.356514796724639);
        assert_eq!((s.median, s.mad), (1110, 36));

        let m0 = b.build(0, 0);
        assert_eq!((m0.cold, m0.hot), (0, 2));
        assert_eq!(m0.map.pixels, [(0, 0), (10, 10)]);

        let m50 = b.build(50, 50);
        assert_eq!((m50.cold, m50.hot), (3, 5));
        assert_eq!(m50.map.pixels, [(33, 12), (47, 39), (15, 30), (40, 30), (47, 20), (25, 5), (0, 0), (10, 10)]);

        let m100 = b.build(100, 100);
        assert_eq!((m100.cold, m100.hot), (35, 38));
        assert_eq!(m100.map.pixels, [&COLD_100[..], &HOT_100[..]].concat());

        // Separate aggressiveness per side, and the 0..=100 clamp.
        assert_eq!(b.build(100, 0).map.pixels, [&COLD_100[..], &[(0, 0), (10, 10)]].concat());
        assert_eq!(b.build(0, 100).map.pixels, HOT_100);
        assert_eq!(b.build(-5, 140), b.build(0, 100));
        assert_eq!(b.counts(75, 75), (20, 23));
        assert_eq!(b.build(75, 75).map.pixels, [&COLD_100[15..], &HOT_100[15..]].concat());
    }

    #[test]
    fn map_info_matches_phd2() {
        let dark = synthetic_dark();
        let filt = build_filtered_dark(&dark);
        let b = DefectMapBuilder::new(dark, filt).unwrap();
        let t = Local.with_ymd_and_hms(2026, 11, 11, 10, 0, 0).unwrap();
        let info = b.map_info(75, 75, "Test Camera", &t);
        assert_eq!(
            info,
            [
                "Generated: 2026-11-11 10:00:00",
                "Camera: Test Camera",
                "Dark Exposure Time: 15000 ms",
                "Dark Frame Count: 25",
                "Aggressiveness, cold: 75",
                "Aggressiveness, hot: 75",
                "Sigma Thresh, cold: 0.35",
                "Sigma Thresh, hot: 0.35",
                "Mean: 1111",
                "Stdev: 69",
                "Median: 1110",
                "MAD: 36",
                "DeltaCold: -24",
                "DeltaHot: +24",
            ]
        );
        let info = b.map_info(-5, 140, "Test Camera", &t);
        assert_eq!(
            info[4..8],
            ["Aggressiveness, cold: 0", "Aggressiveness, hot: 100", "Sigma Thresh, cold: 8.00", "Sigma Thresh, hot: 0.12"]
        );
        assert_eq!(info[12..], ["DeltaCold: -554", "DeltaHot: +8"]);
    }

    #[test]
    fn equal_deviations_keep_one_pixel() {
        // Two hot pixels 50 above a flat 100: the std::set keyed by the
        // deviation keeps the first in row-major order only.
        let mut dark = flat(20, 20, 100, 1000);
        *dark.pixel_mut(12, 3) = 150;
        *dark.pixel_mut(4, 9) = 150;
        let filt = build_filtered_dark(&dark);
        assert!(filt.data.iter().all(|&v| v == 100));
        let b = DefectMapBuilder::new(dark, filt).unwrap();
        assert_eq!(b.build(0, 100).map.pixels, [(12, 3)]);
        assert!(DefectMapBuilder::new(flat(20, 20, 1, 0), flat(20, 19, 1, 0)).is_err());
    }

    #[test]
    fn aggressiveness_to_sigma() {
        assert_eq!(aggr_to_sigma(0), 8.0);
        assert_eq!(aggr_to_sigma(50), 1.0);
        assert_eq!(aggr_to_sigma(100), 0.125);
    }

    fn defect_test_frame() -> UsImage {
        let mut img = UsImage::new(7, 6);
        for y in 0..6 {
            for x in 0..7 {
                *img.pixel_mut(x, y) = ((x * 37 + y * 101) % 997 + 100) as u16;
            }
        }
        *img.pixel_mut(0, 0) = 60000;
        *img.pixel_mut(3, 3) = 9;
        img
    }

    const DEFECTS: [(i32, i32); 12] =
        [(0, 0), (6, 0), (0, 5), (6, 5), (3, 0), (0, 2), (6, 3), (2, 5), (3, 3), (-1, 2), (7, 1), (2, 6)];

    #[test]
    fn remove_defects_corners_edges_and_subframe() {
        // From PHD2's C++ RemoveDefects: four corners (3 neighbours), the four
        // edges (5), an interior pixel (8), three points off the frame.
        let map = DefectMap { pixels: DEFECTS.to_vec() };
        let mut light = defect_test_frame();
        map.remove_defects(&mut light);
        assert_eq!(
            light.data,
            [
                201, 137, 174, 275, 248, 285, 386, 201, 238, 275, 312, 349, 386, 423, 339, 339, 376, 413, 450, 487, 524,
                403, 440, 477, 514, 551, 588, 588, 504, 541, 578, 615, 652, 689, 726, 541, 642, 615, 716, 753, 790, 726,
            ]
        );

        // With a subframe only (3, 3) is inside.
        let mut light = defect_test_frame();
        light.subframe = Some(Rect::new(1, 1, 4, 3));
        map.remove_defects(&mut light);
        let mut expected = defect_test_frame().data;
        expected[3 * 7 + 3] = 514;
        assert_eq!(light.data, expected);
    }

    #[test]
    fn select_dark_smallest_at_least_else_largest() {
        let mut lib = DarkLibrary::default();
        lib.select_dark(1000);
        assert!(lib.current_dark().is_none());
        for (exp, v) in [(1000, 10), (2000, 20), (5000, 50)] {
            lib.add_dark(flat(2, 2, v, exp));
        }
        let pick = |lib: &mut DarkLibrary, exp| {
            lib.select_dark(exp);
            lib.current_dark().unwrap().img_exp_dur
        };
        assert_eq!(pick(&mut lib, 1500), 2000);
        assert_eq!(pick(&mut lib, 2000), 2000);
        assert_eq!(pick(&mut lib, 10), 1000);
        assert_eq!(pick(&mut lib, 6000), 5000);

        // Replacing the selected exposure keeps it selected, on the new dark.
        lib.add_dark(flat(2, 2, 99, 5000));
        assert_eq!(lib.current_dark().unwrap().data[0], 99);

        assert_eq!(lib.properties(), (3, 1000.0, 5000.0));
        lib.clear_darks();
        assert!(lib.current_dark().is_none());
        assert_eq!(lib.properties(), (0, 9999.0, -9999.0));
        // The 9999 start of the minimum survives long darks.
        lib.add_dark(flat(2, 2, 1, 10000));
        lib.add_dark(flat(2, 2, 1, 15000));
        assert_eq!(lib.properties(), (2, 9999.0, 15000.0));
    }

    #[test]
    fn subtract_dark_prefers_the_defect_map() {
        let mut lib = DarkLibrary::default();
        let mut light = image(3, 3, &[100, 100, 100, 100, 5000, 100, 100, 100, 100]);
        // Nothing loaded: untouched.
        lib.subtract_dark(&mut light).unwrap();
        assert_eq!(light.data[4], 5000);

        let mut dark = flat(3, 3, 40, 1000);
        dark.calc_stats();
        lib.add_dark(dark);
        lib.select_dark(1000);
        let mut l = light.clone();
        lib.subtract_dark(&mut l).unwrap();
        // A fresh light: the pedestal is the dark's median.
        assert_eq!(l.pedestal, 40);
        assert_eq!((l.data[0], l.data[4]), (100, 5000));

        lib.defect_map = Some(DefectMap { pixels: vec![(1, 1)] });
        let mut l = light.clone();
        lib.subtract_dark(&mut l).unwrap();
        assert_eq!(l.data, [100; 9]);

        lib.defect_map = None;
        let mut small = flat(2, 2, 1, 0);
        assert_eq!(lib.subtract_dark(&mut small), Err("size mismatch"));
    }

    #[test]
    fn defect_map_text_round_trip() {
        let map = DefectMap { pixels: vec![(3, 4), (0, 0), (639, 479)] };
        let info = vec!["Camera: Test Camera".to_string(), "DeltaHot: +8".to_string()];
        let text = map.to_text(&info);
        assert_eq!(
            text,
            "# PHD2 Defect Map v1\n# Camera: Test Camera\n# DeltaHot: +8\n# Defect count: 3\n3 4\n0 0\n639 479\n"
        );
        assert_eq!(DefectMap::parse(&text), map);

        // CRLF, a lone CR, blank and indented lines, comments, junk, a third
        // token, signs.
        let text = "# PHD2 Defect Map v1\r\n  # indented comment\r\n\r\n1 2\r3\t4 junk\n\t 5   6\nx 7\n8\n+9 -10\n0x1 2\n";
        assert_eq!(DefectMap::parse(text).pixels, [(1, 2), (3, 4), (5, 6), (9, -10)]);
    }

    #[test]
    fn add_defect_rounds_and_skips_duplicates() {
        let mut map = DefectMap { pixels: vec![(10, 20)] };
        assert_eq!(map.add_defect(Point::new(9.6, 19.5)), None);
        assert_eq!(map.add_defect(Point::new(30.49, 7.5)), Some("30 8\n".to_string()));
        assert_eq!(map.pixels, [(10, 20), (30, 8)]);
        assert!(map.contains(30, 8));
    }

    #[test]
    fn dark_library_fits_round_trip() {
        let mut lib = DarkLibrary::default();
        for (exp, v) in [(5000, 300), (1000, 100), (2500, 250)] {
            let mut d = flat(4, 3, v, exp);
            d.data[5] = v + 7;
            lib.add_dark(d);
        }
        let bytes = lib.to_fits("my note");

        // Loading adds on top of what is held and selects for the exposure.
        let mut back = DarkLibrary::default();
        back.add_dark(flat(4, 3, 1, 15000));
        back.load_fits(&bytes, "lib.fit", 2000, 2000).unwrap();
        assert_eq!(back.darks.keys().copied().collect::<Vec<_>>(), [1000, 2500, 5000, 15000]);
        for (exp, d) in &lib.darks {
            let b = &back.darks[exp];
            assert_eq!((b.w, b.h, &b.data), (4, 3, &d.data));
            assert_eq!(b.median_adu, d.data[0]);
        }
        assert_eq!(back.current_dark().unwrap().img_exp_dur, 2500);

        // A broken file: the error, the selection left as it was.
        let err = back.load_fits(b"nonsense", "lib.fit", 1000, 1000).unwrap_err();
        assert_eq!(err, "Error opening FITS file lib.fit");
        assert_eq!(back.current_dark().unwrap().img_exp_dur, 2500);
    }

    #[test]
    fn master_dark_is_the_floored_average() {
        let mut b = MasterDarkBuilder::new(2000, 3);
        for v in [[10u16, 65535], [11, 65535], [13, 65534]] {
            assert!(b.needs_more());
            let mut f = image(2, 1, &v);
            f.bits_per_pixel = 16;
            b.add(f).unwrap();
        }
        assert!(!b.needs_more());
        assert_eq!(b.taken(), 3);
        let m = b.clone().finish().unwrap();
        // (10 + 11 + 13) / 3 = 11; the u32 sum holds 3 × 65535.
        assert_eq!(m.data, [11, 65534]);
        assert_eq!((m.img_exp_dur, m.img_stack_cnt, m.bits_per_pixel), (2000, 3, 16));
        // The last frame's stats stay on the master.
        assert_eq!((m.min_adu, m.max_adu), (13, 65534));

        let mut short = MasterDarkBuilder::new(1000, 2);
        short.add(image(2, 1, &[1, 2])).unwrap();
        assert_eq!(short.add(image(1, 1, &[1])), Err("dark frame size changed"));
        assert!(short.finish().is_none());
    }

    #[test]
    fn dark_library_build_sequence() {
        let mut lib = DarkLibrary::default();
        lib.add_dark(flat(2, 2, 5, 1000));
        lib.add_dark(flat(2, 2, 5, 9000));
        let mut b = DarksBuild::dark_library(&mut lib, vec![500, 1500], 2, false);
        assert_eq!(b.status(), "Building master dark at 500 mSec: Taking dark frame 1/2");
        assert_eq!((b.next_capture(), b.progress()), (Some(500), (0, 4000)));
        b.add_frame(&mut lib, flat(2, 2, 10, 500));
        assert_eq!(b.status(), "Building master dark at 500 mSec: Taking dark frame 2/2");
        b.add_frame(&mut lib, flat(2, 2, 13, 500));
        // The 500 ms master is in the library before the next exposure.
        assert_eq!(lib.darks[&500].data, [11; 4]);
        assert_eq!(b.status(), "Building master dark at 1.5 sec: Taking dark frame 1/2");
        assert_eq!((b.next_capture(), b.progress()), (Some(1500), (1000, 4000)));
        b.add_frame(&mut lib, flat(2, 2, 20, 1500));
        assert!(b.wrapup(true).is_none());
        b.add_frame(&mut lib, flat(2, 2, 20, 1500));
        assert_eq!((b.state(), b.status(), b.next_capture()), (DarksBuildState::Done, "dark library built", None));
        assert_eq!(b.progress(), (4000, 4000));
        assert_eq!(lib.darks.keys().copied().collect::<Vec<_>>(), [500, 1000, 1500, 9000]);
        assert_eq!(b.wrapup(true).unwrap(), "Operation complete: dark library built");
        assert_eq!(b.wrapup(false).unwrap(), "Operation complete: Uncover guide scope\n\ndark library built");

        // A new library clears the darks; a failure keeps the masters built
        // so far and resets the gauge.
        let mut b = DarksBuild::dark_library(&mut lib, vec![1000, 2000], 1, true);
        assert!(lib.darks.is_empty());
        b.add_frame(&mut lib, flat(2, 2, 7, 1000));
        assert_eq!(b.status(), "Building master dark at 2.0 sec: Taking dark frame 1/1");
        b.capture_failed();
        assert_eq!((b.state(), b.status()), (DarksBuildState::Failed, "Operation failed - no changes have been made"));
        assert_eq!((b.progress(), b.next_capture()), ((0, 3000), None));
        assert_eq!(lib.darks.keys().copied().collect::<Vec<_>>(), [1000]);
        assert!(b.wrapup(false).is_none());

        // Cancel; a frame of another size fails; an empty range is done at
        // once; the frame count is held to 1..=20.
        let mut b = DarksBuild::dark_library(&mut lib, vec![1000], 0, false);
        assert_eq!(b.status(), "Building master dark at 1.0 sec: Taking dark frame 1/1");
        b.cancel();
        assert_eq!(b.state(), DarksBuildState::Cancelled);
        assert_eq!(b.status(), "Operation cancelled - no changes have been made");
        let mut b = DarksBuild::dark_library(&mut lib, vec![1000], 2, false);
        b.add_frame(&mut lib, flat(2, 2, 7, 1000));
        b.add_frame(&mut lib, flat(3, 2, 7, 1000));
        assert_eq!(b.state(), DarksBuildState::Failed);
        let b = DarksBuild::dark_library(&mut lib, vec![], 5, false);
        assert_eq!((b.state(), b.status()), (DarksBuildState::Done, "dark library built"));
    }

    #[test]
    fn defect_map_build_sequence() {
        let mut lib = DarkLibrary::default();
        // 30 s and 2 frames are held to the spin ranges: 15 s, 5 frames.
        let mut b = DarksBuild::defect_map(30, 2);
        assert_eq!(b.status(), "Taking darks to compute defect map:  Taking dark frame 1/5");
        assert_eq!((b.next_capture(), b.progress()), (Some(15000), (0, 75000)));
        for _ in 0..5 {
            b.add_frame(&mut lib, flat(2, 2, 3, 15000));
        }
        assert_eq!((b.state(), b.status()), (DarksBuildState::Done, "Master dark data files built"));
        assert!(lib.darks.is_empty());
        let m = b.take_master().unwrap();
        assert_eq!((m.img_exp_dur, m.img_stack_cnt, m.data[0]), (15000, 5, 3));
        assert_eq!(b.wrapup(true).unwrap(), "Operation complete: Master dark data files built");

        let mut b = DarksBuild::defect_map(5, 25);
        b.add_frame(&mut lib, flat(2, 2, 3, 5000));
        b.capture_failed();
        assert_eq!(b.status(), "Taking darks to compute defect map:  5.0 s dark FAILED");
        assert!(b.take_master().is_none());
        let mut b = DarksBuild::defect_map(10, 10);
        b.cancel();
        assert_eq!((b.state(), b.status()), (DarksBuildState::Cancelled, "Operation cancelled"));
    }

    #[test]
    fn exposure_labels_and_dark_range() {
        let labels: Vec<String> = [10, 50, 100, 1000, 1500, 2500, 9999, 10000, 15000, 30000]
            .iter()
            .map(|&ms| exposure_duration_label(ms))
            .collect();
        assert_eq!(labels, ["0.01 s", "0.05 s", "0.1 s", "1.0 s", "1.5 s", "2.5 s", "10.0 s", "10 s", "15 s", "30 s"]);
        assert_eq!(darks_min_exptime_default(false), "1.0 s");
        assert_eq!(darks_max_exptime_default(false), "6.0 s");
        assert_eq!(darks_min_exptime_default(true), "0.1 s");
        assert_eq!(darks_max_exptime_default(true), "2.5 s");

        // PHD2's list, the custom entry last and unsorted.
        let durations = [
            10, 20, 50, 100, 200, 500, 1000, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000, 6000, 7000, 8000, 9000,
            10000, 15000, 1200,
        ];
        assert_eq!(
            dark_lib_exposures(&durations, "1.0 s", "6.0 s"),
            [1000, 1200, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000, 6000]
        );
        assert_eq!(dark_lib_exposures(&durations, "2.5 s", "2.5 s"), [2500]);
        assert!(dark_lib_exposures(&durations, "6.0 s", "1.0 s").is_empty());
        assert!(dark_lib_exposures(&durations, "1.0 s", "45 s").is_empty());
    }

    #[test]
    fn file_names() {
        assert_eq!(dark_lib_file_name(1, 3), "PHD2_dark_lib_3.fit");
        assert_eq!(dark_lib_file_name(2, 3), "PHD2_dark_lib_2_3.fit");
        assert_eq!(defect_map_file_name(1, 7), "PHD2_defect_map_7.txt");
        assert_eq!(defect_map_master_file_name(3, 7), "PHD2_defect_map_master_3_7.fit");
        assert_eq!(defect_map_filter_file_name(1, 7), "PHD2_defect_map_master_filt_7.fit");
    }

    #[test]
    fn geometry_check_drops_what_no_longer_fits() {
        use FileGeometry::*;
        let mut g = DarkGeometry {
            darks_enabled: true,
            defect_map_enabled: true,
            darks_checked: true,
            defect_map_checked: false,
            prev_frame_size: (640, 480),
        };
        // Binning changed: both files are 640×480, the camera now 320×240.
        let out = g.check((320, 240), Size(640, 480), Size(640, 480));
        assert_eq!(
            out,
            GeometryActions {
                unload_defect_map: false,
                unload_darks: true,
                alerts: vec![DEFECT_MAP_MISMATCH, DARK_LIB_MISMATCH, DARKS_AND_DEFECT_MAP_MISMATCH],
            }
        );
        assert_eq!(g, DarkGeometry { prev_frame_size: (320, 240), ..DarkGeometry::default() });

        // Disabled items do not alert again; back at 640×480 they come back.
        assert!(g.check((320, 240), Size(640, 480), Size(640, 480)).alerts.is_empty());
        let out = g.check((640, 480), Size(640, 480), Size(640, 480));
        assert_eq!(out, GeometryActions::default());
        assert!(g.darks_enabled && g.defect_map_enabled);

        // Only the map gone (its .txt deleted): it is unloaded, no alert.
        g.defect_map_checked = true;
        let out = g.check((640, 480), Size(640, 480), Missing);
        assert_eq!(out, GeometryActions { unload_defect_map: true, unload_darks: false, alerts: vec![] });
        assert!(!g.defect_map_checked);

        // An unknown frame size accepts any present file, even unreadable.
        let mut g = DarkGeometry::default();
        g.check((0, 0), Unreadable, Size(1, 1));
        assert!(g.darks_enabled && g.defect_map_enabled);
        assert!(!FileGeometry::Unreadable.usable((640, 480)));
    }

    #[test]
    fn darks_kept_in_use_after_refresh_unchecks_them() {
        use FileGeometry::*;
        // AutoLoadDarks loaded a 640×480 library on a 320×240 camera:
        // SetDarkMenuState disables and unchecks the item, the darks stay.
        let mut g = DarkGeometry { darks_checked: true, ..DarkGeometry::default() };
        assert_eq!(g.refresh((320, 240), Size(640, 480), Missing), [DARK_LIB_MISMATCH]);
        assert!(!g.darks_enabled && !g.darks_checked);
        // Back at 640×480 the item comes back, unchecked; at 320×240 again
        // it alerts but nothing is unloaded, AutoLoadDarks left alone.
        assert_eq!(g.check((640, 480), Size(640, 480), Missing), GeometryActions::default());
        let out = g.check((320, 240), Size(640, 480), Missing);
        assert_eq!(out.alerts, [DARK_LIB_MISMATCH]);
        assert!(!out.unload_darks && !out.unload_defect_map);
    }

    #[test]
    fn file_geometry_reads_the_primary_header_only() {
        // A two-HDU library: the primary's size, whatever follows.
        let mut lib = DarkLibrary::default();
        lib.add_dark(flat(4, 3, 7, 1000));
        lib.add_dark(flat(4, 3, 9, 2000));
        let bytes = lib.to_fits("");
        assert_eq!(FileGeometry::from_fits(&bytes), FileGeometry::Size(4, 3));
        assert!(FileGeometry::from_fits(&bytes).usable((4, 3)));
        assert_eq!(FileGeometry::from_fits(b"nonsense"), FileGeometry::Unreadable);
    }

    #[test]
    fn refresh_alerts_dark_library_first() {
        use FileGeometry::*;
        let mut g = DarkGeometry { darks_enabled: true, defect_map_checked: true, ..DarkGeometry::default() };
        let alerts = g.refresh((320, 240), Size(640, 480), Size(640, 480));
        assert_eq!(alerts, [DARK_LIB_MISMATCH, DEFECT_MAP_MISMATCH]);
        assert!(!g.darks_enabled && !g.defect_map_enabled && !g.defect_map_checked);
        g.darks_checked = true;
        assert!(g.refresh((320, 240), Size(320, 240), Missing).is_empty());
        assert!(g.darks_enabled && g.darks_checked && !g.defect_map_enabled);
        // PHD2's texts, one line each.
        assert_eq!(
            DARK_LIB_MISMATCH,
            "Dark library does not match the camera in this profile. Check that you are connected to the camera you \
             want to use for guiding."
        );
        assert!(DARKS_AND_DEFECT_MAP_MISMATCH.ends_with("current camera. Check that you are connected to the camera you want to use for guiding."));
    }
}
