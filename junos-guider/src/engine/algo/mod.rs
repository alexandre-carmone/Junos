//! The guide algorithms: PHD2's `GuideAlgorithm` family.
//!
//! One algorithm runs per mount axis. Each frame the guider hands it the
//! star's offset along that axis (px, already in mount coordinates) and gets
//! back the move to make (px); the mount turns that into a pulse. The
//! lifecycle hooks let an algorithm forget its history when guiding stops,
//! resumes, dithers or is re-enabled.
//!
//! This module holds the shared vocabulary ([`AlgoKind`], [`Axis`],
//! [`MountClass`], [`AlgoCtx`]), the [`GuideAlgorithm`] trait, the factory
//! ([`make`], [`make_or_default`]) with the per-axis allowed lists, the
//! defaults, and the min-move heuristic. The algorithms themselves live one
//! per file; the Gaussian process in [`gp`].
//!
//! Parameters: [`make`] reads the profile through a lookup on full PHD2
//! paths (`/<mountClass>/GuideAlgorithm/<X|Y>/<ClassName>/<key>`) and hands
//! each algorithm a [`ParamLookup`] on keys relative to its own group. The
//! algorithm validates them like PHD2's setters and exposes
//! [`GuideAlgorithm::params`] so the profile layer can write the effective
//! values back under [`config_path`] — PHD2 persists each setter's result at
//! once; here the write-back is the caller's.
//!
//! Ported from PHD2 src/guide_algorithm.h — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/guide_algorithm.cpp — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/guide_algorithms.h — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/mount.cpp — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/scope.cpp — BSD-3-Clause, see THIRD_PARTY.md
//! Ported from PHD2 src/stepguider.cpp — BSD-3-Clause, see THIRD_PARTY.md

pub mod gp;
pub mod hysteresis;
pub mod identity;
pub mod lowpass;
pub mod lowpass2;
pub mod resist_switch;
pub mod zfilter;
pub mod zfilter_factory;

use super::traits::PierSide;

/// Which algorithm: PHD2's `GUIDE_ALGORITHM` enum, whose integer is what the
/// profile stores (`/<mountClass>/XGuideAlgorithm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum AlgoKind {
    /// "No algorithm": the factory and the UI treat it as [`AlgoKind::Identity`].
    None = -1,
    Identity = 0,
    Hysteresis = 1,
    Lowpass = 2,
    Lowpass2 = 3,
    ResistSwitch = 4,
    /// Predictive PEC.
    GaussianProcess = 5,
    ZFilter = 6,
}

impl AlgoKind {
    /// The persisted integer back to a kind; `None` for anything PHD2's
    /// factory would refuse (`mount.cpp:742`, "invalid guideAlgorithm").
    pub fn from_i32(v: i32) -> Option<AlgoKind> {
        Some(match v {
            -1 => AlgoKind::None,
            0 => AlgoKind::Identity,
            1 => AlgoKind::Hysteresis,
            2 => AlgoKind::Lowpass,
            3 => AlgoKind::Lowpass2,
            4 => AlgoKind::ResistSwitch,
            5 => AlgoKind::GaussianProcess,
            6 => AlgoKind::ZFilter,
            _ => return None,
        })
    }

    /// The untranslated name shown in the UI and written in the guide log's
    /// "X guide algorithm = …" line (`mount.cpp:184`, `GuideAlgorithmName`).
    pub fn ui_name(self) -> &'static str {
        match self {
            AlgoKind::None | AlgoKind::Identity => "None",
            AlgoKind::Hysteresis => "Hysteresis",
            AlgoKind::Lowpass => "Lowpass",
            AlgoKind::Lowpass2 => "Lowpass2",
            AlgoKind::ResistSwitch => "Resist Switch",
            AlgoKind::GaussianProcess => "Predictive PEC",
            AlgoKind::ZFilter => "ZFilter",
        }
    }

    /// The class name each algorithm's `GetGuideAlgorithmClassName` returns:
    /// the last segment of its profile path. "Predictive PEC" keeps its space.
    pub fn class_name(self) -> &'static str {
        match self {
            AlgoKind::None | AlgoKind::Identity => "Identity",
            AlgoKind::Hysteresis => "Hysteresis",
            AlgoKind::Lowpass => "Lowpass",
            AlgoKind::Lowpass2 => "Lowpass2",
            AlgoKind::ResistSwitch => "ResistSwitch",
            AlgoKind::GaussianProcess => "Predictive PEC",
            AlgoKind::ZFilter => "ZFilter",
        }
    }
}

