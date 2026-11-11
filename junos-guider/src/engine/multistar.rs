//! The guide star tracker of PHD2's `GuiderMultiStar`, without the guider
//! state machine around it (that is `guider.rs`): the per-frame position
//! update with its mass and jump checks, the secondary stars that refine the
//! offset in multi-star mode, star selection (by click or auto-select), the
//! subframe box and the valid lock area, and their profile settings.
//!
//! Everything PHD2 reads from `pFrame`, `pMount` or the guider comes in as
//! arguments ([`GuidingView`], [`FindParams`], the lock position, the
//! mount's calibration); time is UTC milliseconds (`wxGetUTCTimeMillis`)
//! from the caller. The quirks that change what clients see are kept, each
//! marked where it happens. The debug-log lines are collected in PHD2's
//! order and handed over by [`MultiStar::take_log`]; `Star::Find`'s own
//! lines are not among them (star.rs writes none).
//!
//! Ported from PHD2 src/guider_multistar.h, src/guider_multistar.cpp, src/guider.cpp (star settings) — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::VecDeque;

use super::algo::ParamLookup;
use super::calibration::{Calibration, camera_to_mount_logged};
use super::image::UsImage;
use super::log::cfmt::fmt_f;
use super::star::{AutoFindParams, FindMode, FindResult, GuideStar, Star, auto_sel_downsample};
use super::stats::DescriptiveStats;
use super::traits::{Point, Rect, phd_round};

pub const MIN_SEARCH_REGION: i32 = 7;
pub const DEFAULT_SEARCH_REGION: i32 = 15;
pub const MAX_SEARCH_REGION: i32 = 50;
/// Stars RefineOffset uses, the primary included.
pub const DEFAULT_MAX_STAR_COUNT: u32 = 9;
/// A primary offset beyond this many sigmas starts a stabilisation period.
pub const DEFAULT_STABILITY_SIGMAX: f64 = 5.0;
/// Stars AutoSelect keeps in multi-star mode.
pub const MAX_LIST_SIZE: usize = 12;
pub const DEFAULT_MASS_CHANGE_THRESHOLD: f64 = 0.5;
pub const DEFAULT_TOLERATE_JUMPS_THRESHOLD: f64 = 4.0;
pub const DEFAULT_MIN_STAR_HFD: f64 = 1.5;
/// `GetMinStarHFDFloor()`: a stored minimum HFD below this is replaced on load.
pub const MIN_STAR_HFD_FLOOR: f64 = 0.1;
pub const DEFAULT_MAX_STAR_HFD: f64 = 20.0;
pub const DEFAULT_MIN_STAR_SNR: f64 = 6.0;

/// The profile keys, PHD2's names.
pub mod keys {
    pub const SEARCH_REGION: &str = "/guider/onestar/SearchRegion";
    pub const MASS_CHANGE_THRESHOLD: &str = "/guider/onestar/MassChangeThreshold";
    pub const MASS_CHANGE_THRESHOLD_ENABLED: &str = "/guider/onestar/MassChangeThresholdEnabled";
    pub const TOLERATE_JUMPS_ENABLED: &str = "/guider/onestar/TolerateJumpsEnabled";
    pub const TOLERATE_JUMPS_THRESHOLD: &str = "/guider/onestar/TolerateJumpsThreshold";
    pub const MULTISTAR_ENABLED: &str = "/guider/multistar/enabled";
    pub const STAR_MIN_HFD: &str = "/guider/StarMinHFD";
    pub const STAR_MAX_HFD: &str = "/guider/StarMaxHFD";
    pub const STAR_MIN_SNR: &str = "/guider/StarMinSNR";
    pub const AUTO_SEL_DOWNSAMPLE: &str = "/guider/AutoSelDownsample";
}

// ------------------------------------------------------------------ settings

/// The star-tracking settings: `GuiderMultiStar::LoadProfileSettings` and
/// the star part of `Guider::LoadProfileSettings` (guider.cpp:225-241).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultiStarConfig {
    /// `/guider/onestar/SearchRegion`, half-width of the search box in px.
    pub search_region: i32,
    pub mass_change_threshold_enabled: bool,
    /// Allowed relative mass change (0.5 = 50 %).
    pub mass_change_threshold: f64,
    pub tolerate_jumps_enabled: bool,
    /// A jump is an offset over this many times the smoothed guide error.
    pub tolerate_jumps_threshold: f64,
    /// `m_multiStarMode`. Change it through [`MultiStar::set_multi_star_mode`].
    pub multi_star: bool,
    /// Every `find` of the tracker and AutoSelect bound the HFD by these.
    pub min_star_hfd: f64,
    pub max_star_hfd: f64,
    /// AutoSelect's minimum SNR (tracking itself uses `find`'s fixed 3).
    pub min_star_snr: f64,
    /// AutoSelect's downsampling: 0 is "Auto", else the factor.
    pub auto_sel_downsample: u32,
}

impl Default for MultiStarConfig {
    fn default() -> Self {
        MultiStarConfig {
            search_region: DEFAULT_SEARCH_REGION,
            mass_change_threshold_enabled: DEFAULT_MASS_CHANGE_THRESHOLD != 1.0,
            mass_change_threshold: DEFAULT_MASS_CHANGE_THRESHOLD,
            tolerate_jumps_enabled: false,
            tolerate_jumps_threshold: DEFAULT_TOLERATE_JUMPS_THRESHOLD,
            multi_star: false,
            min_star_hfd: DEFAULT_MIN_STAR_HFD,
            max_star_hfd: DEFAULT_MAX_STAR_HFD,
            min_star_snr: DEFAULT_MIN_STAR_SNR,
            auto_sel_downsample: 0,
        }
    }
}

impl MultiStarConfig {
    /// Read the profile through `get` (`None` = absent; a boolean is any
    /// non-zero value) with PHD2's defaults and load-time validation.
    ///
    /// PHD2 applies the multi-star flag through `SetMultiStarMode` on every
    /// load, at startup and on each `set_profile` (gear_dialog.cpp:1909).
    /// So the loader keeps its [`MultiStar`], takes the other fields from
    /// here and passes `multi_star` to [`MultiStar::set_multi_star_mode`],
    /// whose side effects (see there) are due on every load, unchanged
    /// value included.
    pub fn from_profile(get: ParamLookup) -> Self {
        let flag = |k: &str, d: bool| get(k).map_or(d, |v| v != 0.0);
        let mut c = MultiStarConfig::default();

        let thr = get(keys::MASS_CHANGE_THRESHOLD).unwrap_or(DEFAULT_MASS_CHANGE_THRESHOLD);
        c.set_mass_change_threshold(thr);
        // The default follows the value as read, before the validation.
        c.mass_change_threshold_enabled = flag(keys::MASS_CHANGE_THRESHOLD_ENABLED, thr != 1.0);
        c.tolerate_jumps_enabled = flag(keys::TOLERATE_JUMPS_ENABLED, false);
        c.tolerate_jumps_threshold = get(keys::TOLERATE_JUMPS_THRESHOLD).unwrap_or(DEFAULT_TOLERATE_JUMPS_THRESHOLD);
        c.set_search_region(get(keys::SEARCH_REGION).map_or(DEFAULT_SEARCH_REGION, |v| v as i32));
        c.multi_star = flag(keys::MULTISTAR_ENABLED, false);

        // guider.cpp:225: a value under the floor is taken as bogus (old
        // releases stored 0). The comment there says it becomes the default;
        // the code sets the floor, which is what PHD2 does.
        let min_hfd = get(keys::STAR_MIN_HFD).unwrap_or(DEFAULT_MIN_STAR_HFD);
        c.min_star_hfd = if min_hfd < MIN_STAR_HFD_FLOOR { MIN_STAR_HFD_FLOOR } else { min_hfd };
        c.max_star_hfd = get(keys::STAR_MAX_HFD).unwrap_or(DEFAULT_MAX_STAR_HFD);
        c.min_star_snr = get(keys::STAR_MIN_SNR).unwrap_or(DEFAULT_MIN_STAR_SNR);
        // guider.cpp:240: `wxMax(0, GetInt(...))` into an unsigned int
        c.auto_sel_downsample = get(keys::AUTO_SEL_DOWNSAMPLE).map_or(0, |v| (v as i32).max(0) as u32);
        c
    }

    /// `SetSearchRegion`: clamped to 7..=50; true (error) when it had to be.
    pub fn set_search_region(&mut self, v: i32) -> bool {
        self.search_region = v.clamp(MIN_SEARCH_REGION, MAX_SEARCH_REGION);
        self.search_region != v
    }

    /// `SetMassChangeThreshold`: a negative value is an error and sets the
    /// default instead.
    pub fn set_mass_change_threshold(&mut self, v: f64) -> bool {
        let bad = v < 0.0;
        self.mass_change_threshold = if bad { DEFAULT_MASS_CHANGE_THRESHOLD } else { v };
        bad
    }

    /// The effective values, which PHD2's setters write back to the profile
    /// on load. Booleans are 0 / 1.
    pub fn profile_values(&self) -> [(&'static str, f64); 10] {
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        [
            (keys::SEARCH_REGION, self.search_region as f64),
            (keys::MASS_CHANGE_THRESHOLD, self.mass_change_threshold),
            (keys::MASS_CHANGE_THRESHOLD_ENABLED, b(self.mass_change_threshold_enabled)),
            (keys::TOLERATE_JUMPS_ENABLED, b(self.tolerate_jumps_enabled)),
            (keys::TOLERATE_JUMPS_THRESHOLD, self.tolerate_jumps_threshold),
            (keys::MULTISTAR_ENABLED, b(self.multi_star)),
            (keys::STAR_MIN_HFD, self.min_star_hfd),
            (keys::STAR_MAX_HFD, self.max_star_hfd),
            (keys::STAR_MIN_SNR, self.min_star_snr),
            (keys::AUTO_SEL_DOWNSAMPLE, self.auto_sel_downsample as f64),
        ]
    }

    /// The `/guider/onestar/TolerateJumps*` pair as UpdateCurrentPosition
    /// uses it: the threshold, or 9e99 (no limit) when disabled.
    fn jump_tolerance(&self) -> f64 {
        if self.tolerate_jumps_enabled { self.tolerate_jumps_threshold } else { 9e99 }
    }
}

/// What the frame and the camera add to the settings for each `Star::Find`
/// (the HFD bounds come from [`MultiStarConfig`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FindParams {
    /// `MyFrame::GetStarFindMode()`. AutoSelect always finds centroids.
    pub mode: FindMode,
    /// `/camera/SaturationByADU` (true by default).
    pub saturation_by_adu: bool,
    /// `/camera/SaturationADU`, 0 while unset.
    pub saturation_adu: u16,
}

impl FindParams {
    /// `GuideCamera::GetSaturationADU()`: the level when saturation is by
    /// ADU, else 0 (the flat-top test).
    pub fn max_adu(&self) -> u16 {
        if self.saturation_by_adu { self.saturation_adu } else { 0 }
    }
}

/// The guider and frame state the tracker reads (PHD2 asks `pFrame`,
/// `pGuider`, `pMount` and `PhdController` for these).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GuidingView {
    /// `Guider::IsGuiding()`: the state is GUIDING.
    pub guiding: bool,
    /// `Guider::IsPaused()`.
    pub paused: bool,
    /// `PhdController::IsSettling()`.
    pub settling: bool,
    /// `MyFrame::GuidingRAOnly()`: the scope's Dec guide mode is None.
    pub ra_only: bool,
    /// `pMount->GetGuidingEnabled()`.
    pub guiding_enabled: bool,
    /// `Guider::CurrentErrorFrameCount()`.
    pub error_frame_count: u32,
    /// `Guider::CurrentErrorSmoothed(ra_only)`.
    pub error_smoothed: f64,
    /// `MyFrame::GetExposureInfo`: the exposure in ms (0 without a camera)
    /// and whether auto exposure is on.
    pub exposure_ms: i32,
    pub auto_exposure: bool,
}

// -------------------------------------------------------------- MassChecker

/// `MassChecker` (guider_multistar.cpp:52): rejects a star whose mass leaves
/// the band around the recent median — another star, a hot pixel or a cloud
/// took its place.
#[derive(Clone, Debug)]
pub struct MassChecker {
    /// (UTC ms, mass), the mass per ms of exposure under auto exposure.
    data: VecDeque<(i64, f64)>,
    high_mass: f64,
    low_mass: f64,
    time_window: i64,
    exposure: i32,
    is_auto_exposure: bool,
}

impl Default for MassChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl MassChecker {
    pub const DEFAULT_TIME_WINDOW_MS: i64 = 22500;

    pub fn new() -> Self {
        MassChecker {
            data: VecDeque::new(),
            high_mass: 0.0,
            low_mass: 9e99,
            // an abrupt change in mass will affect the median after approx
            // half the window
            time_window: 2 * Self::DEFAULT_TIME_WINDOW_MS,
            exposure: 0,
            is_auto_exposure: false,
        }
    }

    /// Masses taken at another exposure are not comparable: a change of
    /// exposure (without auto exposure) or of the auto flag starts over.
    pub fn set_exposure(&mut self, exposure: i32, is_auto: bool) {
        if is_auto != self.is_auto_exposure {
            self.is_auto_exposure = is_auto;
            self.exposure = exposure;
            self.reset();
        } else if exposure != self.exposure {
            self.exposure = exposure;
            if !self.is_auto_exposure {
                self.reset();
            }
        }
    }

    /// Under auto exposure the mass is compared per ms of exposure.
    pub fn adjusted_mass(&self, mass: f64) -> f64 {
        if self.is_auto_exposure { mass / self.exposure as f64 } else { mass }
    }

    /// Add a sample, dropping those older than the window.
    pub fn append_data(&mut self, mass: f64, now_ms: i64) {
        let oldest = now_ms - self.time_window;
        while self.data.front().is_some_and(|e| e.0 < oldest) {
            self.data.pop_front();
        }
        let adj = self.adjusted_mass(mass);
        self.data.push_back((now_ms, adj));
    }

    /// `CheckMass`: `Some(limits)` when `mass` is rejected, the limits being
    /// (low, median, high, spike) in mass units for the log. Needs five
    /// samples; moves the high and low water marks either way.
    pub fn check_mass(&mut self, mass: f64, threshold: f64) -> Option<[f64; 4]> {
        if self.data.len() < 5 {
            return None;
        }

        // std::nth_element: the upper median for an even count
        let mut tmp: Vec<f64> = self.data.iter().map(|e| e.1).collect();
        let mid = tmp.len() / 2;
        tmp.sort_by(f64::total_cmp);
        let med = tmp[mid];

        if med > self.high_mass {
            self.high_mass = med;
        }
        if med < self.low_mass {
            self.low_mass = med;
        }

        // let the low water mark drift to follow the median so that it moves
        // back up after a period of intermittent clouds has brought it down
        self.low_mass += 0.05 * (med - self.low_mass);

        let mut limits = [
            self.low_mass * (1.0 - threshold),
            med,
            self.high_mass * (1.0 + threshold),
            // when mass is depressed by sky conditions, still reject a large
            // spike, even under the high water mark threshold
            med * (1.0 + 2.0 * threshold),
        ];

        let adj = self.adjusted_mass(mass);
        let reject = adj < limits[0] || adj > limits[2] || adj > limits[3];
        if !reject {
            return None;
        }
        if self.is_auto_exposure {
            // back to mass-like numbers for the caller's log
            for l in &mut limits {
                *l *= self.exposure as f64;
            }
        }
        Some(limits)
    }

    pub fn reset(&mut self) {
        self.data.clear();
        self.high_mass = 0.0;
        self.low_mass = 9e99;
    }
}

// ---------------------------------------------------------- DistanceChecker

/// `DistanceChecker::State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DistanceState {
    Guiding,
    /// A jump was seen: frames are rejected until the star comes back or
    /// the wait runs out.
    Waiting,
    /// The wait ran out: frames are accepted again, jumps included.
    Recovering,
}

/// `DistanceChecker` (guider_multistar.cpp:601): while guiding, rejects for
/// up to 5 s a star that jumped far beyond the usual guide error (with
/// "tolerate jumps" off the limit is 9e99, so only an activation by a lost
/// star or a mass change can make it reject).
#[derive(Clone, Copy, Debug)]
pub struct DistanceChecker {
    state: DistanceState,
    expires: i64,
    force_tolerance: f64,
}

impl Default for DistanceChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl DistanceChecker {
    pub const WAIT_INTERVAL_MS: i64 = 5000;
    const MIN_FRAMES_FOR_STATS: u32 = 10;

    /// PHD2's is a static object: `expires` and `force_tolerance` start at
    /// zero, which the constructor leaves to static initialisation.
    pub fn new() -> Self {
        DistanceChecker { state: DistanceState::Guiding, expires: 0, force_tolerance: 0.0 }
    }

    pub fn state(&self) -> DistanceState {
        self.state
    }

    /// `Activate`, on a lost star or a mass change: wait with a tolerance of
    /// 2× the smoothed error, whatever the configured one.
    pub fn activate(&mut self, now_ms: i64, log: &mut Vec<String>) {
        if self.state == DistanceState::Guiding {
            log.push("DistanceChecker: activated".into());
            self.state = DistanceState::Waiting;
            self.expires = now_ms + Self::WAIT_INTERVAL_MS;
            self.force_tolerance = 2.0;
        }
    }

