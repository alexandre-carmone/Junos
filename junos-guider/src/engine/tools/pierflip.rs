//! The Meridian Flip Calibration Tool: one calibration on each side of
//! the pier, near the meridian and the equator; the change of the Dec
//! calibration angle tells whether 'Reverse Dec output after meridian flip'
//! is needed.
//!
//! The engine starts the calibrations, feeds the guider's state after
//! each frame and command ([`PierFlip::guiding_state`]), and applies the
//! result.
//!
//! Ported from PHD2 `src/pierflip_tool.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::{DEC_COMP_LIMIT, degrees, norm, norm_angle, radians};
use crate::engine::log::cfmt::fmt_f;
use crate::engine::traits::PierSide;

/// `enum State`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Intro = 0,
    Slew1,
    Calibrate1,
    Slew2,
    Calibrate2,
    Done,
}

/// `CanRunTool`: (the debug line, the error).
pub fn can_run(ao: bool, scope: bool, dec_guiding_off: bool) -> Result<(), (&'static str, &'static str)> {
    if ao {
        return Err((
            "PFT: called when AO present",
            "The meridian flip calibration tool requires an equipment profile without an AO",
        ));
    }
    if !scope {
        return Err((
            "PFT: called when no mount present",
            "The meridian flip calibration tool requires a mount. Click the Connect Equipment button to select your \
             mount.",
        ));
    }
    if dec_guiding_off {
        return Err((
            "PFT: called when dec guiding disabled",
            "The meridian flip calibration tool cannnot be run with Declination guiding disabled. If your mount can \
             guide in Declination, set your Dec guide mode to Auto and try again.",
        ));
    }
    Ok(())
}

/// What the guider reports to [`PierFlip::guiding_state`].
#[derive(Clone, Copy, Debug)]
pub struct GuidingState {
    pub calibrating_or_guiding: bool,
    pub calibrating: bool,
    /// `TheScope()->IsCalibrated() && !PhdController::IsSettling()`.
    pub calibrated_settled: bool,
    /// `TheScope()->MountCal()`: xAngle, yAngle (radians), pier side.
    pub cal: (f64, f64, PierSide),
}

/// The tool's window state.
#[derive(Clone, Debug)]
pub struct PierFlip {
    state: State,
    calibration_started: bool,
    first: (f64, f64, PierSide),
    result: bool,
    error: String,
    instructions: String,
    status: String,
    log: Vec<String>,
}

impl Default for PierFlip {
    fn default() -> Self {
        Self::new()
    }
}

impl PierFlip {
    /// `ShowPierFlipCalTool`: the window opens on the introduction.
    pub fn new() -> Self {
        let mut t = Self {
            state: State::Intro,
            calibration_started: false,
            first: (0.0, 0.0, PierSide::Unknown),
            result: false,
            error: String::new(),
            instructions: String::new(),
            status: String::new(),
            log: vec!["PFT: opened".into()],
        };
        t.set_state(State::Intro, PierSide::Unknown);
        t
    }

    /// `SetState`; `pier` is the pointing source's side, for the status.
    fn set_state(&mut self, state: State, pier: PierSide) {
        self.log.push(format!("PFT: set state {}", state as i32));
        self.state = state;
        self.calibration_started = false;
        const SLEW: &str = "Point the telescope in the direction of the intersection of the meridian and the \
                            celestial equator, near Hour Angle = 0 and Declination = 0.\n\nClick Calibrate when ready.";
        let calibrating = || format!("Calibrating on the {} side of pier", pier.label());
        self.instructions = match state {
            State::Intro => "This tool will automatically determine the correct value for the setting 'Reverse Dec \
                             output after meridian flip'.\n\nThe procedure requires two calibrations -- one with the \
                             telescope on the East side of the pier, and one on the West. You will be instructed to \
                             slew the telescope when needed.\n\nClick Next to begin"
                .into(),
            State::Slew1 => {
                self.status.clear();
                SLEW.into()
            }
            State::Calibrate1 => {
                self.status = calibrating();
                "Wait for the first calibration to complete.".into()
            }
            State::Slew2 => {
                self.status.clear();
                let other = if self.first.2 == PierSide::East { PierSide::West } else { PierSide::East };
                format!(
                    "Slew the telescope to force a meridian flip - the scope should move to the {} side of the pier, \
                     still pointing near Dec = 0.\n\n{SLEW}",
                    other.label()
                )
            }
            State::Calibrate2 => {
                self.status = calibrating();
                "Wait for the second calibration to complete.".into()
            }
            State::Done => {
                self.status.clear();
                if self.error.is_empty() {
                    format!(
                        "Meridian flip calibration completed sucessfully.\n\nThe correct setting for 'Reverse Dec \
                         output after meridian flip' for this mount is: {}\n\nClick Apply to accept the setting",
                        if self.result { "enabled" } else { "disabled" }
                    )
                } else {
                    format!(
                        "Meridian flip calibration failed.\n\n{}\n\nResolve any calibration issues and try again",
                        self.error
                    )
                }
            }
        };
    }