/// A mount axis: X is RA on a scope, Y is Dec (PHD2's `GuideAxis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Axis {
    X,
    Y,
}

impl Axis {
    /// The axis letter in the profile path.
    pub fn letter(self) -> &'static str {
        match self {
            Axis::X => "X",
            Axis::Y => "Y",
        }
    }

    /// `GuideAlgorithm::GetAxis`: "RA" or "DEC" (untranslated).
    pub fn label(self) -> &'static str {
        match self {
            Axis::X => "RA",
            Axis::Y => "DEC",
        }
    }
}

/// Which kind of mount owns the algorithm: the scope (ST-4 / pulse guiding)
/// or a step guider (AO). PHD2's `GetMountClassName`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountClass {
    Scope,
    StepGuider,
}

impl MountClass {
    /// The first segment of the mount's profile paths (`scope.cpp:1933`,
    /// `stepguider.cpp:1304`).
    pub fn class_name(self) -> &'static str {
        match self {
            MountClass::Scope => "scope",
            MountClass::StepGuider => "stepguider",
        }
    }
}

/// The profile group an algorithm's settings live under
/// (`guide_algorithm.cpp:42`), e.g. `/scope/GuideAlgorithm/X/Hysteresis`.
pub fn config_path(mount_class: MountClass, axis: Axis, kind: AlgoKind) -> String {
    format!(
        "/{}/GuideAlgorithm/{}/{}",
        mount_class.class_name(),
        axis.letter(),
        kind.class_name()
    )
}

/// What the guider knows about the frame an algorithm is asked about. The
/// classic algorithms ignore it; the Gaussian process reads the star SNR,
/// the exposure, the clock and the mount's RA rate and pointing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AlgoCtx {
    /// The guide star's SNR on this frame.
    pub snr: f64,
    /// The exposure, seconds.
    pub exposure_s: f64,
    /// The engine clock, seconds.
    pub now: f64,
    /// The calibration's X rate, px per ms.
    pub x_rate_px_ms: f64,
    /// The calibration's RA guide speed, degrees per second; −1 when unknown
    /// (PHD2's `CalibrationDetails::raGuideSpeed`).
    pub ra_guide_speed: f64,
    /// The mount's RA, hours, when the pointing source reports one.
    pub ra_h: Option<f64>,
    /// The mount's pier side.
    pub pier: PierSide,
}

/// A read-only view of stored settings: the value under a key, or `None` to
/// take the default. Booleans come as 0 / 1. [`make`] takes one on full
/// profile paths; an algorithm's constructor one on keys relative to its
/// [`config_path`] (`"minMove"`, `"hysteresis"`, …).
pub type ParamLookup<'a> = &'a dyn Fn(&str) -> Option<f64>;

/// One guide algorithm on one axis: PHD2's `GuideAlgorithm`.
///
/// [`GuideAlgorithm::set_min_move`] and the per-algorithm setters return
/// PHD2's error flag: `true` means the value was refused and the default (or
/// a clamp) was applied instead. [`GuideAlgorithm::set_param`] is the other
/// way round, as in PHD2.
pub trait GuideAlgorithm: Send {
    /// Which algorithm this is.
    fn kind(&self) -> AlgoKind;