    /// `_CheckDistance`: any offset is "small" unless guiding, unpaused,
    /// not settling and with ten error samples to compare against.
    fn small_offset(distance: f64, view: &GuidingView, tolerance: f64, log: &mut Vec<String>) -> bool {
        if !view.guiding || view.paused || view.settling || view.error_frame_count < Self::MIN_FRAMES_FOR_STATS {
            return true;
        }
        let threshold = tolerance * view.error_smoothed;
        // PHD2 tests `distance > threshold`, so a NaN is small
        if distance > threshold {
            log.push(format!(
                "DistanceChecker: reject for large offset ({} > {}) avgDist = {} count = {}",
                fmt_f(distance, 2),
                fmt_f(threshold, 2),
                fmt_f(view.error_smoothed, 2),
                view.error_frame_count
            ));
            return false;
        }
        true
    }

    /// `CheckDistance`: false rejects the frame.
    pub fn check_distance(
        &mut self,
        distance: f64,
        view: &GuidingView,
        tolerance: f64,
        now_ms: i64,
        log: &mut Vec<String>,
    ) -> bool {
        let tolerance = if self.force_tolerance != 0.0 { self.force_tolerance } else { tolerance };
        let small = Self::small_offset(distance, view, tolerance, log);

        if self.state == DistanceState::Guiding {
            if small {
                return true;
            }
            log.push("DistanceChecker: activated".into());
            self.state = DistanceState::Waiting;
            self.expires = now_ms + Self::WAIT_INTERVAL_MS;
            return false;
        }

        if self.state == DistanceState::Waiting {
            if small {
                log.push("DistanceChecker: deactivated".into());
                self.state = DistanceState::Guiding;
                self.force_tolerance = 0.0;
                return true;
            }
            if now_ms < self.expires {
                return false;
            }
            // timed out: fall through to recovering
            log.push("DistanceChecker: begin recovering".into());
            self.state = DistanceState::Recovering;
        }

        // Recovering. Quirk kept: unlike the way out of Waiting, this one
        // leaves the forced 2.0 tolerance in place until the next Waiting
        // ends with a small offset.
        if small {
            log.push("DistanceChecker: deactivated".into());
            self.state = DistanceState::Guiding;
        }
        true
    }
}

// ----------------------------------------------------------------- tracker

/// The star part of PHD2's `FrameDroppedInfo` (guidinglog.h:89) for a frame
/// whose star was not accepted; the guider adds the frame number, the time
/// and the average distance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameErrorInfo {
    pub star_error: FindResult,
    pub star_mass: f64,
    pub star_snr: f64,
    pub star_hfd: f64,
    /// `StarStatusStr`, the `Status` of `StarLost` and of the DROP row.
    pub status: &'static str,
}

/// [`MultiStar::update_current_position`] when the frame is dropped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionLost {
    pub info: FrameErrorInfo,
    pub reason: LossReason,
}

impl PositionLost {
    /// `ResetAutoExposure()` is due: every loss but "No star selected",
    /// which PHD2 returns before (and logs as `LogImageStarDeselected`).
    pub fn reset_auto_exposure(&self) -> bool {
        self.reason != LossReason::NoStar
    }
}

/// Which of `UpdateCurrentPosition`'s exits dropped the frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LossReason {
    /// No star selected: a plain return, no throw.
    NoStar,
    /// The find failed (the throw at guider_multistar.cpp:958).
    NotFound,
    /// The mass changed too much (989): the limits (low, median, high,
    /// spike), for the `Mass: %.f vs %.f` status (with the median).
    MassChange([f64; 4]),
    /// The star was found but jumped too far (1020): PHD2 shows
    /// "Recovering" in the status bar (the guider's status then replaces it).
    Jump,
}

/// [`MultiStar::update_current_position`] when the star was accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct PositionUpdate {
    /// The new primary star.
    pub primary: Star,
    /// `cameraOfs`: star − lock, refined by the secondary stars in
    /// multi-star mode. `None` without a lock position.
    pub camera_ofs: Option<Point>,
    /// `mountOfs`: `None` without a lock position or a calibration.
    pub mount_ofs: Option<Point>,
    /// The distance the jump check saw and `ImageLogger::LogImage` gets:
    /// |dx| when guiding RA only, else Euclidean; 0 without a lock.
    pub raw_distance: f64,
    /// `UpdateCurrentDistance(distance, distanceRA)` is due with these: the
    /// refined offset's length when refined, else `raw_distance`, and
    /// |mount RA| (0 uncalibrated). `None` without a lock position.
    pub current_distance: Option<(f64, f64)>,
    /// `AdjustAutoExposure(snr)` is due with this (the primary's SNR).
    pub auto_exposure_snr: f64,
    /// `StarStatus` (guider_multistar.cpp:422), the frame's status text:
    /// the guider shows it and ends its "UpdateGuideState exits: " debug
    /// line with it.
    pub status: String,
}

/// What [`MultiStar::auto_select`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoSelectResult {
    /// The new primary, when one was found; the caller sets the lock there.
    pub star: Option<Star>,
    /// AutoFind's saturation level to save to `/camera/SaturationADU`, set
    /// even when no star was found.
    pub saturation_adu_set: Option<u16>,
    /// AutoFind picked a star but the find on it failed ("Unable to find",
    /// guider_multistar.cpp:482). Both finds are the same, from the same
    /// pixel, so PHD2 never gets there in practice.
    pub find_failed: bool,
}

/// The guider state as `GetBoundingBox` sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoxState {
    /// SELECTED, CALIBRATING_PRIMARY, CALIBRATING_SECONDARY: around the star.
    Tracking,
    /// GUIDING, with the lock position.
    Guiding(Point),
    /// Any other state: the full frame.
    Other,
}

/// `AutoSelect`'s edge allowance (guider_multistar.cpp:463): a star must
/// leave room for the calibration moves of each connected mount not yet
/// calibrated. Pass `Some(CalibrationTotDistance())` for those, `None` for
/// the others.
pub fn autoselect_edge_allowance(primary: Option<i32>, secondary: Option<i32>) -> i32 {
    let mut edge = 0;
    for d in [primary, secondary].into_iter().flatten() {
        edge = edge.max(d);
    }
    edge
}

/// `IsValidSecondaryStarPosition`: looser than the lock area, as `find`
/// copes with a search box partly off the frame.
fn valid_secondary(pt: Point, w: i32, h: i32) -> bool {
    pt.x >= 5.0 && pt.x + 5.0 < w as f64 && pt.y >= 5.0 && pt.y + 5.0 < h as f64
}

/// PHD2 runs `Find` on a star it keeps, and an `Error` (bad search bounds)
/// throws before `PeakVal` is set, so the kept star's peak stays
/// (star.cpp:164); [`Star::find`] returns a fresh star instead.
fn find_into(kept: &mut Star, found: Star) -> bool {
    let peak_val = kept.peak_val;
    *kept = found;
    if found.result == FindResult::Error {
        kept.peak_val = peak_val;
    }
    kept.was_found()
}

/// `StarStatus` (guider_multistar.cpp:422). Quirk kept: the exposure is in
/// ms, so the `Exp=%d ms` branch only shows for 0 or less.
fn star_status(star: &Star, view: &GuidingView) -> String {
    let mut status = format!("m={} SNR={}", fmt_f(star.mass, 0), fmt_f(star.snr, 1));
    if star.result == FindResult::Saturated {
        status += " Saturated";
    }
    if view.auto_exposure {
        let exp = view.exposure_ms;
        if exp >= 1 {
            status += &format!(" Exp={} s", fmt_f(exp as f64 / 1000.0, 1));
        } else {
            status += &format!(" Exp={exp} ms");
        }
    }
    status
}

/// `GuiderMultiStar`'s own state: the primary star, the secondary list and
/// the checks run on each frame.
#[derive(Clone, Debug)]
pub struct MultiStar {
    pub cfg: MultiStarConfig,
    primary: Star,
    /// The primary at index 0 (as AutoSelect found it), then the secondaries.
    guide_stars: Vec<GuideStar>,
    primary_dist_stats: DescriptiveStats,
    mass_checker: MassChecker,
    /// A file-scope static in PHD2, so never reset with the star.
    distance_checker: DistanceChecker,
    stabilizing: bool,
    lock_position_moved: bool,
    stars_used: u32,
    /// Debug-log lines since the last [`MultiStar::take_log`].
    log: Vec<String>,
}

impl MultiStar {
    pub fn new(cfg: MultiStarConfig) -> Self {
        MultiStar {
            cfg,
            primary: Star::new(),
            guide_stars: Vec::new(),
            primary_dist_stats: DescriptiveStats::new(),
            mass_checker: MassChecker::new(),
            distance_checker: DistanceChecker::new(),
            stabilizing: false,
            lock_position_moved: false,
            stars_used: 0,
            log: Vec::new(),
        }
    }

    /// The debug-log lines written since the last call, without their
    /// newlines.
    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// `Star::Find` with the settings' HFD bounds.
    fn find(&self, img: &UsImage, x: f64, y: f64, fp: &FindParams) -> Star {
        let c = &self.cfg;
        Star::find(img, c.search_region, x, y, fp.mode, c.min_star_hfd, c.max_star_hfd, fp.max_adu())
    }

    /// `PrimaryStar()` / `CurrentPosition()`.
    pub fn primary(&self) -> &Star {
        &self.primary
    }

    /// `IsLocked()`: the primary was found on the last frame.
    pub fn is_locked(&self) -> bool {
        self.primary.was_found()
    }

    pub fn guide_stars(&self) -> &[GuideStar] {
        &self.guide_stars
    }

    /// Stars that went into the last offset, the primary included.
    pub fn stars_used(&self) -> u32 {
        self.stars_used
    }

    /// `SetMultiStarMode`'s own part. True when the mode was just turned on:
    /// PHD2 then, from SELECTED on, stops guiding, invalidates the star
    /// (full reset) and auto-selects again so the secondaries are found,
    /// restarting guiding, if it was calibrating or guiding, when that
    /// succeeds ("AutoFind forced"); on a failure guiding stays stopped and
    /// the star invalidated.
    ///
    /// The rest is the caller's, on every call: write
    /// `/guider/multistar/enabled`, the debug line `MultiStar mode
    /// enabled|disabled` (+ `, AutoFind forced` after a successful forced
    /// AutoSelect), then `GuideParamChange{Name:"MultiStar", Value:"true"|
    /// "false"}` — a JSON string, PHD2 goes through its wxString overload —
    /// and the guide-log line `INFO: Guiding parameter change, MultiStar =
    /// true|false`, which is forced: written even when not guiding, and it
    /// keeps the log file (guidinglog.cpp:736-742).
    pub fn set_multi_star_mode(&mut self, on: bool) -> bool {
        let turned_on = on && !self.cfg.multi_star;
        self.cfg.multi_star = on;
        if turned_on {
            self.primary_dist_stats.clear_all();
        }
        if !on {
            self.stabilizing = false;
        }
        turned_on
    }

    /// `ClearSecondaryStars`: keep the primary's entry only.
    pub fn clear_secondary_stars(&mut self) {
        if self.guide_stars.len() > 1 {
            self.guide_stars.truncate(1);
            self.log.push("MultiStar: secondary guide stars cleared".into());
        }
    }

    /// The list part of a star picked by hand (`OnLClick`, after the lock
    /// was set on it): single-star guiding, the list holding at most one
    /// entry — the new primary when it was empty, else the old first entry.
    pub fn keep_primary_only(&mut self) {
        self.clear_secondary_stars();
        if self.guide_stars.is_empty() {
            self.guide_stars.push(GuideStar::from_star(self.primary));
        }
    }

    /// The `GuiderMultiStar::SetLockPosition` override, to call after the
    /// guider's own `SetLockPosition` succeeded: in multi-star mode the
    /// secondaries' reference points no longer hold, so wait for the primary
    /// to settle and re-find them.
    pub fn lock_position_set(&mut self) {
        if self.cfg.multi_star {
            self.lock_position_moved = true;
            self.stabilizing = true;
            self.log.push("MultiStar: stabilizing after lock position change".into());
        }
    }

    /// `InvalidateCurrentPosition`. Without a full reset the position stays,
    /// and the next frame still searches there.
    pub fn invalidate_current_position(&mut self, full_reset: bool) {
        self.primary.invalidate();
        if full_reset {
            self.primary.x = 0.0;
            self.primary.y = 0.0;
        }
    }

    /// `SetCurrentPosition` (a click, `set_lock_position` with `exact:
    /// false`): find the star near `pos`. True when found. A position off
    /// the frame changes nothing; otherwise the primary becomes the find's
    /// result, found or not, and the mass history starts over.
    pub fn set_current_position(&mut self, img: &UsImage, pos: Point, fp: &FindParams) -> bool {
        self.log.push(format!("SetCurrentPosition({},{})", fmt_f(pos.x, 2), fmt_f(pos.y, 2)));
        if pos.x <= 0.0 || pos.x >= img.w as f64 || pos.y <= 0.0 || pos.y >= img.h as f64 {
            return false;
        }
        self.mass_checker.reset();
        let found = self.find(img, pos.x, pos.y, fp);
        find_into(&mut self.primary, found)
    }

    /// `AutoSelect`'s star part: `GuideStar::AutoFind` with up to 12 stars
    /// in multi-star mode (1 with subframes or in single-star mode), then a
    /// centroid find on its pick. `edge_allowance` comes from
    /// [`autoselect_edge_allowance`]; `pixel_scale` (″/px, 1.0 unknown)
    /// resolves the "Auto" downsampling. On success the caller sets the
    /// lock position there (then [`MultiStar::lock_position_set`]) and
    /// advances the state from SELECTING; on failure PHD2 has already
    /// replaced the list and, when AutoFind succeeded, the primary. When
    /// the caller's lock position fails, it writes the debug line
    /// `GuiderMultiStar::AutoSelect failed.` itself.
    pub fn auto_select(
        &mut self,
        img: &UsImage,
        roi: Option<Rect>,
        edge_allowance: i32,
        use_subframes: bool,
        pixel_scale: f64,
        fp: &FindParams,
    ) -> AutoSelectResult {
        self.log.push("GuiderMultiStar::AutoSelect enter".into());
        let mut out = AutoSelectResult { star: None, saturation_adu_set: None, find_failed: false };
        if img.data.is_empty() {
            return out; // No Current Image, and no "failed." line
        }

        let c = self.cfg;
        let af = AutoFindParams {
            downsample: auto_sel_downsample(c.auto_sel_downsample, pixel_scale),
            min_hfd: c.min_star_hfd,
            max_hfd: c.max_star_hfd,
            min_snr: c.min_star_snr,
            saturation_by_adu: fp.saturation_by_adu,
            saturation_adu: fp.saturation_adu,
        };
        let max_stars = if use_subframes || !c.multi_star { 1 } else { MAX_LIST_SIZE };
        let found =
            GuideStar::auto_find(img, edge_allowance, c.search_region, roi, &mut self.guide_stars, max_stars, &af);
        out.saturation_adu_set = found.saturation_adu_set;
        let Some(pos) = found.position else {
            self.log.push("GuiderMultiStar::AutoSelect failed.".into());
            return out;
        };

        self.mass_checker.reset();

        // AutoFind may just have set the camera's saturation level, which
        // this find then uses.
        let fp = match found.saturation_adu_set {
            Some(level) => FindParams { mode: FindMode::Centroid, saturation_by_adu: true, saturation_adu: level },
            None => FindParams { mode: FindMode::Centroid, ..*fp },
        };
        let star = self.find(img, pos.x, pos.y, &fp);
        if !find_into(&mut self.primary, star) {
            self.log.push("GuiderMultiStar::AutoSelect failed.".into());
            out.find_failed = true;
            return out;
        }

        let mut list = format!("MultiStar: List ({}): ", self.guide_stars.len());
        for gs in &self.guide_stars {
            let s = &gs.star;
            list += &format!("{{{}, {}}}({}), ", fmt_f(s.x, 2), fmt_f(s.y, 2), fmt_f(s.snr, 1));
        }
        self.log.push(list);

        self.primary_dist_stats.clear_all();
        out.star = Some(self.primary);
        out
    }

    /// `GetBoundingBox`: the subframe to request next, a `2·SR+1` square
    /// clipped to the frame, or an empty rect for the full frame. While
    /// guiding it stays on the lock position as long as the star is within
    /// `SR/3` (both truncated to integers) of it.
    pub fn bounding_box(&self, state: BoxState, force_full_frame: bool, frame_size: (i32, i32)) -> Rect {
        let sr = self.cfg.search_region;
        let pos = match state {
            BoxState::Tracking => self.primary.position(),
            BoxState::Guiding(lock) => {
                let cur = self.primary.position();
                if (cur.distance(&lock) as i32) > sr / 3 { cur } else { lock }
            }
            BoxState::Other => return Rect::default(),
        };
        if !self.primary.was_found() || force_full_frame {
            return Rect::default();
        }
        // SubframeRect
        let (x, y) = (phd_round(pos.x), phd_round(pos.y));
        let box_ = Rect::new(x - sr, y - sr, 2 * sr + 1, 2 * sr + 1);
        box_.intersect(&Rect::new(0, 0, frame_size.0, frame_size.1))
    }

    /// `IsValidLockPosition`: far enough inside the current frame for
    /// `find`'s search box (PHD2's current image is never null, an empty
    /// one is 0 × 0).
    pub fn is_valid_lock_position(&self, pt: Point, img: &UsImage) -> bool {
        // PHD2's `pt.X + 1 + m_searchRegion`, added left to right
        let sr = self.cfg.search_region as f64;
        pt.x >= 1.0 + sr && pt.x + 1.0 + sr < img.w as f64 && pt.y >= 1.0 + sr && pt.y + 1.0 + sr < img.h as f64
    }