    /// "Start over".
    pub fn restart(&mut self) {
        self.set_state(State::Slew1, PierSide::Unknown);
    }

    /// Next / Calibrate / Apply (`OnNextClick`): `start` starts a forced
    /// calibration (PHD2's error text on failure). Returns the setting to
    /// apply when the button was Apply; the window then closes.
    pub fn next(&mut self, pier: PierSide, start: impl FnOnce() -> Result<(), String>) -> Option<bool> {
        match self.state {
            State::Intro => self.set_state(State::Slew1, pier),
            State::Slew1 | State::Slew2 => match start() {
                Ok(()) => {
                    let next = if self.state == State::Slew1 { State::Calibrate1 } else { State::Calibrate2 };
                    self.set_state(next, pier);
                }
                Err(e) => {
                    self.log.push(format!("PFT: start calibration failed: {e}"));
                    self.status = e;
                }
            },
            State::Done => {
                self.log.push(format!("PFT: apply result: {}", self.result as i32));
                return Some(self.result);
            }
            State::Calibrate1 | State::Calibrate2 => {}
        }
        None
    }

    /// `OnGuidingStateUpdated`. Returns true when looping is to start
    /// (after the first calibration).
    pub fn guiding_state(&mut self, g: GuidingState, pier: PierSide) -> bool {
        if self.state != State::Calibrate1 && self.state != State::Calibrate2 {
            return false;
        }
        if self.calibration_started && !g.calibrating_or_guiding {
            self.log.push("PFT: calibration stopped".into());
            let back = if self.state == State::Calibrate1 { State::Slew1 } else { State::Slew2 };
            self.set_state(back, pier);
            return false;
        }
        if g.calibrating && !self.calibration_started {
            self.log.push("PFT: calibration started".into());
            self.calibration_started = true;
        }
        if !self.calibration_started || !g.calibrated_settled {
            return false;
        }
        if self.state == State::Calibrate1 {
            self.log.push("PFT: calibrate1 done, start looping".into());
            self.first = g.cal;
            self.set_state(State::Slew2, pier);
            return true;
        }
        let dx = norm_angle(self.first.0 - g.cal.0);
        self.log.push(format!("PFT: deltaRA = {} deg", fmt_f(degrees(dx), 1)));
        self.error.clear();
        if dx.abs() < radians(150.0) {
            self.log.push("PFT: deltaRA too small!".into());
            self.error = "The RA calibration angles varied by an unexpected amount.".into();
        } else {
            let dy = norm_angle(self.first.1 - g.cal.1);
            self.log.push(format!("PFT: deltaDec = {} deg", fmt_f(degrees(dy), 1)));
            if dy.abs() < radians(30.0) {
                self.result = false;
            } else if dy.abs() > radians(150.0) {
                self.result = true;
            } else {
                self.log.push("PFT: deltaDec not definitive!".into());
                self.error = "The declination calibration angles varied by an unexpected amount.".into();
            }
        }
        self.set_state(State::Done, pier);
        false
    }

    /// `DoOnTimer`: the Dec and hour angle fields (`Unknown` without a
    /// position), their colours (`green`, `yellow`, `red`), and whether
    /// Calibrate is enabled (`None`: left as it is).
    pub fn position(&self, coords: Option<(f64, f64, f64)>, pier: PierSide) -> PositionView {
        let (dec, ha, dec_text, ha_text) = match coords {
            Some((ra, dec, lst)) => {
                let ha = norm(lst - ra, -12.0, 12.0);
                let sign = |v: f64| if v >= 0.0 { "+" } else { "" };
                (dec, ha, format!("{}{}°", sign(dec), fmt_f(dec, 1)), format!("{}{}h", sign(ha), fmt_f(ha, 2)))
            }
            None => (999.0, 999.0, "Unknown".into(), "Unknown".into()),
        };
        let limit = degrees(DEC_COMP_LIMIT);
        let dec_colour = if dec.abs() < 30.0 {
            "green"
        } else if dec.abs() < limit {
            "yellow"
        } else {
            "red"
        };
        let ha_colour = if ha.abs() < 2.5 { "green" } else { "yellow" };
        let (enable, pier_colour) = match self.state {
            State::Slew1 => (Some(pier != PierSide::Unknown && dec.abs() < limit), None),
            State::Slew2 => {
                let other = pier != PierSide::Unknown && pier != self.first.2;
                (Some(other && dec.abs() < limit), Some(if other { "green" } else { "red" }))
            }
            _ => (None, None),
        };
        PositionView { dec: dec_text, ha: ha_text, dec_colour, ha_colour, pier: pier.label(), pier_colour, enable }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }
}