    /// `GetGuideAlgorithmClassName`: the last segment of the profile path
    /// (see [`AlgoKind::class_name`]), and what the event server answers to
    /// `get_algo_param` for `"algorithmName"` (`event_server.cpp:1884`). Kept
    /// on the trait so the server can ask the running algorithm.
    fn class_name(&self) -> &'static str {
        self.kind().class_name()
    }

    /// Forget the history.
    fn reset(&mut self);

    /// The move (px) for this frame's offset (px) along the axis.
    fn result(&mut self, input: f64, ctx: &AlgoCtx) -> f64;

    /// The move to make when the star is lost (dead reckoning). Only the
    /// Gaussian process has one; the rest make no move.
    fn deduce_result(&mut self, _ctx: &AlgoCtx) -> f64 {
        0.0
    }

    /// The minimum move, px; −1 when the algorithm has none.
    fn min_move(&self) -> f64 {
        -1.0
    }

    /// Set the minimum move. `true` = error (the base class refuses: no such
    /// parameter).
    fn set_min_move(&mut self, _min_move: f64) -> bool {
        true
    }

    /// The algorithm's own parameter names. The event server's
    /// `get_algo_param_names` puts `"algorithmName"` in front of them
    /// (`event_server.cpp:1849`), which `get_algo_param` answers with
    /// [`GuideAlgorithm::class_name`].
    fn param_names(&self) -> &'static [&'static str] {
        &[]
    }

    /// Read a parameter by its event-server name; `None` when unknown.
    fn get_param(&self, _name: &str) -> Option<f64> {
        None
    }

    /// Set a parameter by its event-server name. `true` = accepted (PHD2's
    /// `!err`): an unknown name or a refused value gives `false`.
    fn set_param(&mut self, _name: &str, _val: f64) -> bool {
        false
    }

    /// The effective settings as (config key, value) pairs, keys relative to
    /// [`config_path`], for the profile layer to persist. Booleans are 0 / 1.
    fn params(&self) -> Vec<(&'static str, f64)> {
        Vec::new()
    }

    /// The text the guide log writes after "X guide algorithm = <name>, "
    /// (`mount.cpp:1930`), with PHD2's trailing newline. Empty by default,
    /// like PHD2's base class; Identity's is a lone `"\n"`.
    fn settings_summary(&self) -> String {
        String::new()
    }

    // Lifecycle hooks, with PHD2's defaults (guide_algorithm.cpp:99-132).

    fn guiding_started(&mut self, _ctx: &AlgoCtx) {}

    fn guiding_stopped(&mut self) {
        self.reset();
    }

    fn guiding_paused(&mut self) {}

    fn guiding_resumed(&mut self) {
        self.reset();
    }

    /// A dither of `amt` px along this axis was commanded.
    fn guiding_dithered(&mut self, _amt: f64, _ctx: &AlgoCtx) {
        self.reset();
    }

    fn dither_settle_done(&mut self, _success: bool) {}

    /// The guider moved the lock position by `amt` without a pulse.
    fn direct_move_applied(&mut self, _amt: f64) {}

    /// By default the history keeps accumulating while guiding output is off.
    fn guiding_disabled(&mut self) {}

    /// Discard whatever accumulated with guiding disabled.
    fn guiding_enabled(&mut self) {
        self.reset();
    }
}

/// `Mount::CreateGuideAlgorithm` (`mount.cpp:710`): build `kind` for an axis.
/// `profile` reads a full profile path (e.g.
/// `/scope/GuideAlgorithm/X/Hysteresis/minMove`), `None` meaning unset; the
/// algorithm asks for its keys under its own [`config_path`]. `None` builds
/// Identity. Like PHD2's factory it builds any kind on any axis: only the
/// choices offered are limited ([`allowed_kinds`]), so a profile asking for
/// Predictive PEC on Dec gets it.
pub fn make(kind: AlgoKind, axis: Axis, mount_class: MountClass, profile: ParamLookup) -> Box<dyn GuideAlgorithm> {
    // The axis and mount class only pick the profile group; the algorithms
    // behave the same on every axis.
    let group = config_path(mount_class, axis, kind);
    let lookup = |key: &str| profile(&format!("{group}/{key}"));
    let params: ParamLookup = &lookup;
    match kind {
        AlgoKind::None | AlgoKind::Identity => Box::new(identity::Identity::new()),
        AlgoKind::Hysteresis => Box::new(hysteresis::Hysteresis::new(params)),
        AlgoKind::Lowpass => Box::new(lowpass::Lowpass::new(params)),
        AlgoKind::Lowpass2 => Box::new(lowpass2::Lowpass2::new(params)),
        AlgoKind::ResistSwitch => Box::new(resist_switch::ResistSwitch::new(params)),
        AlgoKind::GaussianProcess => Box::new(gp::ppec::GaussianProcess::new(params)),
        AlgoKind::ZFilter => Box::new(zfilter::ZFilter::new(params)),
    }
}