    /// `RefineOffset` (guider_multistar.cpp:720): average the primary's
    /// offset with the secondaries' moves from their reference points,
    /// weighted by SNR relative to the primary, while guiding with more than
    /// one star. The average replaces `camera_ofs` only when it is shorter
    /// than the primary-only offset; true then.
    fn refine_offset(&mut self, img: &UsImage, camera_ofs: &mut Point, view: &GuidingView, fp: &FindParams) -> bool {
        let c = self.cfg;
        let mut find = |x: f64, y: f64| {
            Star::find(img, c.search_region, x, y, fp.mode, c.min_star_hfd, c.max_star_hfd, fp.max_adu())
        };
        self.refine_with(camera_ofs, view, (img.w, img.h), &mut find)
    }

    /// RefineOffset over any star finder, so the tests can script the finds.
    fn refine_with(
        &mut self,
        ofs: &mut Point,
        view: &GuidingView,
        (w, h): (i32, i32),
        find: &mut dyn FnMut(f64, f64) -> Star,
    ) -> bool {
        self.stars_used = 1;
        if !(view.guiding && self.guide_stars.len() > 1 && view.guiding_enabled && !view.settling) {
            return false;
        }

        let orig = *ofs;
        let mut sum_weights = 1.0;
        let mut sum_x = orig.x;
        let mut sum_y = orig.y;
        let primary_distance = sum_x.hypot(sum_y);
        let mut primary_sigma = 0.0;
        self.primary_dist_stats.add_value(primary_distance);

        if self.primary_dist_stats.count() > 5 {
            primary_sigma = self.primary_dist_stats.sigma();
            if !self.stabilizing && primary_distance > DEFAULT_STABILITY_SIGMAX * primary_sigma {
                self.stabilizing = true;
                self.log.push("MultiStar: large primary error, entering stabilization period".into());
            } else if self.stabilizing && primary_distance <= 2.0 * primary_sigma {
                self.stabilizing = false;
                self.log.push("MultiStar: exiting stabilization period".into());
                if self.lock_position_moved {
                    // re-find the secondaries around the moved primary; the
                    // reference points then reflect the current positions
                    self.lock_position_moved = false;
                    self.log.push("MultiStar: updating star positions after lock position change".into());
                    let primary = self.primary.position();
                    for gs in self.guide_stars.iter_mut().skip(1) {
                        let expected = primary + gs.offset_from_primary;
                        let at = if valid_secondary(expected, w, h) { expected } else { gs.star.position() };
                        if find_into(&mut gs.star, find(at.x, at.y)) {
                            gs.reference_point = gs.star.position();
                            gs.was_lost = false;
                        } else {
                            // a lost star keeps its offset from the primary
                            // for a later recovery
                            gs.was_lost = true;
                        }
                    }
                    return false;
                }
            }
        } else {
            self.stabilizing = true; // get some data for primary star movement
        }

        if self.stabilizing || self.guide_stars.len() <= 1 || (sum_x == 0.0 && sum_y == 0.0) {
            return false;
        }

        // AppendStarUse: `[#index dX,dY,weight,flag] `, the index being the
        // star's place in the list at that moment
        let star_use = |i: usize, dx: f64, dy: f64, wt: f64, flag: &str| {
            format!("[#{i} {},{},{},{flag}] ", fmt_f(dx, 2), fmt_f(dy, 2), fmt_f(wt, 2))
        };
        let mut secondary_info = String::from("MultiStar: ");
        let primary_pos = self.primary.position();
        let primary_snr = self.primary.snr;
        let mut averaged = false;
        let mut valid_stars = 0;
        // Quirk kept: a star dropped for its zero count leaves `erasures`
        // set (that branch skips the reset at the bottom), so the next star
        // that reaches the bottom is not stepped past and is measured twice
        // — found again, counted and weighted twice.
        let mut erasures = false;
        let mut i = 1;
        while i < self.guide_stars.len() {
            if self.stars_used >= DEFAULT_MAX_STAR_COUNT || self.guide_stars.len() == 1 {
                break;
            }
            let gs = &mut self.guide_stars[i];
            // a lost star is looked for at its offset from the primary,
            // others where they were last found
            let at = if gs.was_lost { primary_pos + gs.offset_from_primary } else { gs.star.position() };
            if find_into(&mut gs.star, find(at.x, at.y)) {
                let dx = gs.star.x - gs.reference_point.x;
                let dy = gs.star.y - gs.reference_point.y;
                gs.was_lost = false;
                self.stars_used += 1;

                if dx != 0.0 || dy != 0.0 {
                    // exactly zero movement on an axis is suspect
                    if dx == 0.0 || dy == 0.0 {
                        gs.zero_count += 1;
                    } else if gs.zero_count > 0 {
                        gs.zero_count -= 1;
                    }
                    if gs.zero_count == 5 {
                        secondary_info += &star_use(i, 0.0, 0.0, 0.0, "DZ");
                        self.guide_stars.remove(i);
                        erasures = true;
                        continue;
                    }

                    // an excursion unlike the primary's is a "miss"
                    if dx.hypot(dy) > 2.5 * primary_sigma {
                        gs.miss_count += 1;
                        if gs.miss_count > 10 {
                            // reset the reference point to wherever it is now
                            gs.reference_point = gs.star.position();
                            gs.miss_count = 0;
                            secondary_info += &star_use(i, dx, dy, 0.0, "R");
                        } else {
                            secondary_info += &star_use(i, dx, dy, 0.0, &format!("M{}", gs.miss_count));
                        }
                        i += 1;
                        continue;
                    } else if gs.miss_count > 0 {
                        gs.miss_count -= 1;
                    }

                    let wt = gs.star.snr / primary_snr;
                    sum_x += wt * dx;
                    sum_y += wt * dy;
                    sum_weights += wt;
                    averaged = true;
                    valid_stars += 1;
                    secondary_info += &star_use(i, dx, dy, wt, "U");
                } else {
                    // exactly zero on both axes, probably a hot pixel
                    secondary_info += &star_use(i, 0.0, 0.0, 0.0, "DZ");
                    self.guide_stars.remove(i);
                    erasures = true;
                }
            } else {
                // not found in its search region
                secondary_info += &star_use(i, 0.0, 0.0, 0.0, "L");
                gs.was_lost = true;
            }
            if !erasures {
                i += 1;
            } else {
                erasures = false;
            }
        }
        self.log.push(secondary_info);

        if !averaged {
            return false;
        }
        sum_x /= sum_weights;
        sum_y /= sum_weights;
        // apply the average only if it is smaller than the single-star delta
        let refined = sum_x.hypot(sum_y) < primary_distance;
        if refined {
            *ofs = Point::new(sum_x, sum_y);
        }
        self.log.push(format!(
            "{}, {valid_stars} included, MultiStar: {{{}, {}}}, one-star: {{{}, {}}}",
            if refined { "refined" } else { "single-star" },
            fmt_f(sum_x, 2),
            fmt_f(sum_y, 2),
            fmt_f(orig.x, 2),
            fmt_f(orig.y, 2)
        ));
        refined
    }

    /// `UpdateCurrentPosition` (guider_multistar.cpp:925): find the primary
    /// near its last position, check its mass and the size of its move, then
    /// take it and compute the offsets from `lock`. `cal` is the primary
    /// mount's calibration, `None` when it is not calibrated. `now_ms` is
    /// UTC milliseconds.
    pub fn update_current_position(
        &mut self,
        img: &UsImage,
        fp: &FindParams,
        lock: Option<Point>,
        view: &GuidingView,
        now_ms: i64,
        cal: Option<&Calibration>,
    ) -> Result<PositionUpdate, PositionLost> {
        let lost = |star_error, s: &Star, status, reason| PositionLost {
            info: FrameErrorInfo { star_error, star_mass: s.mass, star_snr: s.snr, star_hfd: s.hfd, status },
            reason,
        };

        // An invalidated star away from (0, 0) is still searched for.
        if !self.primary.is_valid() && self.primary.x == 0.0 && self.primary.y == 0.0 {
            self.log.push("UpdateCurrentPosition: no star selected".into());
            return Err(lost(FindResult::Error, &Star::new(), "No star selected", LossReason::NoStar));
        }

        let mut new_star = self.primary;
        let found = self.find(img, self.primary.x, self.primary.y, fp);
        if !find_into(&mut new_star, found) {
            self.primary.set_error(new_star.result);
            self.distance_checker.activate(now_ms, &mut self.log);
            return Err(lost(new_star.result, &Star::new(), new_star.status_str(), LossReason::NotFound));
        }

        // is it the same star? compare the mass
        if self.cfg.mass_change_threshold_enabled {
            self.mass_checker.set_exposure(view.exposure_ms, view.auto_exposure);
            let threshold = self.cfg.mass_change_threshold;
            if let Some(limits) = self.mass_checker.check_mass(new_star.mass, threshold) {
                self.primary.set_error(FindResult::MassChange);
                // the status is the primary's: "No star selected" when it
                // was invalidated
                let reason = LossReason::MassChange(limits);
                let l = lost(FindResult::MassChange, &new_star, self.primary.status_str(), reason);
                self.log.push(format!(
                    "UpdateCurrentPosition: star mass new={} exp={} thresh={}% limits=({}, {}, {})",
                    fmt_f(new_star.mass, 1),
                    fmt_f(limits[1], 1),
                    fmt_f(threshold * 100.0, 0),
                    fmt_f(limits[0], 1),
                    fmt_f(limits[2], 1),
                    fmt_f(limits[3], 1)
                ));
                self.mass_checker.append_data(new_star.mass, now_ms);
                self.distance_checker.activate(now_ms, &mut self.log);
                return Err(l);
            }
        }

        let distance = match lock {
            Some(l) if view.ra_only => (new_star.x - l.x).abs(),
            Some(l) => new_star.position().distance(&l),
            None => 0.0,
        };
        let tolerance = self.cfg.jump_tolerance();
        if !self.distance_checker.check_distance(distance, view, tolerance, now_ms, &mut self.log) {
            self.primary.set_error(FindResult::Error);
            return Err(lost(FindResult::Error, &new_star, self.primary.status_str(), LossReason::Jump));
        }

        self.primary = new_star;
        self.mass_checker.append_data(new_star.mass, now_ms);

        let mut out = PositionUpdate {
            primary: new_star,
            camera_ofs: None,
            mount_ofs: None,
            raw_distance: distance,
            current_distance: None,
            auto_exposure_snr: new_star.snr,
            status: star_status(&new_star, view),
        };
        if let Some(l) = lock {
            let mut ofs = new_star.position() - l;
            let mut reported = distance;
            if self.cfg.multi_star && self.guide_stars.len() > 1 {
                if self.refine_offset(img, &mut ofs, view, fp) {
                    reported = ofs.x.hypot(ofs.y); // reported to the server clients
                }
            } else {
                self.stars_used = 1;
            }
            let mount = cal.and_then(|c| camera_to_mount_logged(Some(ofs), c, &mut self.log));
            out.camera_ofs = Some(ofs);
            out.mount_ofs = mount;
            out.current_distance = Some((reported, mount.map_or(0.0, |m| m.x.abs())));
        }
        Ok(out)
    }

    /// `GetSettingsSummary`, the guide-log header line (the multi-star
    /// branch really ends in a newline and a space).
    pub fn settings_summary(&self) -> String {
        let mut s = format!("Search region = {} px, Star mass tolerance ", self.cfg.search_region);
        if self.cfg.mass_change_threshold_enabled {
            s += &format!("= {}%", fmt_f(self.cfg.mass_change_threshold * 100.0, 1));
        } else {
            s += "disabled";
        }
        if self.cfg.multi_star {
            s += &format!(", Multi-star mode, list size = {}\n ", self.guide_stars.len());
        } else {
            s += ", Single-star mode\n";
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    // ----------------------------------------------------- synthetic frames

    /// A Gaussian star: centre, peak above the background, sigma.
    #[derive(Clone, Copy)]
    struct G {
        x: f64,
        y: f64,
        amp: f64,
        sigma: f64,
    }

    /// Fixed pattern noise in −20..=20 ADU, an integer hash the C++ harness
    /// computes the same way.
    fn noise(x: i32, y: i32) -> i32 {
        let mut h = (x as u32).wrapping_mul(73856093) ^ (y as u32).wrapping_mul(19349663);
        h ^= h >> 13;
        h = h.wrapping_mul(0x5bd1e995);
        h ^= h >> 15;
        (h % 41) as i32 - 20
    }

    /// A 16-bit `w × h` frame: 1000 ADU plus the noise plus the stars,
    /// rounded as `floor(v + 0.5)` — the harness's `render`.
    fn render(w: i32, h: i32, stars: &[G]) -> UsImage {
        let mut img = UsImage::new(w, h);
        img.bits_per_pixel = 16;
        for y in 0..h {
            for x in 0..w {
                let mut v = 1000.0 + noise(x, y) as f64;
                for s in stars {
                    let dx = x as f64 - s.x;
                    let dy = y as f64 - s.y;
                    v += s.amp * (-(dx * dx + dy * dy) / (2.0 * s.sigma * s.sigma)).exp();
                }
                *img.pixel_mut(x, y) = (v + 0.5).floor().clamp(0.0, 65535.0) as u16;
            }
        }
        img
    }

    const FP: FindParams = FindParams { mode: FindMode::Centroid, saturation_by_adu: false, saturation_adu: 0 };

    fn guiding_view() -> GuidingView {
        GuidingView {
            guiding: true,
            guiding_enabled: true,
            error_frame_count: 20,
            error_smoothed: 1.0,
            exposure_ms: 2000,
            ..Default::default()
        }
    }

    fn dc_code(s: DistanceState) -> u8 {
        match s {
            DistanceState::Guiding => 0,
            DistanceState::Waiting => 1,
            DistanceState::Recovering => 2,
        }
    }

    // ------------------------------------------------------------ settings

    #[test]
    fn config_defaults_and_load_rules() {
        let d = MultiStarConfig::default();
        assert_eq!(d, MultiStarConfig::from_profile(&|_| None));
        assert_eq!(d.search_region, 15);
        assert!(d.mass_change_threshold_enabled); // 0.5 != 1.0
        assert_eq!(d.mass_change_threshold, 0.5);
        assert!(!d.tolerate_jumps_enabled && d.tolerate_jumps_threshold == 4.0);
        assert!(!d.multi_star);
        assert_eq!((d.min_star_hfd, d.max_star_hfd, d.min_star_snr), (1.5, 20.0, 6.0));
        assert_eq!(d.auto_sel_downsample, 0);

        let profile = |entries: &'static [(&'static str, f64)]| {
            MultiStarConfig::from_profile(&move |k| entries.iter().find(|e| e.0 == k).map(|e| e.1))
        };
        // a negative downsample is 0 (Auto)
        assert_eq!(profile(&[(keys::AUTO_SEL_DOWNSAMPLE, -2.0)]).auto_sel_downsample, 0);
        assert_eq!(profile(&[(keys::AUTO_SEL_DOWNSAMPLE, 3.0)]).auto_sel_downsample, 3);
        // a threshold of 1.0 disables the check by default
        assert!(!profile(&[(keys::MASS_CHANGE_THRESHOLD, 1.0)]).mass_change_threshold_enabled);
        // a negative one becomes the default, but the enabled default was
        // taken from the value as read
        let c = profile(&[(keys::MASS_CHANGE_THRESHOLD, -1.0)]);
        assert_eq!(c.mass_change_threshold, 0.5);
        assert!(c.mass_change_threshold_enabled);
        assert_eq!(profile(&[(keys::SEARCH_REGION, 3.0)]).search_region, 7);
        assert_eq!(profile(&[(keys::SEARCH_REGION, 60.0)]).search_region, 50);
        assert_eq!(profile(&[(keys::STAR_MIN_HFD, 0.05)]).min_star_hfd, 0.1); // the floor, not 1.5
        assert_eq!(profile(&[(keys::STAR_MIN_HFD, 0.5)]).min_star_hfd, 0.5);
        let c = profile(&[(keys::MULTISTAR_ENABLED, 1.0), (keys::TOLERATE_JUMPS_ENABLED, 1.0)]);
        assert!(c.multi_star && c.tolerate_jumps_enabled);

        let mut c = d;
        assert!(c.set_search_region(51) && c.search_region == 50);
        assert!(!c.set_search_region(20) && c.search_region == 20);
        assert!(c.set_mass_change_threshold(-0.1) && c.mass_change_threshold == 0.5);
        assert_eq!(c.profile_values()[0], (keys::SEARCH_REGION, 20.0));
        assert_eq!(c.profile_values()[2], (keys::MASS_CHANGE_THRESHOLD_ENABLED, 1.0));
        assert_eq!(c.profile_values()[9], (keys::AUTO_SEL_DOWNSAMPLE, 0.0));
    }

    #[test]
    fn settings_summary_lines() {
        let mut ms = MultiStar::new(MultiStarConfig::default());
        assert_eq!(ms.settings_summary(), "Search region = 15 px, Star mass tolerance = 50.0%, Single-star mode\n");
        ms.cfg.mass_change_threshold_enabled = false;
        ms.cfg.multi_star = true;
        ms.guide_stars = vec![GuideStar::default(); 3];
        assert_eq!(
            ms.settings_summary(),
            "Search region = 15 px, Star mass tolerance disabled, Multi-star mode, list size = 3\n "
        );
    }

    // --------------------------------------------------------- MassChecker

