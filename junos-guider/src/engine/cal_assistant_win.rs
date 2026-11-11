//! The Calibration Assistant window as the engine drives it: the target
//! location, the slews there through the pointing source (each awaited on
//! the 1 s tick, then the 2 s pause PHD2 makes), the forced calibration and
//! its verdict once the guider is calibrated. The window's arithmetic and
//! texts are [`super::tools::cal_assistant`]'s.
//!
//! Ported from PHD2 src/calibration_assistant.cpp (InitializeUI, UpdateCurrentPosition, OnSlew, PerformSlew, OnCalibrate, TrackCalibration, PerformSanityChecks) — BSD-3-Clause, see THIRD_PARTY.md

use std::collections::VecDeque;

use serde_json::{Value, json};

use super::calibration::{self, CALSTEP_DEFAULT_STEPS};
use super::command::{CalAssistCmd, CmdError, Reply};
use super::control::{GuideOpts, SettleParams, can_guide};
use super::guider::GuiderState;
use super::log::cfmt::fmt_f;
use super::static_pa_win::pointing_mut;
use super::tools::cal_assistant::{self as ca, MERIDIAN_FLIP, Target, Verdict};
use super::traits::Rect;
use super::Engine;

/// What the window does next, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    /// A slew, the status shown before it.
    Slew { ra: f64, dec: f64, status: &'static str },
    /// The status once the slews before it are done.
    Status(&'static str),
    Calibrate,
}

/// A slew under way: started, or done and pausing until a time (ms).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Slewing {
    Moving,
    Pausing(i64),
}

pub(super) struct CalAssistWin {
    target: Target,
    /// Offset, Dec as shown; east.
    current: (String, String, bool),
    warning: &'static str,
    explanation: String,
    status: String,
    /// A fatal error disables Slew and Calibrate.
    fatal: bool,
    steps: VecDeque<Step>,
    slewing: Option<Slewing>,
    meridian_flipping: bool,
    just_slewed: bool,
    sanity_done: bool,
    sanity: Option<&'static str>,
    monitoring: bool,
    calibration_active: bool,
    last_result: String,
    verdict: Option<Verdict>,
    /// `UpdateCurrentPosition`'s static `lastDec`.
    last_dec: f64,
}

const INVALID_DEC: f64 = 100.0;

impl Engine {
    /// One of the window's controls.
    pub(super) fn cal_assistant(&mut self, c: CalAssistCmd) -> Reply {
        match c {
            CalAssistCmd::Open => self.cal_assistant_open(),
            CalAssistCmd::Set { offset, dec, east } => self.cal_assistant_set(offset, dec, east),
            CalAssistCmd::SaveCustom { single_side } => self.cal_assistant_location("save", single_side),
            CalAssistCmd::LoadCustom => self.cal_assistant_location("load", false),
            CalAssistCmd::Restore => self.cal_assistant_location("restore", false),
            CalAssistCmd::Slew => self.cal_assistant_slew(),
            CalAssistCmd::Calibrate => self.cal_assistant_calibrate(),
            CalAssistCmd::Stop => self.cal_assistant_stop(),
            CalAssistCmd::Close => self.cal_assistant_close(),
        }
    }