/// `Mount::SetXGuideAlgorithm` / `SetYGuideAlgorithm` (`mount.cpp:598-645`)
/// on the integer the profile stores (`/<mountClass>/XGuideAlgorithm` or
/// `YGuideAlgorithm`): −1 means the axis default, and so does a value
/// PHD2's factory refuses (outside −1..=6). Returns the algorithm and the
/// kind to persist under that key.
///
/// PHD2 does all this only when the axis has no algorithm yet or a different
/// one (`mount.cpp:600`): asking for the running kind rebuilds nothing, keeps
/// the history and writes nothing. The caller must skip the call when
/// `current.kind() as i32 == stored`, comparing before −1 is mapped to the
/// default (so −1 does rebuild a running default).
pub fn make_or_default(
    stored: i32,
    axis: Axis,
    mount_class: MountClass,
    profile: ParamLookup,
) -> (Box<dyn GuideAlgorithm>, AlgoKind) {
    let kind = match AlgoKind::from_i32(stored) {
        Some(AlgoKind::None) | None => default_kind(axis, mount_class),
        Some(kind) => kind,
    };
    (make(kind, axis, mount_class, profile), kind)
}

/// The algorithms the Brain offers for an axis, in PHD2's order
/// (`mount.cpp:227-240`): Predictive PEC is RA-only, Resist Switch is not
/// offered on an AO.
pub fn allowed_kinds(axis: Axis, mount_class: MountClass) -> &'static [AlgoKind] {
    use AlgoKind::*;
    match (mount_class, axis) {
        (MountClass::Scope, Axis::X) => &[Hysteresis, Lowpass, Lowpass2, ResistSwitch, GaussianProcess, ZFilter],
        (MountClass::Scope, Axis::Y) => &[Hysteresis, Lowpass, Lowpass2, ResistSwitch, ZFilter],
        (MountClass::StepGuider, _) => &[Hysteresis, Lowpass, Lowpass2, ZFilter],
    }
}

/// Whether the Brain offers `kind` for an axis (see [`allowed_kinds`]).
pub fn allowed(kind: AlgoKind, axis: Axis, mount_class: MountClass) -> bool {
    allowed_kinds(axis, mount_class).contains(&kind)
}

/// The default per axis: a scope guides RA with Hysteresis and Dec with
/// Resist Switch (`scope.cpp:54-55`); an AO uses Hysteresis on both
/// (`stepguider.cpp:48`).
pub fn default_kind(axis: Axis, mount_class: MountClass) -> AlgoKind {
    match (mount_class, axis) {
        (MountClass::Scope, Axis::X) => AlgoKind::Hysteresis,
        (MountClass::Scope, Axis::Y) => AlgoKind::ResistSwitch,
        (MountClass::StepGuider, _) => AlgoKind::Hysteresis,
    }
}

/// The min-move PHD2 proposes for an image scale (arcsec/px):
/// `max(0.1515 + 0.1548 / scale, 0.15)`, from empirical data over a range of
/// scales (`guide_algorithm.cpp:68`). No guard, like PHD2: a zero scale gives
/// infinity, an infinite one 0.1515, a NaN 0.15 (`wxMax` keeps its second
/// argument when the comparison fails, as `f64::max` does).
pub fn smart_default_min_move(image_scale: f64) -> f64 {
    (0.1515 + 0.1548 / image_scale).max(0.15)
}

/// PHD2's `SmartDefaultMinMove(focalLength, pixelSize, binning)`, which the
/// profile wizard, the Guiding Assistant and the image-scale change in the
/// Brain call directly: the image scale is
/// `206.265 × pixel size × binning / focal length` (`myframe.h:733`). Quirk
/// kept: nothing checks the focal length here, so 0 mm gives an infinite
/// scale and 0.1515.
pub fn smart_default_min_move_for(focal_length_mm: i32, pixel_size_um: f64, binning: u32) -> f64 {
    let scale = 206.265 * pixel_size_um * binning as f64 / focal_length_mm as f64;
    smart_default_min_move(scale)
}