/// The window's position fields.
#[derive(Clone, Debug, PartialEq)]
pub struct PositionView {
    pub dec: String,
    pub ha: String,
    pub dec_colour: &'static str,
    pub ha_colour: &'static str,
    pub pier: &'static str,
    pub pier_colour: Option<&'static str>,
    pub enable: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(x: f64, y: f64, pier: PierSide, done: bool) -> GuidingState {
        GuidingState {
            calibrating_or_guiding: true,
            calibrating: !done,
            calibrated_settled: done,
            cal: (radians(x), radians(y), pier),
        }
    }

    fn run(x2: f64, y2: f64) -> PierFlip {
        let mut t = PierFlip::new();
        t.next(PierSide::East, || Ok(()));
        t.next(PierSide::East, || Ok(()));
        assert_eq!(t.state(), State::Calibrate1);
        assert_eq!(t.status(), "Calibrating on the East side of pier");
        t.guiding_state(g(10.0, 100.0, PierSide::East, false), PierSide::East);
        assert!(t.guiding_state(g(10.0, 100.0, PierSide::East, true), PierSide::East));
        assert_eq!(t.state(), State::Slew2);
        let west = "Slew the telescope to force a meridian flip - the scope should move to the West";
        assert!(t.instructions().starts_with(west));
        t.next(PierSide::West, || Ok(()));
        t.guiding_state(g(x2, y2, PierSide::West, false), PierSide::West);
        t.guiding_state(g(x2, y2, PierSide::West, true), PierSide::West);
        t
    }

    #[test]
    fn two_calibrations_give_the_setting() {
        let mut t = run(-170.0, -80.0);
        assert_eq!(t.state(), State::Done);
        assert!(t.instructions().contains("for this mount is: enabled"));
        assert_eq!(t.next(PierSide::West, || Ok(())), Some(true));
        let log = t.take_log();
        assert!(log.contains(&"PFT: deltaRA = -180.0 deg".to_string()), "{log:?}");
        assert!(log.contains(&"PFT: apply result: 1".to_string()));
        assert!(run(-170.0, 100.0).instructions().contains("is: disabled"));
        assert!(run(-170.0, 10.0).instructions().contains("declination calibration angles varied"));
        assert!(run(40.0, 100.0).instructions().contains("RA calibration angles varied"));
    }

    #[test]
    fn refusals_failures_and_position() {
        assert_eq!(can_run(true, true, false).unwrap_err().0, "PFT: called when AO present");
        let no_mount = can_run(false, false, false).unwrap_err().1;
        assert!(no_mount.starts_with("The meridian flip calibration tool requires a mount."));
        assert!(can_run(false, true, true).is_err());
        let mut t = PierFlip::new();
        t.next(PierSide::East, || Ok(()));
        t.next(PierSide::East, || Err("cannot guide".into()));
        assert_eq!((t.state(), t.status()), (State::Slew1, "cannot guide"));
        let p = t.position(Some((5.0, -12.25, 5.5)), PierSide::East);
        assert_eq!((p.dec.as_str(), p.ha.as_str()), ("-12.2°", "+0.50h"));
        assert_eq!((p.dec_colour, p.ha_colour, p.enable), ("green", "green", Some(true)));
        assert_eq!(t.position(None, PierSide::Unknown).enable, Some(false));
        // A calibration that stops goes back to the slew.
        t.next(PierSide::East, || Ok(()));
        t.guiding_state(g(0.0, 90.0, PierSide::East, false), PierSide::East);
        let stopped = GuidingState { calibrating_or_guiding: false, ..g(0.0, 90.0, PierSide::East, false) };
        t.guiding_state(stopped, PierSide::East);
        assert_eq!(t.state(), State::Slew1);
    }
}