    /// `pPointingSource` reporting a position: (RA, Dec, LST), or PHD2's
    /// error.
    fn ca_coords(&self) -> Result<(f64, f64, f64), &'static str> {
        match self.pointing() {
            Some(p) if p.can_report_position() => {
                p.coordinates().ok_or("Mount can't report its pointing position")
            }
            Some(_) => Err("Mount can't report its pointing position"),
            None => Err("Mount is not connected"),
        }
    }

    fn ca_can_slew(&self) -> bool {
        self.pointing().is_some_and(|p| p.can_slew_async())
    }

    fn ca(&mut self) -> Result<&mut CalAssistWin, CmdError> {
        self.cal_assist.as_mut().ok_or_else(|| CmdError::new(1, "Calibration Assistant not open"))
    }

    /// Open (`InitializeUI(false)`); PHD2's errors refuse it.
    fn cal_assistant_open(&mut self) -> Reply {
        let coords = self.ca_coords().map_err(|e| CmdError::new(1, e))?;
        if self.cal_assist.is_none() {
            self.cal_assist = Some(CalAssistWin {
                target: Target { offset: 10, dec: 0, east: true },
                current: (String::new(), String::new(), true),
                warning: "",
                explanation: String::new(),
                status: String::new(),
                fatal: false,
                steps: VecDeque::new(),
                slewing: None,
                meridian_flipping: false,
                just_slewed: false,
                sanity_done: false,
                sanity: None,
                monitoring: false,
                calibration_active: false,
                last_result: String::new(),
                verdict: None,
                last_dec: INVALID_DEC,
            });
            self.ca_initialize(false, coords);
        }
        Ok(json!(0))
    }

    /// `InitializeUI`: the target from the custom location or the
    /// recommendation (`defaults`), the current position, the status.
    fn ca_initialize(&mut self, defaults: bool, coords: (f64, f64, f64)) {
        let site_lat = self.pointing().and_then(|p| p.site_lat_long()).map(|(lat, _)| lat);
        let can_slew = self.ca_can_slew();
        let target = ca::initial_target(&self.store.profile(), defaults, coords, site_lat);
        let Some(w) = self.cal_assist.as_mut() else { return };
        w.target = target;
        w.explanation = ca::explanation(coords.1, can_slew);
        w.current = ca::current(coords, false);
        w.warning = if w.current.2 != w.target.east { MERIDIAN_FLIP } else { "" };
        w.status = match can_slew {
            true => "Adjust 'Calibration Location' values if needed for your site, then click 'Slew'".into(),
            false => format!("Manually move the telescope to a Dec location near {}", target.dec),
        };
    }

    /// The target fields (each one sent changes; the spinners' ranges).
    fn cal_assistant_set(&mut self, offset: Option<i32>, dec: Option<i32>, east: Option<bool>) -> Reply {
        let w = self.ca()?;
        if let Some(o) = offset {
            w.target.offset = o.clamp(ca::OFFSET_RANGE.0, ca::OFFSET_RANGE.1);
        }
        if let Some(d) = dec {
            w.target.dec = d.clamp(ca::DEC_RANGE.0, ca::DEC_RANGE.1);
        }
        if let Some(e) = east {
            w.target.east = e;
            // `OnTargetEast` / `OnTargetWest`.
            w.warning = if w.current.2 != e { MERIDIAN_FLIP } else { "" };
        }
        Ok(json!(0))
    }

    /// Save the target as the custom location (`CalCustomDialog::OnOk`),
    /// Load it, or Restore the recommendation.
    fn cal_assistant_location(&mut self, action: &str, single_side: bool) -> Reply {
        let target = self.ca()?.target;
        match action {
            "save" => {
                ca::save_custom(&mut self.store.profile(), target, single_side);
                // `LoadCustomPosition`.
                let w = self.ca()?;
                w.warning = if w.current.2 != target.east { MERIDIAN_FLIP } else { "" };
            }
            "load" | "restore" => {
                let coords = self.ca_coords().map_err(|e| CmdError::new(1, e))?;
                self.ca_initialize(action == "restore", coords);
            }
            _ => return Err(CmdError::new(1, "invalid action")),
        }
        Ok(json!(0))
    }

    /// Slew (`OnSlew`).
    fn cal_assistant_slew(&mut self) -> Reply {
        let busy = self.ca()?.slewing.is_some();
        if busy {
            return Err(CmdError::new(1, "Slewing..."));
        }
        let Some((ra, dec, lst)) = self.pointing().and_then(|p| p.coordinates()) else {
            self.debug("Cal-slew: slew failed to get scope coordinates");
            let w = self.ca()?;
            (w.status, w.fatal) = ("Could not get coordinates from mount".into(), true);
            return Ok(json!(0));
        };
        let w = self.ca()?;
        w.meridian_flipping = !w.warning.is_empty();
        let plan = ca::slew_plan(lst, dec, w.target, w.meridian_flipping);
        let (to_ra, to_dec) = (plan[plan.len() - 1].0, plan[plan.len() - 1].1);
        let line = format!(
            "CalAsst: slew from ra {}, dec {} to ra {}, dec {}, M/F = {}",
            fmt_f(ra, 2),
            fmt_f(dec, 1),
            fmt_f(to_ra, 2),
            fmt_f(to_dec, 1),
            w.meridian_flipping as i32
        );
        // PHD2's two texts differ by one capital.
        let done = if plan.len() > 1 {
            "Wait for tracking to stabilize, then click 'Calibrate' to start calibration or 'Cancel' to exit"
        } else {
            "Wait for tracking to stabilize, then Click 'Calibrate' to start calibration or 'Cancel' to exit"
        };
        w.status.clear();
        w.steps = plan.into_iter().map(|(ra, dec, status)| Step::Slew { ra, dec, status }).collect();
        w.steps.push_back(Step::Status(done));
        self.debug(&line);
        self.ca_next();
        Ok(json!(0))
    }

    /// Calibrate (`OnCalibrate`).
    fn cal_assistant_calibrate(&mut self) -> Reply {
        if self.ca()?.slewing.is_some() {
            return Err(CmdError::new(1, "Slewing..."));
        }
        let Some((ra, dec, _)) = self.pointing().and_then(|p| p.coordinates()) else {
            let w = self.ca()?;
            (w.status, w.fatal) = ("Scope isn't reporting current position".into(), true);
            return Ok(json!(0));
        };
        if !self.ca()?.sanity_done {
            let sanity = self.ca_sanity(dec);
            let w = self.ca()?;
            (w.sanity_done, w.sanity) = (true, sanity);
        }
        if dec >= 80.0 {
            self.ca()?.status = "Slew the scope closer to Dec = 0".into();
            return Ok(json!(0));
        }
        let can_slew = self.ca_can_slew();
        let w = self.ca()?;
        if !w.just_slewed && can_slew {
            w.status = "Pre-clearing backlash".into();
            w.steps = VecDeque::from([Step::Slew { ra, dec: dec + 2.0 / 60.0, status: "" }, Step::Calibrate]);
        } else {
            w.steps = VecDeque::from([Step::Calibrate]);
        }
        self.ca_next();
        Ok(json!(0))
    }

    /// The slew's Cancel (`RunInBg`'s): the slew is aborted.
    fn cal_assistant_stop(&mut self) -> Reply {
        if self.ca()?.slewing.take().is_some() {
            if let Some(p) = pointing_mut(&mut self.scope, &mut self.aux, &mut self.manual_pointing) {
                p.abort_slew();
            }
            let w = self.ca()?;
            w.steps.clear();
            w.status = "Slew was cancelled".into();
        }
        Ok(json!(0))
    }

    fn cal_assistant_close(&mut self) -> Reply {
        self.cal_assistant_stop()?;
        self.cal_assist = None;
        Ok(json!(0))
    }

    /// `PerformSanityChecks`: from the pointing source's guide rates.
    fn ca_sanity(&self, dec: f64) -> Option<&'static str> {
        let (ra_spd, dec_spd) = self.pointing()?.guide_rates()?;
        if ra_spd <= 0.0 {
            return None;
        }
        let binning = self.binning() as u16;
        let current = self.scope.as_ref().map_or(0, |s| s.scope.calibration_duration());
        let (fl, px) = (self.focal_length, self.cam.pixel_size);
        ca::sanity_message(ra_spd, dec_spd, |sid_rate| {
            let distance = calibration::calibration_distance(fl, px, binning);
            let (_, rec) =
                calibration::calibration_step_size(fl, px, binning, sid_rate, CALSTEP_DEFAULT_STEPS, dec, distance);
            (current, rec)
        })
    }

    /// The next step, as far as it goes without waiting.
    fn ca_next(&mut self) {
        loop {
            let Some(w) = self.cal_assist.as_mut() else { return };
            if w.slewing.is_some() {
                return;
            }
            let Some(step) = w.steps.pop_front() else { return };
            match step {
                Step::Status(s) => w.status = s.into(),
                Step::Slew { ra, dec, status } => {
                    if !status.is_empty() {
                        w.status = status.into();
                    }
                    // `PerformSlew`: capture stops first.
                    if self.ctrl.capture_active() {
                        self.call(|c, cx, ev| c.stop_capturing(cx, ev));
                    }
                    let started = pointing_mut(&mut self.scope, &mut self.aux, &mut self.manual_pointing)
                        .is_some_and(|p| p.can_slew_async() && p.slew_to_async(ra, dec));
                    let w = self.cal_assist.as_mut().expect("open");
                    if started {
                        w.slewing = Some(Slewing::Moving);
                    } else {
                        w.status = "Slew failed! Make sure scope is tracking at sidereal rate".into();
                        w.steps.clear();
                        w.just_slewed = false;
                        self.debug("Cal-slew: slew failed");
                    }
                }
                Step::Calibrate => {
                    let settle = SettleParams { tolerance_px: 99.0, settle_time_s: 9999, timeout_s: 9999, frames: 5 };
                    let opts = GuideOpts { force_recal: true, use_sticky_lock: false };
                    let started =
                        self.call(|c, cx, ev| can_guide(cx).is_ok() && c.guide(opts, settle, None::<Rect>, cx, ev).is_ok());
                    let w = self.cal_assist.as_mut().expect("open");
                    if started {
                        w.status = "Waiting for calibration to complete".into();
                        (w.monitoring, w.just_slewed) = (true, false);
                    } else {
                        w.status = "Calibration could not start - suspend any imaging automation apps".into();
                    }
                }
            }
        }
    }

    /// The window's timer (PHD2's 1.5 s, here the 1 s tick): a slew
    /// awaited and its pause, the current position, or the calibration
    /// followed.
    pub(super) fn cal_assistant_tick(&mut self) {
        let Some(w) = self.cal_assist.as_ref() else { return };
        let now = self.now_ms();
        match w.slewing {
            Some(Slewing::Moving) => {
                if !self.pointing().is_some_and(|p| p.is_slewing()) {
                    let w = self.cal_assist.as_mut().expect("open");
                    w.status = "Pausing...".into();
                    w.slewing = Some(Slewing::Pausing(now + ca::SLEW_SETTLE_MS));
                }
                return;
            }
            Some(Slewing::Pausing(until)) => {
                if now < until {
                    return;
                }
                let can_slew = self.ca_can_slew();
                let coords = self.pointing().and_then(|p| p.coordinates());
                let w = self.cal_assist.as_mut().expect("open");
                w.slewing = None;
                w.just_slewed = true;
                if let Some(c) = coords {
                    w.current = ca::current(c, false);
                    w.warning = if w.current.2 != w.target.east { MERIDIAN_FLIP } else { "" };
                    w.explanation = ca::explanation(c.1, can_slew);
                }
                self.ca_next();
                return;
            }
            None => {}
        }
        if w.monitoring {
            self.ca_track(self.guider.state());
            return;
        }
        // `UpdateCurrentPosition(true)`.
        let can_slew = self.ca_can_slew();
        let coords = self.pointing().and_then(|p| p.coordinates());
        let w = self.cal_assist.as_mut().expect("open");
        let Some(c) = coords else {
            (w.status, w.fatal) = ("Mount can't report its pointing position".into(), true);
            return;
        };
        w.current = ca::current(c, true);
        w.warning = if w.current.2 != w.target.east { MERIDIAN_FLIP } else { "" };
        if !w.meridian_flipping && (w.last_dec - c.1).abs() > 10.0 {
            if w.last_dec != INVALID_DEC {
                w.explanation = ca::explanation(c.1, can_slew);
            }
            w.last_dec = c.1;
        }
    }

    /// `TrackCalibration`.
    fn ca_track(&mut self, state: GuiderState) {
        use GuiderState::*;
        match state {
            Uninitialized | Selecting | Selected => {
                let w = self.cal_assist.as_mut().expect("open");
                if w.calibration_active {
                    w.status = "Calibration failed or was cancelled".into();
                    w.last_result = "Incomplete".into();
                    (w.calibration_active, w.monitoring) = (false, false);
                }
            }
            CalibratingPrimary | CalibratingSecondary => self.cal_assist.as_mut().expect("open").calibration_active = true,
            Calibrated | Guiding => {
                let eval = {
                    let Engine { scope, store, .. } = self;
                    let mut prof = store.profile();
                    match scope.as_mut().filter(|s| s.scope.is_calibrated()) {
                        Some(s) => {
                            let cal = s.scope.mount.get_last_calibration(&prof);
                            let details = s.scope.load_calibration_details(&mut prof, &mut Vec::new());
                            ca::evaluate(Some((&cal, &details)), s.scope.dec_compensation_enabled())
                        }
                        None => ca::evaluate(None, false),
                    }
                };
                for l in &eval.log {
                    self.debug(l);
                }
                let w = self.cal_assist.as_mut().expect("open");
                (w.calibration_active, w.monitoring) = (false, false);
                (w.status, w.last_result, w.verdict) = (eval.status, eval.reasons, Some(eval.verdict));
            }
            Stop => {}
        }
    }

    /// The window, for `guider_tools`.
    pub(super) fn cal_assistant_view(&self) -> Value {
        let Some(w) = self.cal_assist.as_ref() else { return Value::Null };
        json!({
            "target": {"offset": w.target.offset, "dec": w.target.dec, "east": w.target.east},
            "current": {"offset": w.current.0, "dec": w.current.1, "east": w.current.2},
            "warning": if w.slewing.is_some() { ca::WATCH_SLEWING } else { w.warning },
            "explanation": w.explanation,
            "status": w.status,
            "fatal": w.fatal,
            "slewing": w.slewing.is_some(),
            "can_slew": self.ca_can_slew(),
            "calibrating": w.monitoring,
            "sanity": w.sanity,
            "verdict": w.verdict.map(|v| format!("{v:?}")),
            "last_result": w.last_result,
        })
    }
}