/// PHD2's no-argument `SmartDefaultMinMove()` (`guide_algorithm.cpp:71`),
/// what `ResetParams` applies from the current equipment: 0.2 when the focal
/// length is 0 (unknown), else [`smart_default_min_move_for`].
///
/// `ResetParams` (`guide_algorithm.cpp:56`, the Brain's reset button) is the
/// caller's: delete the algorithm's [`config_path`] group, [`make`] it again
/// on an empty lookup, call `set_min_move` with this value when `min_move()`
/// is not negative, then save its `params()`.
pub fn smart_default_min_move_current(focal_length_mm: i32, pixel_size_um: f64, binning: u32) -> f64 {
    if focal_length_mm == 0 {
        return 0.2;
    }
    smart_default_min_move_for(focal_length_mm, pixel_size_um, binning)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_params(_: &str) -> Option<f64> {
        None
    }

    #[test]
    fn kind_discriminants_round_trip() {
        for (v, k) in [
            (-1, AlgoKind::None),
            (0, AlgoKind::Identity),
            (1, AlgoKind::Hysteresis),
            (2, AlgoKind::Lowpass),
            (3, AlgoKind::Lowpass2),
            (4, AlgoKind::ResistSwitch),
            (5, AlgoKind::GaussianProcess),
            (6, AlgoKind::ZFilter),
        ] {
            assert_eq!(k as i32, v);
            assert_eq!(AlgoKind::from_i32(v), Some(k));
        }
        assert_eq!(AlgoKind::from_i32(7), None);
        assert_eq!(AlgoKind::from_i32(-2), None);
    }

    #[test]
    fn names() {
        assert_eq!(AlgoKind::None.ui_name(), "None");
        assert_eq!(AlgoKind::Identity.ui_name(), "None");
        assert_eq!(AlgoKind::Identity.class_name(), "Identity");
        assert_eq!(AlgoKind::ResistSwitch.ui_name(), "Resist Switch");
        assert_eq!(AlgoKind::ResistSwitch.class_name(), "ResistSwitch");
        assert_eq!(AlgoKind::GaussianProcess.ui_name(), "Predictive PEC");
        assert_eq!(AlgoKind::GaussianProcess.class_name(), "Predictive PEC");
        assert_eq!(AlgoKind::ZFilter.class_name(), "ZFilter");
        assert_eq!(Axis::X.label(), "RA");
        assert_eq!(Axis::Y.label(), "DEC");
        assert_eq!(
            config_path(MountClass::Scope, Axis::X, AlgoKind::Hysteresis),
            "/scope/GuideAlgorithm/X/Hysteresis"
        );
        assert_eq!(
            config_path(MountClass::StepGuider, Axis::Y, AlgoKind::GaussianProcess),
            "/stepguider/GuideAlgorithm/Y/Predictive PEC"
        );
    }

    #[test]
    fn allowed_table() {
        use AlgoKind::*;
        let ra = [Hysteresis, Lowpass, Lowpass2, ResistSwitch, GaussianProcess, ZFilter];
        let dec = [Hysteresis, Lowpass, Lowpass2, ResistSwitch, ZFilter];
        let ao = [Hysteresis, Lowpass, Lowpass2, ZFilter];
        let all = [None, Identity, Hysteresis, Lowpass, Lowpass2, ResistSwitch, GaussianProcess, ZFilter];
        // The order is the Brain's.
        assert_eq!(allowed_kinds(Axis::X, MountClass::Scope), ra);
        assert_eq!(allowed_kinds(Axis::Y, MountClass::Scope), dec);
        assert_eq!(allowed_kinds(Axis::X, MountClass::StepGuider), ao);
        assert_eq!(allowed_kinds(Axis::Y, MountClass::StepGuider), ao);
        for k in all {
            assert_eq!(allowed(k, Axis::X, MountClass::Scope), ra.contains(&k), "{k:?} RA");
            assert_eq!(allowed(k, Axis::Y, MountClass::Scope), dec.contains(&k), "{k:?} DEC");
            assert_eq!(allowed(k, Axis::X, MountClass::StepGuider), ao.contains(&k), "{k:?} AO X");
            assert_eq!(allowed(k, Axis::Y, MountClass::StepGuider), ao.contains(&k), "{k:?} AO Y");
        }
    }

    #[test]
    fn defaults() {
        assert_eq!(default_kind(Axis::X, MountClass::Scope), AlgoKind::Hysteresis);
        assert_eq!(default_kind(Axis::Y, MountClass::Scope), AlgoKind::ResistSwitch);
        assert_eq!(default_kind(Axis::X, MountClass::StepGuider), AlgoKind::Hysteresis);
        assert_eq!(default_kind(Axis::Y, MountClass::StepGuider), AlgoKind::Hysteresis);
    }

    #[test]
    fn factory() {
        let a = make(AlgoKind::None, Axis::X, MountClass::Scope, &no_params);
        assert_eq!(a.kind(), AlgoKind::Identity);
        // Every kind on every axis, Predictive PEC on Dec and on an AO
        // included: PHD2's factory checks neither.
        for k in [
            AlgoKind::Identity,
            AlgoKind::Hysteresis,
            AlgoKind::Lowpass,
            AlgoKind::Lowpass2,
            AlgoKind::ResistSwitch,
            AlgoKind::GaussianProcess,
            AlgoKind::ZFilter,
        ] {
            for (axis, mc) in [
                (Axis::X, MountClass::Scope),
                (Axis::Y, MountClass::Scope),
                (Axis::X, MountClass::StepGuider),
                (Axis::Y, MountClass::StepGuider),
            ] {
                let a = make(k, axis, mc, &no_params);
                assert_eq!(a.kind(), k);
                assert_eq!(a.class_name(), k.class_name());
            }
        }
    }

    #[test]
    fn factory_reads_the_algorithm_group() {
        let profile = |k: &str| match k {
            "/scope/GuideAlgorithm/Y/ResistSwitch/minMove" => Some(0.4),
            "/scope/GuideAlgorithm/X/ResistSwitch/minMove" => Some(0.5),
            "/stepguider/GuideAlgorithm/Y/Hysteresis/aggression" => Some(1.2),
            "/scope/GuideAlgorithm/Y/Hysteresis/aggression" => Some(0.3),
            _ => None,
        };
        let a = make(AlgoKind::ResistSwitch, Axis::Y, MountClass::Scope, &profile);
        assert_eq!(a.min_move(), 0.4);
        let a = make(AlgoKind::Hysteresis, Axis::Y, MountClass::StepGuider, &profile);
        assert_eq!(a.get_param("aggression"), Some(1.2));
        assert_eq!(a.min_move(), 0.2);
        // Predictive PEC's group keeps the space in its class name.
        let profile = |k: &str| (k == "/scope/GuideAlgorithm/X/Predictive PEC/gp_min_move").then_some(0.3);
        let a = make(AlgoKind::GaussianProcess, Axis::X, MountClass::Scope, &profile);
        assert_eq!(a.min_move(), 0.3);
        // A fallback reads the default algorithm's own group.
        let profile = |k: &str| (k == "/stepguider/GuideAlgorithm/X/Hysteresis/minMove").then_some(0.33);
        let (a, _) = make_or_default(99, Axis::X, MountClass::StepGuider, &profile);
        assert_eq!(a.min_move(), 0.33);
    }

    #[test]
    fn summaries() {
        let algo = |k| make(k, Axis::X, MountClass::Scope, &no_params).settings_summary();
        // Identity's newline ends the X line: "X guide algorithm = None, \n".
        assert_eq!(algo(AlgoKind::None), "\n");
        assert_eq!(algo(AlgoKind::Hysteresis), "Hysteresis = 0.100, Aggression = 0.700, Minimum move = 0.200\n");
        assert_eq!(algo(AlgoKind::Lowpass), "Slope weight = 5.000, Minimum move = 0.200\n");
        assert_eq!(algo(AlgoKind::Lowpass2), "Aggressiveness = 80.000, Minimum move = 0.200\n");
        assert_eq!(algo(AlgoKind::ResistSwitch), "Minimum move = 0.200 Aggression = 100% FastSwitch = enabled\n");
        assert_eq!(algo(AlgoKind::ZFilter), "Type=Bessel-4, Exp-factor=2.0, Minimum move = 0.100\n");
    }

    #[test]
    fn make_or_default_falls_back() {
        // −1 and the values PHD2's factory refuses take the axis default.
        let (a, k) = make_or_default(-1, Axis::Y, MountClass::Scope, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::ResistSwitch, AlgoKind::ResistSwitch));
        let (a, k) = make_or_default(7, Axis::X, MountClass::StepGuider, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::Hysteresis, AlgoKind::Hysteresis));
        let (a, k) = make_or_default(-2, Axis::Y, MountClass::Scope, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::ResistSwitch, AlgoKind::ResistSwitch));
        // Any valid kind is built and kept, offered on that axis or not.
        let (a, k) = make_or_default(0, Axis::X, MountClass::Scope, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::Identity, AlgoKind::Identity));
        let (a, k) = make_or_default(6, Axis::X, MountClass::StepGuider, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::ZFilter, AlgoKind::ZFilter));
        let (a, k) = make_or_default(5, Axis::Y, MountClass::Scope, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::GaussianProcess, AlgoKind::GaussianProcess));
        let (a, k) = make_or_default(3, Axis::X, MountClass::Scope, &no_params);
        assert_eq!((a.kind(), k), (AlgoKind::Lowpass2, AlgoKind::Lowpass2));
    }

    #[test]
    fn trait_defaults_and_hooks() {
        // Identity has no min-move and no params; the base defaults apply
        // but for its summary.
        let mut a = make(AlgoKind::Identity, Axis::X, MountClass::Scope, &no_params);
        assert_eq!(a.min_move(), -1.0);
        assert!(a.set_min_move(0.3));
        assert!(a.param_names().is_empty());
        assert_eq!(a.get_param("minMove"), None);
        assert!(!a.set_param("minMove", 0.3));
        assert_eq!(a.settings_summary(), "\n");
        assert_eq!(a.deduce_result(&AlgoCtx::default()), 0.0);

        // The hooks that reset drop Hysteresis' last move; the others keep it.
        let ctx = AlgoCtx::default();
        let mut h = make(AlgoKind::Hysteresis, Axis::X, MountClass::Scope, &no_params);
        let prime = |h: &mut Box<dyn GuideAlgorithm>| {
            h.result(1.0, &ctx);
        };
        let carries = |h: &mut Box<dyn GuideAlgorithm>| h.result(1.0, &ctx) > 0.63 + 1e-12;
        prime(&mut h);
        h.guiding_started(&ctx);
        assert!(carries(&mut h));
        h.guiding_paused();
        assert!(carries(&mut h));
        h.dither_settle_done(true);
        assert!(carries(&mut h));
        h.direct_move_applied(0.5);
        assert!(carries(&mut h));
        h.guiding_disabled();
        assert!(carries(&mut h));
        h.guiding_stopped();
        assert!(!carries(&mut h));
        h.guiding_resumed();
        assert!(!carries(&mut h));
        prime(&mut h);
        h.guiding_dithered(2.0, &ctx);
        assert!(!carries(&mut h));
        prime(&mut h);
        h.guiding_enabled();
        assert!(!carries(&mut h));
    }

    #[test]
    fn smart_min_move() {
        // 1.0"/px: 0.1515 + 0.1548 = 0.3063.
        assert!((smart_default_min_move(1.0) - 0.3063).abs() < 1e-12);
        // The 0.15 floor never binds for a positive scale (0.1515 alone is
        // above it): a coarse 1000"/px still gives 0.1515 + 0.0001548.
        assert_eq!(smart_default_min_move(1000.0), 0.1515 + 0.1548 / 1000.0);
        // Unguarded, like PHD2.
        assert_eq!(smart_default_min_move(f64::INFINITY), 0.1515);
        assert_eq!(smart_default_min_move(0.0), f64::INFINITY);
        assert_eq!(smart_default_min_move(f64::NAN), 0.15);
        // 600 mm, 3.75 µm, bin 1 → 1.2892"/px → 0.1515 + 0.1548 / 1.2892.
        let scale = 206.265 * 3.75 / 600.0;
        assert_eq!(smart_default_min_move_for(600, 3.75, 1), 0.1515 + 0.1548 / scale);
        // 2×2 binning halves the second term's share: 0.1515 + 0.1548 / 2.5784.
        assert_eq!(smart_default_min_move_for(600, 3.75, 2), 0.1515 + 0.1548 / (2.0 * scale));
        // The direct form does not check the focal length; the current-gear one does.
        assert_eq!(smart_default_min_move_for(0, 3.75, 1), 0.1515);
        assert_eq!(smart_default_min_move_current(0, 3.75, 1), 0.2);
        assert_eq!(smart_default_min_move_current(600, 3.75, 1), smart_default_min_move_for(600, 3.75, 1));
        // Unknown pixel size (PHD2 stores 0): an infinite min-move, as PHD2.
        assert_eq!(smart_default_min_move_current(600, 0.0, 1), f64::INFINITY);
    }
}