    #[test]
    fn mass_checker_rejects_a_3x_jump_and_accepts_slow_drift() {
        let mut mc = MassChecker::new();
        mc.set_exposure(2000, false);
        let mut t = 0;
        for m in [1000.0, 1020.0, 980.0, 1010.0] {
            // fewer than five samples: never rejects
            assert_eq!(mc.check_mass(m * 3.0, 0.5), None);
            mc.append_data(m, t);
            t += 2000;
        }
        mc.append_data(990.0, t);
        assert!(mc.check_mass(3000.0, 0.5).is_some());
        // 3 % per frame for 30 frames: the median keeps up
        for k in 0..30 {
            t += 2000;
            let m = 1000.0 * (1.0 + 0.03 * k as f64);
            assert_eq!(mc.check_mass(m, 0.5), None, "frame {k}");
            mc.append_data(m, t);
        }
        // a new exposure starts over (no auto exposure)
        mc.set_exposure(3000, false);
        assert_eq!(mc.check_mass(1e9, 0.5), None);
    }

    #[test]
    fn mass_checker_matches_phd2() {
        let mut mc = MassChecker::new();
        for &(t, exp, aut, mass, limits) in MASS {
            mc.set_exposure(exp, aut);
            assert_eq!(mc.check_mass(mass, 0.5), limits, "t = {t}");
            mc.append_data(mass, t);
        }
    }

    // ----------------------------------------------------- DistanceChecker

    #[test]
    fn distance_checker_waits_on_a_jump_then_recovers() {
        let mut dc = DistanceChecker::new();
        let mut log = Vec::new();
        let view = guiding_view(); // smoothed error 1 px, 20 frames
        assert!(dc.check_distance(2.0, &view, 4.0, 0, &mut log));
        // a 10 px jump while guiding: rejected for 5 s
        assert!(!dc.check_distance(10.0, &view, 4.0, 1000, &mut log));
        assert_eq!(dc.state(), DistanceState::Waiting);
        assert!(!dc.check_distance(10.0, &view, 4.0, 5999, &mut log));
        // then accepted again, still far
        assert!(dc.check_distance(10.0, &view, 4.0, 6000, &mut log));
        assert_eq!(dc.state(), DistanceState::Recovering);
        assert!(dc.check_distance(1.0, &view, 4.0, 7000, &mut log));
        assert_eq!(dc.state(), DistanceState::Guiding);
        // with jumps tolerated (9e99) nothing is a jump
        assert!(dc.check_distance(1e6, &view, 9e99, 8000, &mut log));
        // PHD2 tests `distance > threshold`: a NaN passes
        assert!(dc.check_distance(f64::NAN, &view, 4.0, 9000, &mut log));
        let reject = "DistanceChecker: reject for large offset (10.00 > 4.00) avgDist = 1.00 count = 20";
        let begin = "DistanceChecker: begin recovering";
        let (on, off) = ("DistanceChecker: activated", "DistanceChecker: deactivated");
        assert_eq!(log, [reject, on, reject, reject, begin, off]);
    }

    #[test]
    fn distance_checker_matches_phd2() {
        let mut dc = DistanceChecker::new();
        let mut log = Vec::new();
        for &(op, dist, guiding, paused, settling, cnt, tol, t, ok, state) in DIST {
            let view = GuidingView { guiding, paused, settling, error_frame_count: cnt, ..guiding_view() };
            let r = if op == 'A' {
                dc.activate(t, &mut log);
                true
            } else {
                dc.check_distance(dist, &view, tol, t, &mut log)
            };
            assert_eq!((r, dc_code(dc.state())), (ok, state), "t = {t}");
        }
    }

    // ------------------------------------------------- box and lock area

    #[test]
    fn bounding_box_and_lock_area_match_phd2() {
        let mut ms = MultiStar::new(MultiStarConfig::default());
        for &(st, px, py, found, lx, ly, force, (x, y, w, h)) in BOXES {
            let state = match st {
                2 | 3 | 4 => BoxState::Tracking,
                6 => BoxState::Guiding(Point::new(lx, ly)),
                _ => BoxState::Other,
            };
            ms.primary.set_xy(px, py);
            ms.primary.set_error(if found { FindResult::Ok } else { FindResult::LowSnr });
            assert_eq!(ms.bounding_box(state, force, (128, 96)), Rect::new(x, y, w, h), "({px}, {py})");
        }
        let img = UsImage::new(128, 96);
        for &(x, y, ok) in LOCKS {
            assert_eq!(ms.is_valid_lock_position(Point::new(x, y), &img), ok, "({x}, {y})");
        }
        assert!(!ms.is_valid_lock_position(Point::new(50.0, 50.0), &UsImage::default()));
    }

    // -------------------------------------------------------- RefineOffset

    /// A tracker in multi-star mode, guiding past the stabilisation (six
    /// primary distances of sigma ~0.55 already in), with secondaries at
    /// `refs` of the given SNRs, primary SNR 20.
    fn refining(refs: &[(f64, f64, f64)]) -> MultiStar {
        let mut ms = MultiStar::new(MultiStarConfig { multi_star: true, ..Default::default() });
        ms.primary = Star { x: 60.0, y: 50.0, snr: 20.0, result: FindResult::Ok, valid: true, ..Star::new() };
        ms.guide_stars.push(GuideStar::from_star(ms.primary));
        for &(x, y, snr) in refs {
            let s = Star { x, y, snr, result: FindResult::Ok, valid: true, ..Star::new() };
            ms.guide_stars.push(GuideStar::from_star(s));
        }
        for v in [1.0, 0.0, 1.0, 0.0, 1.0, 0.0] {
            ms.primary_dist_stats.add_value(v);
        }
        ms
    }

    /// A finder that puts every secondary at its reference plus `shift`.
    fn shifted(ms: &MultiStar, shift: Point) -> impl FnMut(f64, f64) -> Star + use<> {
        let stars: Vec<Star> = ms.guide_stars[1..].iter().map(|g| g.star).collect();
        move |x: f64, y: f64| {
            let s = stars.iter().find(|s| s.x as i32 == x as i32 && s.y as i32 == y as i32).unwrap();
            Star { x: s.x + shift.x, y: s.y + shift.y, ..*s }
        }
    }

    #[test]
    fn refine_offset_averages_by_snr() {
        let mut ms = refining(&[(20.0, 20.0, 10.0), (100.0, 20.0, 20.0), (20.0, 80.0, 40.0)]);
        let mut find = shifted(&ms, Point::new(0.1, 0.05));
        let mut ofs = Point::new(0.5, 0.3);
        assert!(ms.refine_with(&mut ofs, &guiding_view(), (128, 96), &mut find));
        // weights 1 (primary), 0.5, 1, 2
        assert!((ofs.x - (0.5 + 3.5 * 0.1) / 4.5).abs() < 1e-12);
        assert!((ofs.y - (0.3 + 3.5 * 0.05) / 4.5).abs() < 1e-12);
        assert_eq!(ms.stars_used, 4);
        assert!(!ms.stabilizing);
    }

    #[test]
    fn refine_offset_keeps_the_primary_offset_when_the_average_is_worse() {
        let mut ms = refining(&[(20.0, 20.0, 10.0), (100.0, 20.0, 20.0), (20.0, 80.0, 40.0)]);
        let mut find = shifted(&ms, Point::new(0.4, 0.3));
        let mut ofs = Point::new(0.05, 0.0);
        assert!(!ms.refine_with(&mut ofs, &guiding_view(), (128, 96), &mut find));
        assert_eq!(ofs, Point::new(0.05, 0.0));
        assert_eq!(ms.stars_used, 4); // all three were still measured
        // nothing happens unless guiding with guiding enabled and settled
        for view in [
            GuidingView { guiding: false, ..guiding_view() },
            GuidingView { guiding_enabled: false, ..guiding_view() },
            GuidingView { settling: true, ..guiding_view() },
        ] {
            assert!(!ms.refine_with(&mut ofs, &view, (128, 96), &mut |_: f64, _: f64| -> Star { unreachable!() }));
            assert_eq!(ms.stars_used, 1);
        }
    }

    /// The scripted scene of the C++ harness: a star (x, y, SNR, lost).
    struct Ts {
        x: f64,
        y: f64,
        snr: f64,
        lost: bool,
    }

    const R: [(f64, f64); 10] = [
        (60.0, 50.0),
        (20.0, 20.0),
        (100.0, 20.0),
        (20.0, 80.0),
        (100.0, 80.0),
        (40.0, 30.0),
        (80.0, 30.0),
        (40.0, 75.0),
        (80.0, 75.0),
        (120.0, 50.0),
    ];

    /// The field's drift: a jump at 12, then (from 20) a move to the new
    /// lock position (64, 47).
    fn drift(f: i32) -> (f64, f64) {
        if f < 20 {
            let mut fx = 0.3 * (0.7 * f as f64).sin();
            let mut fy = 0.25 * (0.5 * f as f64).cos();
            if f == 12 {
                fx += 2.5;
                fy -= 1.5;
            }
            (fx, fy)
        } else {
            let mut m = 1.0;
            for _ in 0..f - 19 {
                m *= 0.5;
            }
            (4.0 * (1.0 - m) + 0.1 * (0.7 * f as f64).sin(), -3.0 * (1.0 - m) + 0.1 * (0.5 * f as f64).cos())
        }
    }

    /// Star 0 is the primary; 2 never moves in y (zero count), 4 wanders
    /// off from frame 6 (misses), 5 never moves (a hot pixel), 6 is lost on
    /// frames 9–11, 9 sits so near the right edge that the re-find after
    /// the lock move looks where it last was.
    fn truth(f: i32) -> Vec<Ts> {
        let (fx, fy) = drift(f);
        (0..10)
            .map(|k| {
                let mut t = Ts {
                    x: R[k].0 + fx,
                    y: R[k].1 + fy,
                    snr: if k == 0 { 25.0 } else { 8.0 + 2.0 * k as f64 },
                    lost: false,
                };
                if k == 2 {
                    t.y = R[k].1;
                }
                if k == 4 && f >= 6 {
                    t.x += 1.5;
                    t.y += 1.5;
                }
                if k == 5 {
                    t.x = R[k].0;
                    t.y = R[k].1;
                }
                if k == 6 && (9..=11).contains(&f) {
                    t.lost = true;
                }
                if k >= 7 {
                    t.x += 0.02 * k as f64;
                    t.y -= 0.01 * k as f64;
                }
                t
            })
            .collect()
    }

