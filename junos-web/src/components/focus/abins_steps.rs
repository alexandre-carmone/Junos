//! Aberration Inspector: what to do next, from a result. `steps` is pure and
//! applied in order: the run's own problems first (bad datapoints, no usable
//! centre tile, no step size — fix and re-run), then the spacing, then the
//! tilt (the plate's screws when one is fitted), then a new run to check.
//! Spacing comes before tilt: it moves the whole sensor and every tile with
//! it, so the tilt is read again afterwards.
//!
//! The yardstick is the run's CFZ, as in the tile map: within half of it a
//! correction is left alone, within it is optional, beyond it is needed.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE};
use crate::i18n::Translations;
use crate::ws::{AbInsResult, AbInsScrew, AbInsTiltPlate};

use super::abins_plate::{direction, position_text, NO_CHANGE_MM};

/// The centre tile's `id`, the reference of every delta.
const CENTRE: i64 = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Severity {
    Needed,
    Optional,
    Info,
    Good,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Step {
    /// `insufficientDatapoints` or `datapointMismatch`.
    FixData,
    /// The centre tile not fit, excluded or missing.
    CentreUnusable,
    /// No focuser step size or CFZ: the deltas have no scale.
    SetStepSize,
    /// Move the sensor nearer the flattener (or away) by `microns`.
    Spacing { nearer: bool, microns: f64, severity: Severity },
    /// The plate's screws to turn, largest first.
    Screws { screws: Vec<AbInsScrew>, severity: Severity },
    /// The tilt, without a plate to correct it with.
    Tilt { lr: f64, tb: f64, severity: Severity },
    /// The backfocus could not be measured: no usable tile around the centre.
    NoBackfocus,
    /// The tilt could not be measured: a side (left, right, top or bottom)
    /// without a usable tile.
    NoTilt,
    Rerun,
    AllGood,
}

impl Step {
    pub(super) fn severity(&self) -> Severity {
        match self {
            Step::FixData | Step::CentreUnusable | Step::SetStepSize | Step::NoBackfocus | Step::NoTilt => Severity::Needed,
            Step::Spacing { severity, .. } | Step::Screws { severity, .. } | Step::Tilt { severity, .. } => *severity,
            Step::Rerun => Severity::Info,
            Step::AllGood => Severity::Good,
        }
    }
}

/// Needed beyond the CFZ, optional within it (the caller has already left
/// out anything within half of it).
fn severity(microns: f64, cfz: f64) -> Severity {
    if microns.abs() > cfz { Severity::Needed } else { Severity::Optional }
}

/// The steps for `r`, `plate` being the screws under the current settings
/// when a tilt plate is fitted.
pub(super) fn steps(r: &AbInsResult, plate: Option<&AbInsTiltPlate>) -> Vec<Step> {
    if r.warnings.iter().any(|w| w == "insufficientDatapoints" || w == "datapointMismatch") {
        return vec![Step::FixData, Step::Rerun];
    }
    if !r.tiles.iter().any(|t| t.id == CENTRE && t.fit && !t.excluded) {
        return vec![Step::CentreUnusable, Step::Rerun];
    }
    if r.focuser_step_microns <= 0.0 || r.cfz_steps <= 0.0 {
        return vec![Step::SetStepSize, Step::Rerun];
    }
    let cfz = r.cfz_microns();
    let mut out = Vec::new();

    let b = &r.backfocus;
    if b.ok && b.delta_microns.abs() > cfz / 2.0 {
        out.push(Step::Spacing {
            nearer: b.delta_microns < 0.0,
            microns: b.delta_microns.abs(),
            severity: severity(b.delta_microns, cfz),
        });
    }

    let t = &r.tilt;
    let worst = t.lr_microns.abs().max(t.tb_microns.abs());
    if t.ok && worst > cfz / 2.0 {
        let mut screws: Vec<AbInsScrew> = plate
            .map(|p| p.points.iter().filter(|s| s.adjustment_mm.abs() >= NO_CHANGE_MM).cloned().collect())
            .unwrap_or_default();
        screws.sort_by(|a, b| b.adjustment_mm.abs().total_cmp(&a.adjustment_mm.abs()));
        let severity = severity(worst, cfz);
        out.push(if screws.is_empty() {
            Step::Tilt { lr: t.lr_microns, tb: t.tb_microns, severity }
        } else {
            Step::Screws { screws, severity }
        });
    }

    // A measure that failed is not a good one: say so rather than "all good".
    if !b.ok {
        out.push(Step::NoBackfocus);
    }
    if !t.ok {
        out.push(Step::NoTilt);
    }

    out.push(if out.is_empty() { Step::AllGood } else { Step::Rerun });
    out
}

pub(super) fn severity_class(s: Severity) -> &'static str {
    match s {
        Severity::Needed => "text-state-err",
        Severity::Optional => "text-state-warn",
        Severity::Info => "text-text-blue",
        Severity::Good => "text-state-ok",
    }
}