    struct Refine {
        f: i32,
        refined: bool,
        ofs: (f64, f64),
        stab: bool,
        moved: bool,
        used: u32,
        req: &'static [(i32, i32)],
        stars: Option<&'static [(f64, f64, f64, f64, u32, u32, bool)]>,
        log: &'static [&'static str],
    }

    #[test]
    fn refine_offset_matches_phd2() {
        let mut ms = MultiStar::new(MultiStarConfig { multi_star: true, ..Default::default() });
        let mut lock = Point::new(60.0, 50.0);
        for (k, r) in R.iter().enumerate() {
            let snr = if k == 0 { 25.0 } else { 8.0 + 2.0 * k as f64 };
            let s = Star { x: r.0, y: r.1, snr, result: FindResult::Ok, valid: true, ..Star::new() };
            let mut gs = GuideStar::from_star(s);
            gs.offset_from_primary = Point::new(r.0 - R[0].0, r.1 - R[0].1);
            ms.guide_stars.push(gs);
        }
        let view = guiding_view();
        for g in SCRIPTED {
            let f = g.f;
            let t = truth(f);
            ms.primary.set_xy(t[0].x, t[0].y);
            ms.primary.snr = t[0].snr;
            ms.primary.set_error(FindResult::Ok);
            if f == 20 {
                lock = Point::new(64.0, 47.0);
                ms.lock_position_set();
            }
            let mut ofs = ms.primary.position() - lock;
            let mut req = Vec::new();
            let mut find = |x: f64, y: f64| {
                let (xi, yi) = (x as i32, y as i32);
                req.push((xi, yi));
                let mut best = 0;
                let mut bd = 0.0;
                for (k, s) in t.iter().enumerate() {
                    let d = (s.x - xi as f64).hypot(s.y - yi as f64);
                    if k == 0 || d < bd {
                        best = k;
                        bd = d;
                    }
                }
                let s = &t[best];
                if bd > 15.0 || s.lost {
                    Star { x: xi as f64, y: yi as f64, result: FindResult::LowSnr, valid: true, ..Star::new() }
                } else {
                    Star {
                        x: s.x,
                        y: s.y,
                        mass: 1000.0,
                        snr: s.snr,
                        hfd: 3.0,
                        peak_val: 0,
                        result: FindResult::Ok,
                        valid: true,
                    }
                }
            };
            let refined = ms.refine_with(&mut ofs, &view, (128, 96), &mut find);
            assert_eq!(
                (refined, (ofs.x, ofs.y), ms.stabilizing, ms.lock_position_moved, ms.stars_used),
                (g.refined, g.ofs, g.stab, g.moved, g.used),
                "frame {f}"
            );
            assert_eq!(req, g.req, "frame {f}");
            if let Some(stars) = g.stars {
                let got: Vec<_> = ms.guide_stars[1..]
                    .iter()
                    .map(|s| {
                        let p = s.reference_point;
                        (s.star.x, s.star.y, p.x, p.y, s.miss_count, s.zero_count, s.was_lost)
                    })
                    .collect();
                assert_eq!(got, stars, "frame {f}");
            }
            assert_eq!(ms.take_log(), g.log, "frame {f}");
        }
    }

    // ----------------------------------------------- UpdateCurrentPosition

    #[test]
    fn update_current_position_tracks_a_04px_shift() {
        let star = |dx: f64| [G { x: 50.3 + dx, y: 40.7, amp: 4000.0, sigma: 1.6 }];
        let img0 = render(100, 80, &star(0.0));
        let mut ms = MultiStar::new(MultiStarConfig::default());
        assert!(ms.set_current_position(&img0, Point::new(50.0, 41.0), &FP));
        let lock = ms.primary().position();
        assert!((lock.x - 50.3).abs() < 0.05 && (lock.y - 40.7).abs() < 0.05);

        let view = guiding_view();
        for (k, dx) in [0.4, 0.8, 1.2].into_iter().enumerate() {
            let img = render(100, 80, &star(dx));
            let up = ms.update_current_position(&img, &FP, Some(lock), &view, 2000 * (k as i64 + 1), None).unwrap();
            let ofs = up.camera_ofs.unwrap();
            assert!((ofs.x - dx).abs() < 0.05 && ofs.y.abs() < 0.05, "{ofs:?}");
            assert_eq!(up.mount_ofs, None); // uncalibrated
            assert_eq!(up.current_distance, Some((up.raw_distance, 0.0)));
            assert_eq!(up.auto_exposure_snr, up.primary.snr);
            assert_eq!(ms.stars_used(), 1);
        }
        // RA only: the distance is |dx|
        let img = render(100, 80, &star(0.8));
        let ra = GuidingView { ra_only: true, ..view };
        let up =
            ms.update_current_position(&img, &FP, Some(Point::new(lock.x, lock.y - 3.0)), &ra, 8000, None).unwrap();
        assert_eq!(up.raw_distance, (up.primary.x - lock.x).abs());
        // no lock yet: no offsets, no distance update
        let up = ms.update_current_position(&img, &FP, None, &view, 10000, None).unwrap();
        assert_eq!((up.camera_ofs, up.current_distance, up.raw_distance), (None, None, 0.0));
    }

    #[test]
    fn update_current_position_without_a_star() {
        let img = render(100, 80, &[]);
        let mut ms = MultiStar::new(MultiStarConfig::default());
        // nothing selected: no search, no auto-exposure reset
        let l = ms.update_current_position(&img, &FP, None, &guiding_view(), 0, None).unwrap_err();
        assert_eq!(l.info.status, "No star selected");
        assert_eq!((l.info.star_error, l.reason), (FindResult::Error, LossReason::NoStar));
        assert!(!l.reset_auto_exposure());

        // an invalidated star keeps its position and is searched for
        let star = render(100, 80, &[G { x: 50.3, y: 40.7, amp: 4000.0, sigma: 1.6 }]);
        assert!(ms.set_current_position(&star, Point::new(50.0, 41.0), &FP));
        ms.invalidate_current_position(false);
        assert!(!ms.is_locked());
        assert!(ms.update_current_position(&star, &FP, None, &guiding_view(), 0, None).is_ok());

        // lost: the find's error, zero measurements, the checker waits
        let l = ms.update_current_position(&img, &FP, None, &guiding_view(), 0, None).unwrap_err();
        assert!(matches!(l.info.star_error, FindResult::LowSnr | FindResult::LowMass));
        assert_eq!(l.info.status, l.info.star_error.status_str());
        assert_eq!((l.info.star_mass, l.info.star_snr, l.reason), (0.0, 0.0, LossReason::NotFound));
        assert!(l.reset_auto_exposure());
        assert_eq!(ms.primary().result, l.info.star_error);
        assert_eq!(ms.distance_checker.state(), DistanceState::Waiting);

        ms.invalidate_current_position(true);
        assert_eq!(ms.primary().position(), Point::new(0.0, 0.0));
    }

    struct Frame {
        f: i32,
        err: bool,
        info: Option<(i32, f64, f64, f64, &'static str)>,
        primary: (f64, f64, f64, f64, f64, i32, bool),
        cam: Option<(f64, f64)>,
        mount: Option<(f64, f64)>,
        ucd: Option<(f64, f64)>,
        adjust: f64,
        reset: bool,
        stab: bool,
        used: u32,
        list: usize,
        dc: u8,
        /// `errorInfo->status`: `StarStatus` when found, else the drop's.
        status: &'static str,
        /// What UpdateCurrentPosition shows itself (`pFrame->StatusMsg`).
        msg: &'static str,
        log: &'static [&'static str],
    }

    /// One `update_current_position` against its PHD2 row.
    fn check_frame(ms: &mut MultiStar, r: Result<PositionUpdate, PositionLost>, g: &Frame) {
        let f = g.f;
        let p = ms.primary();
        assert_eq!((p.x, p.y, p.mass, p.snr, p.hfd, p.result.error_code(), p.valid), g.primary, "frame {f}");
        let pair = |p: Option<Point>| p.map(|p| (p.x, p.y));
        match r {
            Ok(up) => {
                assert!(!g.err && g.info.is_none() && !g.reset && g.msg.is_empty());
                assert_eq!(up.primary, *p);
                assert_eq!((pair(up.camera_ofs), pair(up.mount_ofs)), (g.cam, g.mount), "frame {f}");
                assert_eq!((up.current_distance, up.auto_exposure_snr), (g.ucd, g.adjust), "frame {f}");
                assert_eq!(up.status, g.status, "frame {f}");
            }
            Err(l) => {
                assert!(g.err && g.cam.is_none() && g.ucd.is_none() && g.adjust == -1.0);
                let i = l.info;
                let info = (i.star_error.error_code(), i.star_mass, i.star_snr, i.star_hfd, i.status);
                assert_eq!((Some(info), l.reset_auto_exposure()), (g.info, g.reset), "frame {f}");
                let msg = match l.reason {
                    LossReason::MassChange(m) => format!("Mass: {} vs {}", fmt_f(i.star_mass, 0), fmt_f(m[1], 0)),
                    LossReason::Jump => "Recovering".into(),
                    LossReason::NoStar | LossReason::NotFound => String::new(),
                };
                assert_eq!((i.status, msg.as_str()), (g.status, g.msg), "frame {f}");
            }
        }
        assert_eq!(
            (ms.stabilizing, ms.stars_used, ms.guide_stars.len(), dc_code(ms.distance_checker.state())),
            (g.stab, g.used, g.list, g.dc),
            "frame {f}"
        );
        assert_eq!(ms.take_log(), g.log, "frame {f}");
    }

    /// The harness's calibration: x angle 0.3, 0.05 rad of orthogonality
    /// error.
    fn calibration() -> Calibration {
        Calibration { x_angle: 0.3, y_angle: 0.3 + PI / 2.0 + 0.05, ..Default::default() }
    }

    #[test]
    fn update_current_position_matches_phd2() {
        let base = [
            G { x: 40.3, y: 50.6, amp: 3000.0, sigma: 1.6 },
            G { x: 90.7, y: 30.2, amp: 2000.0, sigma: 1.6 },
            G { x: 100.1, y: 75.4, amp: 1200.0, sigma: 1.6 },
            G { x: 20.4, y: 20.8, amp: 900.0, sigma: 1.6 },
            G { x: 64.2, y: 80.3, amp: 1500.0, sigma: 1.6 }, // never moves
        ];
        let cfg = MultiStarConfig { tolerate_jumps_enabled: true, multi_star: true, ..Default::default() };
        let mut ms = MultiStar::new(cfg);
        let cal = calibration();
        let mut lock = Point::default();
        let mut golden = FRAMES.iter();

        for f in 0..=24 {
            let (fx, fy) = if f < 13 {
                (0.35 * (0.9 * f as f64).sin(), 0.3 * (0.6 * f as f64).cos() - 0.3)
            } else {
                let mut m = 1.0;
                for _ in 0..f - 12 {
                    m *= 0.6;
                }
                (3.0 * (1.0 - m) + 0.05 * (1.3 * f as f64).sin(), -2.0 * (1.0 - m) + 0.05 * (1.1 * f as f64).cos())
            };
            let stars: Vec<G> = base
                .iter()
                .enumerate()
                .map(|(k, s)| {
                    let mut s = *s;
                    if k != 4 {
                        s.x += fx;
                        s.y += fy;
                    }
                    if k == 0 && f == 12 {
                        s.amp *= 3.0; // a mass change
                    }
                    s
                })
                .collect();
            let img = render(128, 96, &stars);

            if f == 0 {
                assert!(ms.set_current_position(&img, Point::new(40.0, 51.0), &FP));
                // From PHD2's C++: SetCurrentPosition's centroid.
                assert_eq!(ms.primary().position(), Point::new(40.309405180182232, 50.592021395217593));
                lock = ms.primary().position();
                ms.lock_position_set();
                assert_eq!(
                    ms.take_log(),
                    ["SetCurrentPosition(40.00,51.00)", "MultiStar: stabilizing after lock position change"]
                );
                ms.guide_stars.push(GuideStar::from_star(*ms.primary()));
                for s in &base[1..] {
                    let found = Star::find(&img, 15, s.x, s.y, FindMode::Centroid, 1.5, 20.0, 0);
                    let mut gs = GuideStar::from_star(found);
                    gs.offset_from_primary = found.position() - ms.primary().position();
                    ms.guide_stars.push(gs);
                }
                continue;
            }
            if f == 13 {
                lock = Point::new(lock.x + 3.0, lock.y - 2.0); // a dither
                ms.lock_position_set();
            }

            let view = GuidingView {
                settling: (13..=15).contains(&f),
                error_frame_count: f as u32,
                error_smoothed: 0.6,
                ..guiding_view()
            };
            let r = ms.update_current_position(&img, &FP, Some(lock), &view, 1_000_000 + 2000 * f as i64, Some(&cal));
            let g = golden.next().unwrap();
            assert_eq!(g.f, f);
            check_frame(&mut ms, r, g);
        }
        assert!(golden.next().is_none());
    }

    /// A lone star under auto exposure, guiding with jumps tolerated: a
    /// 5 px jump is held off for 5 s then taken (Recovering), an RA-only
    /// frame ignores a Dec jump, a mass change forces the 2.0 tolerance, a
    /// lost star activates the checker.
    #[test]
    fn update_current_position_jumps_match_phd2() {
        let cfg = MultiStarConfig { tolerate_jumps_enabled: true, ..Default::default() };
        let mut ms = MultiStar::new(cfg);
        let cal = calibration();
        let mut lock = Point::default();
        let mut golden = JUMPS.iter();
        for f in 0..=14 {
            let (mut dx, mut dy, mut amp) = (0.1 * (0.9 * f as f64).sin(), 0.1 * (0.7 * f as f64).cos(), 3000.0);
            if (4..=8).contains(&f) {
                dx += 5.0;
                dy += 1.0;
            }
            match f {
                10 => dy += 5.0,
                11 => amp *= 3.0,
                13 => amp = 0.0,
                _ => {}
            }
            let img = render(100, 80, &[G { x: 50.3 + dx, y: 40.6 + dy, amp, sigma: 1.6 }]);
            if f == 0 {
                assert!(ms.set_current_position(&img, Point::new(50.0, 41.0), &FP));
                lock = ms.primary().position();
                ms.take_log();
                continue;
            }
            let view = GuidingView {
                ra_only: f == 10,
                error_smoothed: 0.6,
                exposure_ms: 1500,
                auto_exposure: true,
                ..guiding_view()
            };
            let r = ms.update_current_position(&img, &FP, Some(lock), &view, 2_000_000 + 2000 * f as i64, Some(&cal));
            let g = golden.next().unwrap();
            assert_eq!(g.f, f);
            check_frame(&mut ms, r, g);
        }
        assert!(golden.next().is_none());
    }

    #[test]
    fn star_status_and_kept_peak() {
        let star = Star { mass: 1234.5, snr: 12.34, result: FindResult::Saturated, valid: true, ..Star::new() };
        let view = GuidingView { exposure_ms: 2500, ..Default::default() };
        assert_eq!(star_status(&star, &view), "m=1234 SNR=12.3 Saturated");
        let auto = GuidingView { auto_exposure: true, ..view };
        assert_eq!(star_status(&star, &auto), "m=1234 SNR=12.3 Saturated Exp=2.5 s");
        // the exposure is in ms: the "ms" branch only shows at 0
        assert_eq!(star_status(&star, &GuidingView { exposure_ms: 0, ..auto }), "m=1234 SNR=12.3 Saturated Exp=0 ms");

        // a find that fails on its bounds keeps the kept star's peak, any
        // other result brings its own
        let mut kept = Star { peak_val: 900, ..star };
        assert!(!find_into(&mut kept, Star { x: 3.0, ..Star::new() }));
        assert_eq!((kept.x, kept.peak_val, kept.mass), (3.0, 900, 0.0));
        let low = Star { result: FindResult::LowSnr, valid: true, ..Star::new() };
        assert!(!find_into(&mut kept, low));
        assert_eq!(kept.peak_val, 0);
    }

    // ---------------------------------------------------- star selection

    #[test]
    fn set_current_position_rules() {
        let img = render(100, 80, &[G { x: 50.3, y: 40.7, amp: 4000.0, sigma: 1.6 }]);
        let mut ms = MultiStar::new(MultiStarConfig::default());
        // off the frame: nothing happens
        for p in [Point::new(0.0, 40.0), Point::new(100.0, 40.0), Point::new(50.0, 80.0)] {
            assert!(!ms.set_current_position(&img, p, &FP));
            assert!(!ms.primary().is_valid());
        }
        // a miss still replaces the primary
        assert!(!ms.set_current_position(&img, Point::new(10.0, 10.0), &FP));
        assert!(ms.primary().is_valid() && !ms.is_locked());
        assert!(ms.set_current_position(&img, Point::new(51.0, 40.0), &FP));
        assert!(ms.is_locked());
        assert_eq!(ms.take_log()[..2], ["SetCurrentPosition(0.00,40.00)", "SetCurrentPosition(100.00,40.00)"]);
    }

    #[test]
    fn auto_select_wrapper() {
        let img = render(
            160,
            120,
            &[
                G { x: 50.3, y: 40.6, amp: 6000.0, sigma: 1.6 },
                G { x: 110.7, y: 80.2, amp: 3000.0, sigma: 1.6 },
                G { x: 30.4, y: 90.8, amp: 2000.0, sigma: 1.6 },
            ],
        );
        let fp = FindParams { mode: FindMode::Peak, saturation_by_adu: true, saturation_adu: 0 };
        let mut ms = MultiStar::new(MultiStarConfig { multi_star: true, ..Default::default() });
        ms.primary_dist_stats.add_value(1.0);
        let r = ms.auto_select(&img, None, 0, false, 1.0, &fp);
        let s = r.star.unwrap();
        assert!((s.x - 50.3).abs() < 0.05 && (s.y - 40.6).abs() < 0.05);
        assert_eq!(*ms.primary(), s);
        assert_eq!(r.saturation_adu_set, Some(65535));
        assert_eq!(ms.guide_stars().len(), 3);
        assert_eq!(ms.primary_dist_stats.count(), 0);
        assert_eq!(
            ms.take_log(),
            [
                "GuiderMultiStar::AutoSelect enter",
                "MultiStar: List (3): {50.30, 40.60}(212.9), {110.70, 80.19}(146.7), {30.42, 90.82}(118.2), "
            ]
        );

        // subframes or single-star mode: the primary alone
        assert_eq!(ms.auto_select(&img, None, 0, true, 1.0, &fp).star, Some(s));
        assert_eq!(ms.guide_stars().len(), 1);
        ms.set_multi_star_mode(false);
        ms.auto_select(&img, None, 0, false, 1.0, &fp);
        assert_eq!(ms.guide_stars().len(), 1);

        // the edge allowance keeps the largest uncalibrated distance
        assert_eq!(autoselect_edge_allowance(None, None), 0);
        assert_eq!(autoselect_edge_allowance(Some(25), Some(40)), 40);
        ms.take_log();
        assert!(ms.auto_select(&img, None, 45, false, 1.0, &fp).star.is_none());
        assert_eq!(ms.take_log(), ["GuiderMultiStar::AutoSelect enter", "GuiderMultiStar::AutoSelect failed."]);
        assert!(ms.auto_select(&UsImage::default(), None, 0, false, 1.0, &fp).star.is_none());
        assert_eq!(ms.take_log(), ["GuiderMultiStar::AutoSelect enter"]);
    }

    /// The final centroid find's saturation level: the camera's when
    /// saturation is by ADU (a level AutoFind keeps), 0 (the flat-top test)
    /// when it is not.
    #[test]
    fn auto_select_saturation_level() {
        // a lone star peaking near 7000 ADU over a 1000 ADU sky
        let img = render(160, 120, &[G { x: 70.3, y: 60.6, amp: 6000.0, sigma: 1.6 }]);
        let mut ms = MultiStar::new(MultiStarConfig::default());
        // a preset 5000: AutoFind keeps it and, the star being over it,
        // takes it on its last pass; the find calls it saturated
        let preset = FindParams { mode: FindMode::Centroid, saturation_by_adu: true, saturation_adu: 5000 };
        let r = ms.auto_select(&img, None, 0, false, 1.0, &preset);
        assert_eq!(r.saturation_adu_set, None);
        assert_eq!(r.star.unwrap().result, FindResult::Saturated);
        // the same level unused: the find runs the flat-top test, which a
        // Gaussian peak passes
        let off = FindParams { saturation_by_adu: false, ..preset };
        let r = ms.auto_select(&img, None, 0, false, 1.0, &off);
        assert_eq!(r.saturation_adu_set, None);
        assert_eq!(r.star.unwrap().result, FindResult::Ok);
    }

    #[test]
    fn multi_star_mode_and_lock_moves() {
        let mut ms = MultiStar::new(MultiStarConfig::default());
        ms.lock_position_set(); // single star: no effect
        assert!(!ms.stabilizing && !ms.lock_position_moved);
        ms.primary_dist_stats.add_value(1.0);
        assert!(ms.set_multi_star_mode(true));
        assert_eq!(ms.primary_dist_stats.count(), 0);
        assert!(!ms.set_multi_star_mode(true));
        ms.lock_position_set();
        assert!(ms.stabilizing && ms.lock_position_moved);
        assert!(!ms.set_multi_star_mode(false));
        assert!(!ms.stabilizing);
        ms.guide_stars = vec![GuideStar::default(); 4];
        ms.clear_secondary_stars();
        assert_eq!(ms.guide_stars().len(), 1);
        // a hand-picked star: an empty list gets the primary
        ms.guide_stars.clear();
        ms.primary.set_xy(30.0, 40.0);
        ms.keep_primary_only();
        assert_eq!(ms.guide_stars()[0].reference_point, Point::new(30.0, 40.0));
        ms.keep_primary_only();
        assert_eq!(ms.guide_stars().len(), 1);
        // only an actual clear and a multi-star lock move are logged
        ms.clear_secondary_stars();
        assert_eq!(
            ms.take_log(),
            ["MultiStar: stabilizing after lock position change", "MultiStar: secondary guide stars cleared"]
        );
    }

    // ------------------------------------------------- goldens from PHD2
    //
    // From PHD2's C++: guider_multistar.cpp's MassChecker, DistanceChecker,
    // GetBoundingBox, IsValidLockPosition, SetCurrentPosition,
    // UpdateCurrentPosition and RefineOffset compiled verbatim with
    // star.cpp's Find and guiding_stats.cpp, wx stubbed, on the frames and
    // the scripted finder above. The logs are their debug lines, plus
    // mount.cpp's CameraToMount lines, without Star::Find's.

    #[rustfmt::skip]
    const MASS: &[(i64, i32, bool, f64, Option<[f64; 4]>)] = &[
        (2000, 2000, false, 1000.0, None),
        (4000, 2000, false, 1020.0, None),
        (6000, 2000, false, 980.0, None),
        (8000, 2000, false, 1010.0, None),
        (10000, 2000, false, 990.0, None),
        (12000, 2000, false, 1005.0, None),
        (14000, 2000, false, 3000.0, Some([500.125, 1005.0, 1507.5, 2010.0])),
        (16000, 2000, false, 1015.0, None),
        (18000, 2000, false, 400.0, Some([500.4815625, 1010.0, 1515.0, 2020.0])),
        (20000, 2000, false, 1000.0, None),
        (22000, 2000, false, 1000.0, None),
        (24000, 2000, false, 1030.0, None),
        (26000, 2000, false, 1060.0, None),
        (28000, 2000, false, 1090.0, None),
        (30000, 2000, false, 1120.0, None),
        (32000, 2000, false, 1150.0, None),
        (34000, 2000, false, 1180.0, None),
        (36000, 2000, false, 1210.0, None),
        (38000, 2000, false, 1240.0, None),
        (40000, 2000, false, 1270.0, None),
        (42000, 2000, false, 1300.0, None),
        (44000, 2000, false, 1330.0, None),
        (46000, 2000, false, 1359.9999999999998, None),
        (48000, 2000, false, 1390.0000000000002, None),
        (50000, 2000, false, 1420.0, None),
        (52000, 2000, false, 1450.0, None),
        (54000, 2000, false, 1480.0, None),
        (56000, 2000, false, 1510.0, None),
        (58000, 2000, false, 1540.0, None),
        (60000, 2000, false, 1569.9999999999998, None),
        (110000, 2000, false, 1500.0, None),
        (112000, 2000, false, 1500.0, None),
        (114000, 1500, true, 900.0, None),
        (116000, 1500, true, 900.0, None),
        (118000, 1500, true, 910.0, None),
        (120000, 1500, true, 920.0, None),
        (122000, 1500, true, 930.0, None),
        (124000, 1500, true, 940.0, None),
        (126000, 1500, true, 950.0, None),
        (128000, 3000, true, 1800.0, None),
        (130000, 3000, true, 5000.0, Some([911.42624999999987, 1839.9999999999998, 2760.0, 3679.9999999999995])),
        (132000, 1000, false, 600.0, None),
    ];
    #[rustfmt::skip]
    const DIST: &[(char, f64, bool, bool, bool, u32, f64, i64, bool, u8)] = &[
        ('C', 2.0, true, false, false, 20, 4.0, 0, true, 0),
        ('C', 10.0, true, false, false, 20, 4.0, 1000, false, 1),
        ('C', 10.0, true, false, false, 20, 4.0, 3000, false, 1),
        ('C', 10.0, true, false, false, 20, 4.0, 6000, true, 2),
        ('C', 10.0, true, false, false, 20, 4.0, 7000, true, 2),
        ('C', 1.0, true, false, false, 20, 4.0, 8000, true, 0),
        ('A', 0.0, true, false, false, 20, 4.0, 9000, true, 1),
        ('C', 3.0, true, false, false, 20, 4.0, 9500, false, 1),
        ('C', 1.5, true, false, false, 20, 4.0, 10000, true, 0),
        ('A', 0.0, true, false, false, 20, 4.0, 11000, true, 1),
        ('C', 3.0, true, false, false, 20, 4.0, 17000, true, 2),
        ('C', 1.0, true, false, false, 20, 4.0, 18000, true, 0),
        ('C', 3.0, true, false, false, 20, 4.0, 19000, false, 1),
        ('C', 3.0, true, true, false, 20, 4.0, 20000, true, 0),
        ('C', 30.0, true, false, true, 20, 4.0, 21000, true, 0),
        ('C', 30.0, true, false, false, 9, 4.0, 22000, true, 0),
        ('C', 30.0, false, false, false, 20, 4.0, 23000, true, 0),
        ('C', 30.0, true, false, false, 20, 8.9999999999999999e+99, 24000, true, 0),
        ('C', 30.0, true, false, false, 20, 4.0, 25000, false, 1),
        ('A', 0.0, true, false, false, 20, 4.0, 26000, true, 1),
        ('C', 30.0, true, false, false, 20, 4.0, 30999, true, 2),
        ('C', 30.0, true, false, false, 20, 4.0, 31000, true, 2),
    ];
    #[rustfmt::skip]
    const BOXES: &[(u8, f64, f64, bool, f64, f64, bool, (i32, i32, i32, i32))] = &[
        (2, 40.399999999999999, 50.5, true, 0.0, 0.0, false, (25, 36, 31, 31)),
        (3, 40.5, 50.490000000000002, true, 0.0, 0.0, false, (26, 35, 31, 31)),
        (6, 44.899999999999999, 50.0, true, 40.0, 50.0, false, (25, 35, 31, 31)),
        (6, 46.5, 50.0, true, 40.0, 50.0, false, (32, 35, 31, 31)),
        (6, 45.990000000000002, 50.0, true, 40.0, 50.0, false, (25, 35, 31, 31)),
        (6, 40.0, 56.0, true, 40.0, 50.0, false, (25, 41, 31, 31)),
        (2, 5.2000000000000002, 90.700000000000003, true, 0.0, 0.0, false, (0, 76, 21, 20)),
        (2, 40.0, 50.0, true, 0.0, 0.0, true, (0, 0, 0, 0)),
        (2, 40.0, 50.0, false, 0.0, 0.0, false, (0, 0, 0, 0)),
        (6, 40.0, 50.0, false, 40.0, 50.0, false, (0, 0, 0, 0)),
        (5, 40.0, 50.0, true, 40.0, 50.0, false, (0, 0, 0, 0)),
        (0, 40.0, 50.0, true, 40.0, 50.0, false, (0, 0, 0, 0)),
    ];
    #[rustfmt::skip]
    const LOCKS: &[(f64, f64, bool)] = &[
        (16.0, 16.0, true),
        (15.9, 50.0, false),
        (111.90000000000001, 50.0, true),
        (112.0, 50.0, false),
        (50.0, 79.900000000000006, true),
        (50.0, 80.0, false),
        (50.0, 15.999000000000001, false),
    ];
    #[rustfmt::skip]
    const FRAMES: &[Frame] = &[
        // SetCurrentPosition error=0 at (40.309405180182232, 50.592021395217593)
        // secondary 1 found=1 (90.694361546860335, 30.197975108389667) snr 117.9320297730717
        // secondary 2 found=1 (100.11496060491679, 75.408806715459747) snr 88.783482697502421
        // secondary 3 found=1 (20.397074292351501, 20.811296696306965) snr 74.387646882557689
        // secondary 4 found=1 (64.209636740321315, 80.288736222010868) snr 98.79011558138852
        Frame { f: 1, err: false, info: None, primary: (40.583329130655414, 50.543549573221618, 48077.63013698638, 147.74650161628145, 3.3045023353112399, 0, true), cam: Some((0.27392395047318274, -0.048471821995974551)), mount: Some((0.24736514227896469, -0.11473482031655224)), ucd: Some((0.27817952507408611, 0.24736514227896469)), adjust: 147.74650161628145, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=48078 SNR=147.7", msg: "", log: &["CameraToMount -- cameraTheta (-0.18) - m_xAngle (0.30) = xAngle (-0.48 = -0.48)", "CameraToMount -- cameraTheta (-0.18) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.43 = -0.43)", "CameraToMount -- cameraX=0.27 cameraY=-0.05 hyp=0.28 cameraTheta=-0.18 mountX=0.25 mountY=-0.11, mountTheta=-0.43", ] },
        Frame { f: 2, err: false, info: None, primary: (40.644129545484333, 50.399832291453549, 48073.452054794463, 147.21599176926148, 3.4535146534784551, 0, true), cam: Some((0.33472436530210103, -0.19218910376404352)), mount: Some((0.26297863631009821, -0.26902654318911484)), ucd: Some((0.38597545560892943, 0.26297863631009821)), adjust: 147.21599176926148, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=48073 SNR=147.2", msg: "", log: &["CameraToMount -- cameraTheta (-0.52) - m_xAngle (0.30) = xAngle (-0.82 = -0.82)", "CameraToMount -- cameraTheta (-0.52) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.77 = -0.77)", "CameraToMount -- cameraX=0.33 cameraY=-0.19 hyp=0.39 cameraTheta=-0.52 mountX=0.26 mountY=-0.27, mountTheta=-0.80", ] },
        Frame { f: 3, err: false, info: None, primary: (40.456599808566637, 50.225725922818782, 48022.71917808213, 147.35443069948099, 3.6683375204259177, 0, true), cam: Some((0.14719462838440478, -0.36629547239881077)), mount: Some((0.032372685796495944, -0.39132476706687636)), ucd: Some((0.39476402030211794, 0.032372685796495944)), adjust: 147.35443069948099, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=48023 SNR=147.4", msg: "", log: &["CameraToMount -- cameraTheta (-1.19) - m_xAngle (0.30) = xAngle (-1.49 = -1.49)", "CameraToMount -- cameraTheta (-1.19) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.44 = -1.44)", "CameraToMount -- cameraX=0.15 cameraY=-0.37 hyp=0.39 cameraTheta=-1.19 mountX=0.03 mountY=-0.39, mountTheta=-1.49", ] },
        Frame { f: 4, err: false, info: None, primary: (40.149596880434586, 50.074536283760636, 47949.250000000044, 147.42966916916416, 3.876388760401523, 0, true), cam: Some((-0.15980829974764532, -0.5174851114569563)), mount: Some((-0.30559800709597401, -0.46186054646166141)), ucd: Some((0.54159905211129367, 0.30559800709597401)), adjust: 147.42966916916416, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=47949 SNR=147.4", msg: "", log: &["CameraToMount -- cameraTheta (-1.87) - m_xAngle (0.30) = xAngle (-2.17 = -2.17)", "CameraToMount -- cameraTheta (-1.87) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-2.12 = -2.12)", "CameraToMount -- cameraX=-0.16 cameraY=-0.52 hyp=0.54 cameraTheta=-1.87 mountX=-0.31 mountY=-0.46, mountTheta=-2.16", ] },
        Frame { f: 5, err: false, info: None, primary: (39.965553929796542, 50.00362558964563, 47876.010273972657, 147.49559514513234, 3.9948378671706455, 0, true), cam: Some((-0.34385125038568987, -0.58839580557196314)), mount: Some((-0.50237649638620641, -0.48503384416107853)), ucd: Some((0.68150077505934048, 0.50237649638620641)), adjust: 147.49559514513234, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=47876 SNR=147.5", msg: "", log: &["CameraToMount -- cameraTheta (-2.10) - m_xAngle (0.30) = xAngle (-2.40 = -2.40)", "CameraToMount -- cameraTheta (-2.10) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-2.35 = -2.35)", "CameraToMount -- cameraX=-0.34 cameraY=-0.59 hyp=0.68 cameraTheta=-2.10 mountX=-0.50 mountY=-0.49, mountTheta=-2.37", ] },
        Frame { f: 6, err: false, info: None, primary: (40.036794517532257, 50.029940932528426, 47913.63013698635, 147.46420854025968, 3.9532296499310515, 0, true), cam: Some((-0.27261066264997424, -0.56208046268916689)), mount: Some((-0.42654104784843527, -0.47716178502579765)), ucd: Some((0.62470074429868094, 0.42654104784843527)), adjust: 147.46420854025968, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=47914 SNR=147.5", msg: "", log: &["CameraToMount -- cameraTheta (-2.02) - m_xAngle (0.30) = xAngle (-2.32 = -2.32)", "CameraToMount -- cameraTheta (-2.02) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-2.27 = -2.27)", "CameraToMount -- cameraX=-0.27 cameraY=-0.56 hyp=0.62 cameraTheta=-2.02 mountX=-0.43 mountY=-0.48, mountTheta=-2.30", ] },
        Frame { f: 7, err: false, info: None, primary: (40.311737252926441, 50.145108533431504, 47981.869863013751, 147.39029460098257, 3.7636781817077796, 0, true), cam: Some((0.0023320727442097677, -0.44691286178608891)), mount: Some((-0.12984386708679685, -0.43359638723698118)), ucd: Some((0.44691894633492113, 0.12984386708679685)), adjust: 147.39029460098257, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=47982 SNR=147.4", msg: "", log: &["CameraToMount -- cameraTheta (-1.57) - m_xAngle (0.30) = xAngle (-1.87 = -1.87)", "CameraToMount -- cameraTheta (-1.57) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.82 = -1.82)", "CameraToMount -- cameraX=0.00 cameraY=-0.45 hyp=0.45 cameraTheta=-1.57 mountX=-0.13 mountY=-0.43, mountTheta=-1.86", ] },
        Frame { f: 8, err: false, info: None, primary: (40.583266939086826, 50.31781831012772, 48043.065068493088, 147.27056361940336, 3.4716808870966815, 0, true), cam: Some((0.27386175890459441, -0.2742030850898729)), mount: Some((0.18059757888474232, -0.33343325865636297)), ucd: Some((0.38754044287418865, 0.18059757888474232)), adjust: 147.27056361940336, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=48043 SNR=147.3", msg: "", log: &["CameraToMount -- cameraTheta (-0.79) - m_xAngle (0.30) = xAngle (-1.09 = -1.09)", "CameraToMount -- cameraTheta (-0.79) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.04 = -1.04)", "CameraToMount -- cameraX=0.27 cameraY=-0.27 hyp=0.39 cameraTheta=-0.79 mountX=0.18 mountY=-0.33, mountTheta=-1.07", ] },
        Frame { f: 9, err: false, info: None, primary: (40.646214142118652, 50.486102314210108, 48041.037671232814, 147.26121287620083, 3.4339375135048669, 0, true), cam: Some((0.33680896193641985, -0.10591908100748526)), mount: Some((0.29046466249366798, -0.18595418397980468)), ucd: Some((0.35307099648959972, 0.29046466249366798)), adjust: 147.26121287620083, reset: false, stab: true, used: 1, list: 5, dc: 0,
            status: "m=48041 SNR=147.3", msg: "", log: &["CameraToMount -- cameraTheta (-0.30) - m_xAngle (0.30) = xAngle (-0.60 = -0.60)", "CameraToMount -- cameraTheta (-0.30) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.55 = -0.55)", "CameraToMount -- cameraX=0.34 cameraY=-0.11 hyp=0.35 cameraTheta=-0.30 mountX=0.29 mountY=-0.19, mountTheta=-0.57", ] },
        Frame { f: 10, err: false, info: None, primary: (40.44879642081947, 50.584334746778609, 48195.006849315156, 147.46475929047398, 3.3199528907396649, 0, true), cam: Some((0.13939124063723796, -0.0076866484389839229)), mount: Some((0.13089397851001983, -0.041933633972906871)), ucd: Some((0.1396030176271735, 0.13089397851001983)), adjust: 147.46475929047398, reset: false, stab: false, used: 1, list: 5, dc: 0,
            status: "m=48195 SNR=147.5", msg: "", log: &["MultiStar: exiting stabilization period", "MultiStar: updating star positions after lock position change", "CameraToMount -- cameraTheta (-0.06) - m_xAngle (0.30) = xAngle (-0.36 = -0.36)", "CameraToMount -- cameraTheta (-0.06) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.31 = -0.31)", "CameraToMount -- cameraX=0.14 cameraY=-0.01 hyp=0.14 cameraTheta=-0.06 mountX=0.13 mountY=-0.04, mountTheta=-0.31", ] },
        Frame { f: 11, err: false, info: None, primary: (40.1449160230149, 50.581795375324447, 48144.510273972672, 147.4558695620585, 3.666704480053157, 0, true), cam: Some((-0.16448915716733126, -0.010226019893146088)), mount: Some((-0.16016448941961375, 0.030787151038507824)), ucd: Some((0.16480671803198443, 0.16016448941961375)), adjust: 147.4558695620585, reset: false, stab: false, used: 5, list: 4, dc: 0,
            status: "m=48145 SNR=147.5", msg: "", log: &["MultiStar: [#1 -0.31,-0.01,0.80,U] [#2 -0.30,-0.00,0.60,U] [#3 -0.28,-0.01,0.50,U] [#4 0.00,0.00,0.00,DZ] ", "single-star, 3 included, MultiStar: {-0.25, -0.01}, one-star: {-0.16, -0.01}", "CameraToMount -- cameraTheta (-3.08) - m_xAngle (0.30) = xAngle (-3.38 = 2.90)", "CameraToMount -- cameraTheta (-3.08) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-3.33 = 2.95)", "CameraToMount -- cameraX=-0.16 cameraY=-0.01 hyp=0.16 cameraTheta=-3.08 mountX=-0.16 mountY=0.03, mountTheta=2.95", ] },
        Frame { f: 12, err: true, info: Some((7, 144517.61643835602, 263.20197176516785, 3.6331288622332973, "Star lost - mass changed")), primary: (40.1449160230149, 50.581795375324447, 48144.510273972672, 147.4558695620585, 3.666704480053157, 7, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 5, list: 4, dc: 1,
            status: "Star lost - mass changed", msg: "Mass: 144518 vs 48041", log: &["UpdateCurrentPosition: star mass new=144517.6 exp=48041.0 thresh=50% limits=(23995.6, 72061.6, 96082.1)", "DistanceChecker: activated", ] },
        Frame { f: 13, err: false, info: None, primary: (41.455767422586128, 49.792499678484162, 47932.191780821973, 147.48416780345471, 3.7019943798894865, 0, true), cam: Some((-1.8536377575961041, 1.2004782832665697)), mount: Some((-1.4160821970891371, 1.6217556409038012)), ucd: Some((2.2084204864519266, 1.4160821970891371)), adjust: 147.48416780345471, reset: false, stab: true, used: 1, list: 4, dc: 0,
            status: "m=47932 SNR=147.5", msg: "", log: &["MultiStar: stabilizing after lock position change", "DistanceChecker: deactivated", "CameraToMount -- cameraTheta (2.57) - m_xAngle (0.30) = xAngle (2.27 = 2.27)", "CameraToMount -- cameraTheta (2.57) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.32 = 2.32)", "CameraToMount -- cameraX=-1.85 cameraY=1.20 hyp=2.21 cameraTheta=2.57 mountX=-1.42 mountY=1.62, mountTheta=2.29", ] },
        Frame { f: 14, err: false, info: None, primary: (42.187462857032585, 49.276516353269066, 47916.250000000058, 147.15016153399532, 3.6708714390330228, 0, true), cam: Some((-1.1219423231496464, 0.68449495805147365)), mount: Some((-0.86955034853719271, 0.94078864025681963)), ucd: Some((1.3142632628481685, 0.86955034853719271)), adjust: 147.15016153399532, reset: false, stab: true, used: 1, list: 4, dc: 0,
            status: "m=47916 SNR=147.2", msg: "", log: &["CameraToMount -- cameraTheta (2.59) - m_xAngle (0.30) = xAngle (2.29 = 2.29)", "CameraToMount -- cameraTheta (2.59) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.34 = 2.34)", "CameraToMount -- cameraX=-1.12 cameraY=0.68 hyp=1.31 cameraTheta=2.59 mountX=-0.87 mountY=0.94, mountTheta=2.32", ] },
        Frame { f: 15, err: false, info: None, primary: (42.683264176645025, 49.000390267242011, 47956.250000000029, 147.16705538801094, 3.9154715565556004, 0, true), cam: Some((-0.62614100353720659, 0.40836887202441829)), mount: Some((-0.47749409456210451, 0.55058343607112858)), ucd: Some((0.74754109716394579, 0.47749409456210451)), adjust: 147.16705538801094, reset: false, stab: true, used: 1, list: 4, dc: 0,
            status: "m=47956 SNR=147.2", msg: "", log: &["CameraToMount -- cameraTheta (2.56) - m_xAngle (0.30) = xAngle (2.26 = 2.26)", "CameraToMount -- cameraTheta (2.56) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.31 = 2.31)", "CameraToMount -- cameraX=-0.63 cameraY=0.41 hyp=0.75 cameraTheta=2.56 mountX=-0.48 mountY=0.55, mountTheta=2.29", ] },
        Frame { f: 16, err: false, info: None, primary: (42.958583677410104, 48.884121972431203, 47903.818493150713, 147.27431552703001, 3.9321815182282109, 0, true), cam: Some((-0.35082150277212776, 0.29210057721360982)), mount: Some((-0.24883095982403119, 0.36981450642856184)), ucd: Some((0.45650681705295276, 0.24883095982403119)), adjust: 147.27431552703001, reset: false, stab: true, used: 1, list: 4, dc: 0,
            status: "m=47904 SNR=147.3", msg: "", log: &["CameraToMount -- cameraTheta (2.45) - m_xAngle (0.30) = xAngle (2.15 = 2.15)", "CameraToMount -- cameraTheta (2.45) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.20 = 2.20)", "CameraToMount -- cameraX=-0.35 cameraY=0.29 hyp=0.46 cameraTheta=2.45 mountX=-0.25 mountY=0.37, mountTheta=2.16", ] },
        Frame { f: 17, err: false, info: None, primary: (43.058766856934085, 48.812297540727798, 47898.818493150713, 147.26590344216862, 3.9074120619326695, 0, true), cam: Some((-0.25063832324814683, 0.22027614551020491)), mount: Some((-0.17434788372847437, 0.27543720710388547)), ucd: Some((0.33367821229663092, 0.17434788372847437)), adjust: 147.26590344216862, reset: false, stab: true, used: 1, list: 4, dc: 0,
            status: "m=47899 SNR=147.3", msg: "", log: &["CameraToMount -- cameraTheta (2.42) - m_xAngle (0.30) = xAngle (2.12 = 2.12)", "CameraToMount -- cameraTheta (2.42) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.17 = 2.17)", "CameraToMount -- cameraX=-0.25 cameraY=0.22 hyp=0.33 cameraTheta=2.42 mountX=-0.17 mountY=0.28, mountTheta=2.14", ] },
        Frame { f: 18, err: false, info: None, primary: (43.107401045585242, 48.725989987810713, 47858.102739726048, 147.29547169127395, 3.826475675237905, 0, true), cam: Some((-0.20200413459699007, 0.13396859259312066)), mount: Some((-0.15339149456549708, 0.17978045616764618)), ucd: Some((0.24239070567094859, 0.15339149456549708)), adjust: 147.29547169127395, reset: false, stab: false, used: 1, list: 4, dc: 0,
            status: "m=47858 SNR=147.3", msg: "", log: &["MultiStar: exiting stabilization period", "MultiStar: updating star positions after lock position change", "CameraToMount -- cameraTheta (2.56) - m_xAngle (0.30) = xAngle (2.26 = 2.26)", "CameraToMount -- cameraTheta (2.56) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.31 = 2.31)", "CameraToMount -- cameraX=-0.20 cameraY=0.13 hyp=0.24 cameraTheta=2.56 mountX=-0.15 mountY=0.18, mountTheta=2.28", ] },
        Frame { f: 19, err: false, info: None, primary: (43.19063141723823, 48.636750626524986, 47853.102739726048, 147.28705902108851, 3.6549620165090517, 0, true), cam: Some((0.014706926395224511, -0.046285529610287791)), mount: Some((0.00037175415238068919, -0.048485176403507094)), ucd: Some((0.048565872125386357, 0.00037175415238068919)), adjust: 147.28705902108851, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47853 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.08,-0.09,0.80,U] [#2 0.07,-0.09,0.60,U] [#3 0.10,-0.10,0.50,U] ", "refined, 3 included, MultiStar: {0.01, -0.05}, one-star: {-0.12, 0.04}", "CameraToMount -- cameraTheta (-1.26) - m_xAngle (0.30) = xAngle (-1.56 = -1.56)", "CameraToMount -- cameraTheta (-1.26) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.51 = -1.51)", "CameraToMount -- cameraX=0.01 cameraY=-0.05 hyp=0.05 cameraTheta=-1.26 mountX=0.00 mountY=-0.05, mountTheta=-1.56", ] },
        Frame { f: 20, err: false, info: None, primary: (43.277253865250557, 48.589759743162617, 47848.62328767126, 147.26826550628525, 3.5443648235348082, 0, true), cam: Some((-0.032151314931674335, -0.0022616520549760821)), mount: Some((-0.031383688210280009, 0.0057630198396815633)), ucd: Some((0.032230763594018101, 0.031383688210280009)), adjust: 147.26826550628525, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47849 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.18,-0.15,0.80,U] [#2 0.17,-0.13,0.60,U] [#3 0.19,-0.15,0.50,U] ", "single-star, 3 included, MultiStar: {0.11, -0.09}, one-star: {-0.03, -0.00}", "CameraToMount -- cameraTheta (-3.07) - m_xAngle (0.30) = xAngle (-3.37 = 2.91)", "CameraToMount -- cameraTheta (-3.07) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-3.32 = 2.96)", "CameraToMount -- cameraX=-0.03 cameraY=-0.00 hyp=0.03 cameraTheta=-3.07 mountX=-0.03 mountY=0.01, mountTheta=2.96", ] },
        Frame { f: 21, err: false, info: None, primary: (43.299083028628225, 48.603365501965662, 47843.62328767126, 147.2598515282063, 3.5021894972519316, 0, true), cam: Some((-0.010322151554007064, 0.011344106748069294)), mount: Some((-0.0065087152552497978, 0.013545187103902458)), ucd: Some((0.015337391258472926, 0.0065087152552497978)), adjust: 147.2598515282063, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47844 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.20,-0.13,0.80,U] [#2 0.20,-0.11,0.60,U] [#3 0.21,-0.14,0.50,U] ", "single-star, 3 included, MultiStar: {0.13, -0.08}, one-star: {-0.01, 0.01}", "CameraToMount -- cameraTheta (2.31) - m_xAngle (0.30) = xAngle (2.01 = 2.01)", "CameraToMount -- cameraTheta (2.31) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.06 = 2.06)", "CameraToMount -- cameraX=-0.01 cameraY=0.01 hyp=0.02 cameraTheta=2.31 mountX=-0.01 mountY=0.01, mountTheta=2.02", ] },
        Frame { f: 22, err: false, info: None, primary: (43.256265839963262, 48.644660593893406, 47845.863013698552, 147.26907956240959, 3.6141584603374479, 0, true), cam: Some((-0.053139340218969267, 0.052639198675812793)), mount: Some((-0.035210003848077723, 0.06414965662823563)), ucd: Some((0.074797558223106825, 0.035210003848077723)), adjust: 147.26907956240959, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47846 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.15,-0.08,0.80,U] [#2 0.15,-0.08,0.60,U] [#3 0.17,-0.10,0.50,U] ", "single-star, 3 included, MultiStar: {0.09, -0.04}, one-star: {-0.05, 0.05}", "CameraToMount -- cameraTheta (2.36) - m_xAngle (0.30) = xAngle (2.06 = 2.06)", "CameraToMount -- cameraTheta (2.36) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.11 = 2.11)", "CameraToMount -- cameraX=-0.05 cameraY=0.05 hyp=0.07 cameraTheta=2.36 mountX=-0.04 mountY=0.06, mountTheta=2.07", ] },
        Frame { f: 23, err: false, info: None, primary: (43.231974670330942, 48.65852472829269, 47846.863013698552, 147.27076231249563, 3.6436196434750854, 0, true), cam: Some((0.059360455806203881, -0.023009677132058703)), mount: Some((0.049909384901517827, -0.036980373782411022)), ucd: Some((0.063664031880190167, 0.049909384901517827)), adjust: 147.27076231249563, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47847 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.13,-0.07,0.80,U] [#2 0.12,-0.06,0.60,U] [#3 0.15,-0.09,0.50,U] ", "refined, 3 included, MultiStar: {0.06, -0.02}, one-star: {-0.08, 0.07}", "CameraToMount -- cameraTheta (-0.37) - m_xAngle (0.30) = xAngle (-0.67 = -0.67)", "CameraToMount -- cameraTheta (-0.37) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.62 = -0.62)", "CameraToMount -- cameraX=0.06 cameraY=-0.02 hyp=0.06 cameraTheta=-0.37 mountX=0.05 mountY=-0.04, mountTheta=-0.64", ] },
        Frame { f: 24, err: false, info: None, primary: (43.272616123968639, 48.623355138601482, 47844.62328767126, 147.2615343611572, 3.5616966687006064, 0, true), cam: Some((-0.03678905621359263, 0.031333743383889612)), mount: Some((-0.025886173481057706, 0.039461411347824403)), ucd: Some((0.048324301666287164, 0.025886173481057706)), adjust: 147.2615343611572, reset: false, stab: false, used: 4, list: 4, dc: 0,
            status: "m=47845 SNR=147.3", msg: "", log: &["MultiStar: [#1 0.17,-0.11,0.80,U] [#2 0.17,-0.10,0.60,U] [#3 0.19,-0.12,0.50,U] ", "single-star, 3 included, MultiStar: {0.10, -0.06}, one-star: {-0.04, 0.03}", "CameraToMount -- cameraTheta (2.44) - m_xAngle (0.30) = xAngle (2.14 = 2.14)", "CameraToMount -- cameraTheta (2.44) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (2.19 = 2.19)", "CameraToMount -- cameraX=-0.04 cameraY=0.03 hyp=0.05 cameraTheta=2.44 mountX=-0.03 mountY=0.04, mountTheta=2.15", ] },
    ];

    #[rustfmt::skip]
    const JUMPS: &[Frame] = &[
        // SetCurrentPosition at (50.304331554631482, 40.701173071587021)
        Frame { f: 1, err: false, info: None, primary: (50.38727701460224, 40.676448198605833, 47758.671232876775, 147.17216904152608, 3.4953571460997348, 0, true), cam: Some((0.082945459970758861, -0.024724872981188639)), mount: Some((0.071934124944297032, -0.044477271755644858)), ucd: Some((0.086552115362344972, 0.071934124944297032)), adjust: 147.17216904152608, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47759 SNR=147.2 Exp=1.5 s", msg: "", log: &["CameraToMount -- cameraTheta (-0.29) - m_xAngle (0.30) = xAngle (-0.59 = -0.59)", "CameraToMount -- cameraTheta (-0.29) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.54 = -0.54)", "CameraToMount -- cameraX=0.08 cameraY=-0.02 hyp=0.09 cameraTheta=-0.29 mountX=0.07 mountY=-0.04, mountTheta=-0.55", ] },
        Frame { f: 2, err: false, info: None, primary: (50.404734842725581, 40.620212888337832, 47750.93150684928, 147.15671588128373, 3.4195507242180758, 0, true), cam: Some((0.10040328809409971, -0.080960183249189299)), mount: Some((0.071993554659343623, -0.10328349821076233)), ucd: Some((0.12897818238698006, 0.071993554659343623)), adjust: 147.15671588128373, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47751 SNR=147.2 Exp=1.5 s", msg: "", log: &["CameraToMount -- cameraTheta (-0.68) - m_xAngle (0.30) = xAngle (-0.98 = -0.98)", "CameraToMount -- cameraTheta (-0.68) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.93 = -0.93)", "CameraToMount -- cameraX=0.10 cameraY=-0.08 hyp=0.13 cameraTheta=-0.68 mountX=0.07 mountY=-0.10, mountTheta=-0.96", ] },
        Frame { f: 3, err: false, info: None, primary: (50.350111644744651, 40.555082680573697, 47703.599315068452, 147.17407747530828, 3.4141733714568154, 0, true), cam: Some((0.045780090113169081, -0.14609039101332399)), mount: Some((0.00056272801707538133, -0.15287497009440165)), ucd: Some((0.15309545714094777, 0.00056272801707538133)), adjust: 147.17407747530828, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47704 SNR=147.2 Exp=1.5 s", msg: "", log: &["CameraToMount -- cameraTheta (-1.27) - m_xAngle (0.30) = xAngle (-1.57 = -1.57)", "CameraToMount -- cameraTheta (-1.27) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.52 = -1.52)", "CameraToMount -- cameraX=0.05 cameraY=-0.15 hyp=0.15 cameraTheta=-1.27 mountX=0.00 mountY=-0.15, mountTheta=-1.57", ] },
        Frame { f: 4, err: true, info: Some((8, 47768.999999999935, 146.81995094408197, 3.6179243995436097, "No star found")), primary: (50.350111644744651, 40.555082680573697, 47703.599315068452, 147.17407747530828, 3.4141733714568154, 8, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 1, list: 0, dc: 1,
            status: "No star found", msg: "Recovering", log: &["DistanceChecker: reject for large offset (5.02 > 2.40) avgDist = 0.60 count = 20", "DistanceChecker: activated", ] },
        Frame { f: 5, err: true, info: Some((8, 47737.06849315072, 146.86275024507219, 3.7230753822901748, "No star found")), primary: (50.350111644744651, 40.555082680573697, 47703.599315068452, 147.17407747530828, 3.4141733714568154, 8, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 1, list: 0, dc: 1,
            status: "No star found", msg: "Recovering", log: &["DistanceChecker: reject for large offset (4.97 > 2.40) avgDist = 0.60 count = 20", ] },
        Frame { f: 6, err: true, info: Some((8, 47811.431506849352, 146.79206833205311, 3.655498601910629, "No star found")), primary: (50.350111644744651, 40.555082680573697, 47703.599315068452, 147.17407747530828, 3.4141733714568154, 8, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 1, list: 0, dc: 1,
            status: "No star found", msg: "Recovering", log: &["DistanceChecker: reject for large offset (5.00 > 2.40) avgDist = 0.60 count = 20", ] },
        Frame { f: 7, err: false, info: None, primary: (55.305092225526728, 41.625791913950039, 47849.869863013628, 146.7537142661007, 3.5371145260349812, 0, true), cam: Some((5.0007606708952466, 0.92461884236301728)), mount: Some((5.050652693668563, -0.34133330755054403)), ucd: Some((5.0855213391770766, 5.050652693668563)), adjust: 146.7537142661007, reset: false, stab: false, used: 1, list: 0, dc: 2,
            status: "m=47850 SNR=146.8 Exp=1.5 s", msg: "", log: &["DistanceChecker: reject for large offset (5.09 > 2.40) avgDist = 0.60 count = 20", "DistanceChecker: begin recovering", "CameraToMount -- cameraTheta (0.18) - m_xAngle (0.30) = xAngle (-0.12 = -0.12)", "CameraToMount -- cameraTheta (0.18) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.07 = -0.07)", "CameraToMount -- cameraX=5.00 cameraY=0.92 hyp=5.09 cameraTheta=0.18 mountX=5.05 mountY=-0.34, mountTheta=-0.07", ] },
        Frame { f: 8, err: false, info: None, primary: (55.38596317448939, 41.6823267580207, 47849.869863013635, 146.75514225607085, 3.5052884535528901, 0, true), cam: Some((5.0816316198579088, 0.98115368643367873)), mount: Some((5.1446188509261361, -0.30656378783303884)), ucd: Some((5.1754847576185661, 5.1446188509261361)), adjust: 146.75514225607085, reset: false, stab: false, used: 1, list: 0, dc: 2,
            status: "m=47850 SNR=146.8 Exp=1.5 s", msg: "", log: &["DistanceChecker: reject for large offset (5.18 > 2.40) avgDist = 0.60 count = 20", "CameraToMount -- cameraTheta (0.19) - m_xAngle (0.30) = xAngle (-0.11 = -0.11)", "CameraToMount -- cameraTheta (0.19) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.06 = -0.06)", "CameraToMount -- cameraX=5.08 cameraY=0.98 hyp=5.18 cameraTheta=0.19 mountX=5.14 mountY=-0.31, mountTheta=-0.06", ] },
        Frame { f: 9, err: false, info: None, primary: (50.404748784045644, 40.698898433566796, 47752.671232876775, 147.16206462563269, 3.4924798117539386, 0, true), cam: Some((0.10041722941416253, -0.0022746380202249838)), mount: Some((0.095260041898429945, -0.027047545167124841)), ucd: Some((0.1004429885125866, 0.095260041898429945)), adjust: 147.16206462563269, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47753 SNR=147.2 Exp=1.5 s", msg: "", log: &["DistanceChecker: deactivated", "CameraToMount -- cameraTheta (-0.02) - m_xAngle (0.30) = xAngle (-0.32 = -0.32)", "CameraToMount -- cameraTheta (-0.02) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-0.27 = -0.27)", "CameraToMount -- cameraX=0.10 cameraY=-0.00 hyp=0.10 cameraTheta=-0.02 mountX=0.10 mountY=-0.03, mountTheta=-0.28", ] },
        Frame { f: 10, err: false, info: None, primary: (50.335330160896333, 45.671125907799656, 47840.726027397315, 146.73070463649398, 3.5901511784944495, 0, true), cam: Some((0.030998606264851958, 4.9699528362126344)), mount: Some((1.4983355889315189, 4.8077798604011743)), ucd: Some((0.030998606264851958, 1.4983355889315189)), adjust: 146.73070463649398, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47841 SNR=146.7 Exp=1.5 s", msg: "", log: &["CameraToMount -- cameraTheta (1.56) - m_xAngle (0.30) = xAngle (1.26 = 1.26)", "CameraToMount -- cameraTheta (1.56) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (1.31 = 1.31)", "CameraToMount -- cameraX=0.03 cameraY=4.97 hyp=4.97 cameraTheta=1.56 mountX=1.50 mountY=4.81, mountTheta=1.27", ] },
        Frame { f: 11, err: true, info: Some((7, 144187.31506849328, 263.10208487006201, 3.5950648155149816, "Star lost - mass changed")), primary: (50.335330160896333, 45.671125907799656, 47840.726027397315, 146.73070463649398, 3.5901511784944495, 7, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 1, list: 0, dc: 1,
            status: "Star lost - mass changed", msg: "Mass: 144187 vs 47759", log: &["UpdateCurrentPosition: star mass new=144187.3 exp=47758.7 thresh=50% limits=(23879.3, 71638.0, 95517.3)", "DistanceChecker: activated", ] },
        Frame { f: 12, err: false, info: None, primary: (50.210844433953177, 40.553269028601981, 47760.249999999956, 147.07383535463367, 3.6837921739210633, 0, true), cam: Some((-0.093487120678304336, -0.14790404298504001)), mount: Some((-0.13302029099625973, -0.12017698067431257)), ucd: Some((0.17497270548299926, 0.13302029099625973)), adjust: 147.07383535463367, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47760 SNR=147.1 Exp=1.5 s", msg: "", log: &["DistanceChecker: deactivated", "CameraToMount -- cameraTheta (-2.13) - m_xAngle (0.30) = xAngle (-2.43 = -2.43)", "CameraToMount -- cameraTheta (-2.13) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-2.38 = -2.38)", "CameraToMount -- cameraX=-0.09 cameraY=-0.15 hyp=0.17 cameraTheta=-2.13 mountX=-0.13 mountY=-0.12, mountTheta=-2.41", ] },
        Frame { f: 13, err: true, info: Some((3, 0.0, 0.0, 0.0, "Star lost - low mass")), primary: (50.210844433953177, 40.553269028601981, 47760.249999999956, 147.07383535463367, 3.6837921739210633, 3, true), cam: None, mount: None, ucd: None, adjust: -1.0, reset: true, stab: false, used: 1, list: 0, dc: 1,
            status: "Star lost - low mass", msg: "", log: &["DistanceChecker: activated", ] },
        Frame { f: 14, err: false, info: None, primary: (50.310295297389715, 40.510398952776569, 47761.000000000065, 147.08060261240317, 3.5079370929586307, 0, true), cam: Some((0.0059637427582330815, -0.19077411881045236)), mount: Some((-0.050680225947801194, -0.18631886702671205)), ucd: Some((0.19086731159523093, 0.050680225947801194)), adjust: 147.08060261240317, reset: false, stab: false, used: 1, list: 0, dc: 0,
            status: "m=47761 SNR=147.1 Exp=1.5 s", msg: "", log: &["DistanceChecker: deactivated", "CameraToMount -- cameraTheta (-1.54) - m_xAngle (0.30) = xAngle (-1.84 = -1.84)", "CameraToMount -- cameraTheta (-1.54) - (m_xAngle (0.30) + m_yAngleError (-0.05)) = yAngle (-1.79 = -1.79)", "CameraToMount -- cameraX=0.01 cameraY=-0.19 hyp=0.19 cameraTheta=-1.54 mountX=-0.05 mountY=-0.19, mountTheta=-1.84", ] },
    ];

    #[rustfmt::skip]
    const SCRIPTED: &[Refine] = &[
        Refine { f: 0, refined: false, ofs: (0.0, 0.25), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 1, refined: false, ofs: (0.19326530617130544, 0.21939564047259097), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 2, refined: false, ofs: (0.29563491899654082, 0.13507557646703816), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 3, refined: false, ofs: (0.2589628099946637, 0.017684300416924259), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 4, refined: false, ofs: (0.10049644504677246, -0.10403670913678553), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 5, refined: false, ofs: (-0.10523496830688828, -0.20028590388672995), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 6, refined: false, ofs: (-0.26147273172407637, -0.24749812415011263), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 7, refined: false, ofs: (-0.29473578378730281, -0.23411417182269645), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 8, refined: false, ofs: (-0.18937999136169736, -0.16341090521590473), stab: true, moved: false, used: 1, req: &[], stars: None, log: &[] },
        Refine { f: 9, refined: false, ofs: (0.0050441701453038945, -0.052698949857692412), stab: false, moved: false, used: 9, req: &[(20, 20), (100, 20), (20, 80), (100, 80), (40, 30), (80, 30), (40, 75), (80, 75), (120, 50), ], stars: Some(&[(20.005044170145304, 19.947301050142304, 20.0, 20.0, 0, 0, false), (100.00504417014531, 20.0, 100.0, 20.0, 0, 1, false), (20.005044170145304, 79.9473010501423, 20.0, 80.0, 0, 0, false), (101.50504417014531, 81.4473010501423, 100.0, 80.0, 1, 0, false), (80.0, 30.0, 80.0, 30.0, 0, 0, true), (40.145044170145304, 74.877301050142307, 40.0, 75.0, 0, 0, false), (80.165044170145308, 74.867301050142302, 80.0, 75.0, 0, 0, false), (120.18504417014532, 49.857301050142304, 120.0, 50.0, 0, 0, false), ]), log: &["MultiStar: exiting stabilization period", "MultiStar: [#1 0.01,-0.05,0.40,U] [#2 0.01,0.00,0.48,U] [#3 0.01,-0.05,0.56,U] [#4 1.51,1.45,0.00,M1] [#5 0.00,0.00,0.00,DZ] [#5 0.00,0.00,0.00,L] [#6 0.15,-0.12,0.88,U] [#7 0.17,-0.13,0.96,U] [#8 0.19,-0.14,1.04,U] ", "single-star, 6 included, MultiStar: {0.09, -0.09}, one-star: {0.01, -0.05}", ] },
        Refine { f: 10, refined: true, ofs: (0.19709597961563791, 0.056964947080729654), stab: false, moved: false, used: 8, req: &[(20, 19), (100, 20), (20, 79), (101, 81), (80, 30), (40, 74), (80, 74), (120, 49), ], stars: None, log: &["MultiStar: [#1 0.20,0.07,0.40,U] [#2 0.20,0.00,0.48,U] [#3 0.20,0.07,0.56,U] [#4 1.70,1.57,0.00,M2] [#5 0.00,0.00,0.00,L] [#6 0.34,0.00,0.00,M1] [#7 0.36,-0.01,0.00,M1] [#8 0.38,-0.02,0.00,M1] ", "refined, 3 included, MultiStar: {0.20, 0.06}, one-star: {0.20, 0.07}", ] },
        Refine { f: 11, refined: false, ofs: (0.29645047016310144, 0.177167443572813), stab: false, moved: false, used: 8, req: &[(20, 20), (100, 20), (20, 80), (101, 81), (80, 30), (40, 75), (80, 74), (120, 49), ], stars: None, log: &["MultiStar: [#1 0.30,0.18,0.00,M1] [#2 0.30,0.00,0.00,M1] [#3 0.30,0.18,0.00,M1] [#4 1.80,1.68,0.00,M3] [#5 0.00,0.00,0.00,L] [#6 0.44,0.11,0.00,M2] [#7 0.46,0.10,0.00,M2] [#8 0.48,0.09,0.00,M2] ", ] },
        Refine { f: 12, refined: false, ofs: (2.7563796724264833, -1.2599574283374082), stab: false, moved: false, used: 9, req: &[(20, 20), (100, 20), (20, 80), (101, 81), (82, 28), (40, 75), (80, 75), (120, 50), ], stars: None, log: &["MultiStar: [#1 2.76,-1.26,0.00,M2] [#2 2.76,0.00,0.00,M2] [#3 2.76,-1.26,0.00,M2] [#4 4.26,0.24,0.00,M4] [#5 2.76,-1.26,0.00,M1] [#6 2.90,-1.33,0.00,M3] [#7 2.92,-1.34,0.00,M3] [#8 2.94,-1.35,0.00,M3] ", ] },
        Refine { f: 13, refined: false, ofs: (0.095729508704806676, 0.24414690643200743), stab: false, moved: false, used: 9, req: &[(22, 18), (102, 20), (22, 78), (20, 80), (104, 80), (82, 28), (42, 73), (82, 73), ], stars: Some(&[(20.095729508704807, 20.244146906432007, 20.0, 20.0, 1, 0, false), (20.095729508704807, 80.244146906432007, 20.0, 80.0, 0, 0, false), (101.59572950870481, 81.744146906432007, 100.0, 80.0, 5, 0, false), (80.095729508704807, 30.244146906432007, 80.0, 30.0, 0, 0, false), (40.235729508704807, 75.174146906432014, 40.0, 75.0, 2, 0, false), (80.255729508704803, 75.164146906432009, 80.0, 75.0, 2, 0, false), (122.9363796724265, 48.650042571662588, 120.0, 50.0, 3, 0, false), ]), log: &["MultiStar: [#1 0.10,0.24,0.40,U] [#2 0.00,0.00,0.00,DZ] [#2 0.10,0.24,0.56,U] [#2 0.10,0.24,0.56,U] [#3 1.60,1.74,0.00,M5] [#4 0.10,0.24,0.80,U] [#5 0.24,0.17,0.88,U] [#6 0.26,0.16,0.96,U] ", "single-star, 6 included, MultiStar: {0.15, 0.22}, one-star: {0.10, 0.24}", ] },
        Refine { f: 14, refined: true, ofs: (-0.027674235229480537, 0.14734081181277661), stab: false, moved: false, used: 8, req: &[(20, 20), (20, 80), (101, 81), (80, 30), (40, 75), (80, 75), (122, 48), ], stars: None, log: &["MultiStar: [#1 -0.11,0.19,0.40,U] [#2 -0.11,0.19,0.56,U] [#3 1.39,1.69,0.00,M6] [#4 -0.11,0.19,0.80,U] [#5 0.03,0.12,0.88,U] [#6 0.05,0.11,0.96,U] [#7 0.07,0.10,1.04,U] ", "refined, 6 included, MultiStar: {-0.03, 0.15}, one-star: {-0.11, 0.19}", ] },
        Refine { f: 15, refined: true, ofs: (-0.18163922444540301, 0.04552407768570689), stab: false, moved: false, used: 8, req: &[(19, 20), (19, 80), (101, 81), (79, 30), (40, 75), (80, 75), (120, 50), ], stars: None, log: &["MultiStar: [#1 -0.26,0.09,0.40,U] [#2 -0.26,0.09,0.56,U] [#3 1.24,1.59,0.00,M7] [#4 -0.26,0.09,0.80,U] [#5 -0.12,0.02,0.88,U] [#6 -0.10,0.01,0.96,U] [#7 -0.08,-0.00,1.04,U] ", "refined, 6 included, MultiStar: {-0.18, 0.05}, one-star: {-0.26, 0.09}", ] },
        Refine { f: 16, refined: true, ofs: (-0.2114838151992986, -0.077509760225203048), stab: false, moved: false, used: 8, req: &[(19, 20), (19, 80), (101, 81), (79, 30), (39, 75), (79, 75), (119, 49), ], stars: None, log: &["MultiStar: [#1 -0.29,-0.04,0.40,U] [#2 -0.29,-0.04,0.56,U] [#3 1.21,1.46,0.00,M8] [#4 -0.29,-0.04,0.80,U] [#5 -0.15,-0.11,0.88,U] [#6 -0.13,-0.12,0.96,U] [#7 -0.11,-0.13,1.04,U] ", "refined, 6 included, MultiStar: {-0.21, -0.08}, one-star: {-0.29, -0.04}", ] },
        Refine { f: 17, refined: true, ofs: (-0.1031716301250082, -0.19163772744425422), stab: false, moved: false, used: 8, req: &[(19, 19), (19, 79), (101, 81), (79, 29), (39, 74), (79, 74), (119, 49), ], stars: None, log: &["MultiStar: [#1 -0.19,-0.15,0.40,U] [#2 -0.19,-0.15,0.56,U] [#3 1.31,1.35,0.00,M9] [#4 -0.19,-0.15,0.80,U] [#5 -0.05,-0.22,0.88,U] [#6 -0.03,-0.23,0.96,U] [#7 -0.01,-0.24,1.04,U] ", "refined, 6 included, MultiStar: {-0.10, -0.19}, one-star: {-0.19, -0.15}", ] },
        Refine { f: 18, refined: false, ofs: (0.010086914166343774, -0.22778256547116627), stab: false, moved: false, used: 8, req: &[(19, 19), (19, 79), (101, 81), (79, 29), (39, 74), (79, 74), (119, 49), ], stars: None, log: &["MultiStar: [#1 0.01,-0.23,0.40,U] [#2 0.01,-0.23,0.56,U] [#3 1.51,1.27,0.00,M10] [#4 0.01,-0.23,0.80,U] [#5 0.15,-0.30,0.88,U] [#6 0.17,-0.31,0.96,U] [#7 0.19,-0.32,1.04,U] ", "single-star, 6 included, MultiStar: {0.09, -0.27}, one-star: {0.01, -0.23}", ] },
        Refine { f: 19, refined: false, ofs: (0.20087092865897915, -0.24929303904909261), stab: false, moved: false, used: 8, req: &[(20, 19), (20, 79), (101, 81), (80, 29), (40, 74), (80, 74), (120, 49), ], stars: Some(&[(20.200870928658979, 19.750706960950904, 20.0, 20.0, 0, 0, false), (20.200870928658979, 79.750706960950907, 20.0, 80.0, 0, 0, false), (101.70087092865899, 81.250706960950907, 101.70087092865899, 81.250706960950907, 0, 0, false), (80.200870928658986, 29.750706960950904, 80.0, 30.0, 0, 0, false), (40.34087092865898, 74.680706960950914, 40.0, 75.0, 0, 0, false), (80.360870928658983, 74.670706960950909, 80.0, 75.0, 0, 0, false), (120.38087092865899, 49.660706960950904, 120.0, 50.0, 0, 0, false), ]), log: &["MultiStar: [#1 0.20,-0.25,0.40,U] [#2 0.20,-0.25,0.56,U] [#3 1.70,1.25,0.00,R] [#4 0.20,-0.25,0.80,U] [#5 0.34,-0.32,0.88,U] [#6 0.36,-0.33,0.96,U] [#7 0.38,-0.34,1.04,U] ", "single-star, 6 included, MultiStar: {0.28, -0.29}, one-star: {0.20, -0.25}", ] },
        Refine { f: 20, refined: false, ofs: (-1.9009392644305123, 1.4160928470923579), stab: true, moved: true, used: 1, req: &[], stars: None, log: &["MultiStar: stabilizing after lock position change", ] },
        Refine { f: 21, refined: false, ofs: (-0.91542531688570961, 0.70244630720040391), stab: false, moved: false, used: 1, req: &[(23, 17), (23, 77), (103, 77), (83, 27), (43, 72), (83, 72), (120, 49), ], stars: Some(&[(23.084574683114294, 17.7024463072004, 23.084574683114294, 17.7024463072004, 0, 0, false), (23.084574683114294, 77.702446307200404, 23.084574683114294, 77.702446307200404, 0, 0, false), (104.5845746831143, 79.202446307200404, 104.5845746831143, 79.202446307200404, 0, 0, false), (83.084574683114297, 27.7024463072004, 83.084574683114297, 27.7024463072004, 0, 0, false), (43.224574683114291, 72.632446307200411, 43.224574683114291, 72.632446307200411, 0, 0, false), (83.244574683114294, 72.622446307200406, 83.244574683114294, 72.622446307200406, 0, 0, false), (123.2645746831143, 47.6124463072004, 123.2645746831143, 47.6124463072004, 0, 0, false), ]), log: &["MultiStar: exiting stabilization period", "MultiStar: updating star positions after lock position change", ] },
        Refine { f: 22, refined: true, ofs: (0.29996878999885407, -0.2151492298856085), stab: false, moved: false, used: 8, req: &[(23, 17), (23, 77), (104, 79), (83, 27), (43, 72), (83, 72), (123, 47), ], stars: None, log: &["MultiStar: [#1 0.45,-0.33,0.40,U] [#2 0.45,-0.33,0.56,U] [#3 0.45,-0.33,0.64,U] [#4 0.45,-0.33,0.80,U] [#5 0.45,-0.33,0.88,U] [#6 0.45,-0.33,0.96,U] [#7 0.45,-0.33,1.04,U] ", "refined, 7 included, MultiStar: {0.30, -0.22}, one-star: {-0.47, 0.38}", ] },
        Refine { f: 23, refined: false, ofs: (-0.28820714171840223, 0.23583047587530359), stab: false, moved: false, used: 8, req: &[(23, 17), (23, 77), (105, 78), (83, 27), (43, 72), (83, 72), (123, 47), ], stars: Some(&[(23.711792858281598, 17.2358304758753, 23.084574683114294, 17.7024463072004, 0, 0, false), (23.711792858281598, 77.235830475875304, 23.084574683114294, 77.702446307200404, 0, 0, false), (105.2117928582816, 78.735830475875304, 104.5845746831143, 79.202446307200404, 0, 0, false), (83.711792858281598, 27.2358304758753, 83.084574683114297, 27.7024463072004, 0, 0, false), (43.851792858281598, 72.16583047587531, 43.224574683114291, 72.632446307200411, 0, 0, false), (83.871792858281594, 72.155830475875305, 83.244574683114294, 72.622446307200406, 0, 0, false), (123.8917928582816, 47.1458304758753, 123.2645746831143, 47.6124463072004, 0, 0, false), ]), log: &["MultiStar: [#1 0.63,-0.47,0.40,U] [#2 0.63,-0.47,0.56,U] [#3 0.63,-0.47,0.64,U] [#4 0.63,-0.47,0.80,U] [#5 0.63,-0.47,0.88,U] [#6 0.63,-0.47,0.96,U] [#7 0.63,-0.47,1.04,U] ", "single-star, 7 included, MultiStar: {0.48, -0.35}, one-star: {-0.29, 0.24}", ] },
    ];
}