/// A step in one line (the screws' detail is the card's).
pub(super) fn title(step: &Step, tr: &'static Translations) -> String {
    match step {
        Step::FixData => tr.abins_do_fix_data.to_string(),
        Step::CentreUnusable => tr.abins_do_centre.to_string(),
        Step::SetStepSize => tr.abins_do_step_size.to_string(),
        Step::Spacing { nearer, microns, .. } => format!("{} · {microns:.0} µm",
            if *nearer { tr.abins_advice_nearer } else { tr.abins_advice_away }),
        Step::Screws { .. } => tr.abins_do_screws.to_string(),
        Step::Tilt { lr, tb, .. } => format!("{} · {} {lr:+.0} µm · {} {tb:+.0} µm",
            tr.abins_do_tilt, tr.abins_tilt_lr, tr.abins_tilt_tb),
        Step::NoBackfocus => tr.abins_do_no_backfocus.to_string(),
        Step::NoTilt => tr.abins_do_no_tilt.to_string(),
        Step::Rerun => tr.abins_do_rerun.to_string(),
        Step::AllGood => tr.abins_do_all_good.to_string(),
    }
}

/// The "What to do" card: the steps as an ordered list.
pub(super) fn steps_card(steps: &[Step], tr: &'static Translations) -> impl IntoView + use<> {
    let items = steps.iter().map(|step| {
        let sev = step.severity();
        let screws = match step {
            Step::Screws { screws, .. } => Some(screws.iter().map(|s| {
                let (_, cls, dir) = direction(s.adjustment_mm, tr);
                view! {
                    <li class="font-mono text-xs">
                        {format!("#{} · {} · ", s.index, position_text(&s.position, tr))}
                        <span class=cls>{dir}</span>
                        {format!(" · {:+.3} mm · {:+.2} {}", s.adjustment_mm, s.turns, tr.abins_turns.to_lowercase())}
                    </li>
                }
            }).collect_view()),
            _ => None,
        };
        view! {
            <li class="leading-tight">
                <span class=severity_class(sev)>{title(step, tr)}</span>
                {(sev == Severity::Optional).then(|| view! {
                    <span class="ml-1.5 text-xs text-text-muted">{format!("({})", tr.abins_optional)}</span>
                })}
                {screws.map(|s| view! { <ul class="mt-1 flex flex-col gap-0.5">{s}</ul> })}
            </li>
        }
    }).collect_view();
    view! {
        <div class=CARD>
            <span class=CARD_TITLE>{tr.abins_todo}</span>
            <ol class="list-decimal pl-5 flex flex-col gap-1.5">{items}</ol>
            <div class="text-xs text-text-muted leading-tight">{tr.abins_todo_hint}</div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::{AbInsBackfocus, AbInsTile, AbInsTilt};

    /// A clean run: 3 µm steps, a 30-step CFZ (90 µm), every tile fit, no
    /// backfocus nor tilt.
    fn run() -> AbInsResult {
        AbInsResult {
            focuser_step_microns: 3.0,
            cfz_steps: 30.0,
            tiles: (0..9).map(|id| AbInsTile { id, used: true, fit: true, ..Default::default() }).collect(),
            backfocus: AbInsBackfocus { ok: true, ..Default::default() },
            tilt: AbInsTilt { ok: true, ..Default::default() },
            ..Default::default()
        }
    }

    fn screw(index: i64, mm: f64) -> AbInsScrew {
        AbInsScrew { index, adjustment_mm: mm, ..Default::default() }
    }

    #[test]
    fn all_good_within_half_the_cfz() {
        let mut r = run();
        r.backfocus.delta_microns = 40.0;
        r.tilt.lr_microns = -44.0;
        assert_eq!(steps(&r, None), vec![Step::AllGood]);
    }

    #[test]
    fn backfocus_only() {
        let mut r = run();
        r.backfocus.delta_microns = -60.0;
        assert_eq!(steps(&r, None), vec![
            Step::Spacing { nearer: true, microns: 60.0, severity: Severity::Optional },
            Step::Rerun,
        ]);
        r.backfocus.delta_microns = 120.0;
        assert_eq!(steps(&r, None)[0], Step::Spacing { nearer: false, microns: 120.0, severity: Severity::Needed });
    }

    #[test]
    fn tilt_with_plate_comes_after_spacing_largest_screw_first() {
        let mut r = run();
        r.backfocus.delta_microns = 100.0;
        r.tilt.tb_microns = -95.0;
        let plate = AbInsTiltPlate {
            points: vec![screw(1, 0.010), screw(2, -0.0004), screw(3, -0.030)],
            ..Default::default()
        };
        let s = steps(&r, Some(&plate));
        assert_eq!(s.len(), 3);
        assert!(matches!(s[0], Step::Spacing { nearer: false, .. }));
        assert_eq!(s[1], Step::Screws { screws: vec![screw(3, -0.030), screw(1, 0.010)], severity: Severity::Needed });
        assert_eq!(s[2], Step::Rerun);
        // No plate: the tilt itself.
        assert_eq!(steps(&r, None)[1], Step::Tilt { lr: 0.0, tb: -95.0, severity: Severity::Needed });
    }

    #[test]
    fn centre_not_fit_or_excluded() {
        let mut r = run();
        r.backfocus.delta_microns = 500.0;
        r.tiles[4].fit = false;
        assert_eq!(steps(&r, None), vec![Step::CentreUnusable, Step::Rerun]);
        r.tiles[4].fit = true;
        r.tiles[4].excluded = true;
        assert_eq!(steps(&r, None), vec![Step::CentreUnusable, Step::Rerun]);
    }

    #[test]
    fn unmeasured_is_not_all_good() {
        let mut r = run();
        r.tilt.ok = false;
        assert_eq!(steps(&r, None), vec![Step::NoTilt, Step::Rerun]);
        r.backfocus.delta_microns = 120.0;
        assert_eq!(steps(&r, None), vec![
            Step::Spacing { nearer: false, microns: 120.0, severity: Severity::Needed },
            Step::NoTilt,
            Step::Rerun,
        ]);
        let mut r = run();
        r.backfocus.ok = false;
        assert_eq!(steps(&r, None), vec![Step::NoBackfocus, Step::Rerun]);
    }

    #[test]
    fn no_step_size() {
        let mut r = run();
        r.focuser_step_microns = 0.0;
        assert_eq!(steps(&r, None), vec![Step::SetStepSize, Step::Rerun]);
        let mut r = run();
        r.warnings = vec!["datapointMismatch".into()];
        r.focuser_step_microns = 0.0;
        assert_eq!(steps(&r, None), vec![Step::FixData, Step::Rerun]);
    }
}
