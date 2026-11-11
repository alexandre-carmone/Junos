//! The Drift Align window as the engine drives it: Drift loops, selects a
//! star, guides and turns Dec guiding off (`StartDecDrift`), the guide
//! steps feeding the graph's trend lines; Adjust goes back to looping with
//! the lock on the drifted star, full frames, and a lost star ignored; the
//! slew to the phase's location; the phase switch; close puts everything
//! back. Each step of Drift is retried after every command and frame, as
//! PHD2's `OnAppStateNotify` does.
//!
//! Ported from PHD2 src/drift_tool.cpp (DriftToolWin, UpdateModeState, OnSlew, OnDrift, OnAdjust, OnPhase, OnClose, UpdateScopeCoordinates) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::{Value, json};

use super::calibration::ProfileAccess as _;
use super::command::{CmdError, DriftAlignCmd, Reply};
use super::guider::GuiderState;
use super::log::cfmt::fmt_f;
use super::mount::GuideStepInfo;
use super::static_pa_win::pointing_mut;
use super::tools::drift_align::{self as da, Mode, Phase, Trend};
use super::Engine;

pub(super) struct DriftWin {
    phase: Phase,
    mode: Mode,
    drifting: bool,
    need_end_dec_drift: bool,
    save_sticky: bool,
    save_use_subframes: bool,
    can_slew: bool,
    site_lat: Option<f64>,
    /// The slew fields, degrees: RA offset from the meridian, Dec.
    slew: (f64, f64),
    status: String,
    trend: Trend,
}

impl Engine {
    /// One of the window's controls.
    pub(super) fn drift_align(&mut self, c: DriftAlignCmd) -> Reply {
        if !matches!(c, DriftAlignCmd::Open) && self.drift.is_none() {
            return Err(CmdError::new(1, "Drift Align tool not open"));
        }
        match c {
            DriftAlignCmd::Open => self.drift_open()?,
            DriftAlignCmd::Slew { ra, dec } => self.drift_slew(ra, dec),
            DriftAlignCmd::Drift => {
                self.debug("Drift tool: Drift");
                self.drift_mode(Mode::Drift);
            }
            DriftAlignCmd::Adjust => {
                self.debug("Drift tool: Adjust");
                self.drift_mode(Mode::Adjust);
            }
            DriftAlignCmd::Phase => {
                let w = self.drift.as_mut().expect("open");
                w.phase = if w.phase == Phase::Alt { Phase::Az } else { Phase::Alt };
                let line = format!("Drift tool: Phase {}", if w.phase == Phase::Alt { "Alt" } else { "Az" });
                self.debug(&line);
                self.drift_phase_state();
                if self.drift.as_ref().is_some_and(|w| w.mode != Mode::Idle) {
                    self.drift_mode(Mode::Idle);
                }
            }
            DriftAlignCmd::Notes(text) => {
                let key = da::notes_key(self.drift.as_ref().expect("open").phase);
                self.store.profile().set_str(key, &text);
            }
            DriftAlignCmd::Close => self.drift_close(),
        }
        Ok(json!(0))
    }

    /// `CreateDriftToolWindow` (the pixel-scale prompt is the UI's) and
    /// the window's constructor.
    fn drift_open(&mut self) -> Result<(), CmdError> {
        if self.camera.is_none() {
            return Err(CmdError::new(1, "Please connect a camera first."));
        }
        if self.drift.is_some() {
            return Ok(());
        }
        let pointing = self.pointing();
        let can_slew = pointing.is_some_and(|p| p.can_slew_async());
        let site_lat = pointing.and_then(|p| p.site_lat_long()).map(|(lat, _)| lat);
        let length = self.store.global().get_i32(da::keys::GRAPH_LENGTH, da::DEFAULT_GRAPH_LENGTH);
        // No sticky lock position while the tool runs.
        let save_sticky = self.guider.lock_pos_is_sticky();
        self.guider.set_lock_pos_is_sticky(false);
        self.drift = Some(DriftWin {
            phase: Phase::Az,
            mode: Mode::Idle,
            drifting: false,
            need_end_dec_drift: false,
            save_sticky,
            save_use_subframes: self.cam.use_subframes,
            can_slew,
            site_lat,
            slew: (0.0, 0.0),
            status: String::new(),
            trend: Trend::new(length),
        });
        self.drift_phase_state();
        self.drift_mode(Mode::Idle);
        Ok(())
    }

    /// `UpdatePhaseState`: the slew fields from the global keys.
    fn drift_phase_state(&mut self) {
        let phase = self.drift.as_ref().expect("open").phase;
        let slew = da::load_slew(&self.store.global(), phase);
        self.drift.as_mut().expect("open").slew = slew;
    }

    fn drift_status(&mut self, text: &str) {
        self.debug(&format!("Drift tool: status: {text}"));
        self.drift.as_mut().expect("open").status = text.to_string();
    }

    fn drift_ignore_lost_star(&mut self, ignore: bool) {
        let mut gev = Vec::new();
        self.guider.set_ignore_lost_star_looping(ignore, &mut gev);
        self.emit_guider_events(gev);
    }

    /// `StartLoopingInteractive`: guiding stops, looping goes on.
    fn drift_loop(&mut self) {
        self.call(|c, cx, ev| c.start_looping(cx, ev));
    }

    /// `m_mode = mode; UpdateModeState()`.
    fn drift_mode(&mut self, mode: Mode) {
        self.drift.as_mut().expect("open").mode = mode;
        self.drift_update();
    }

    /// `UpdateModeState`.
    fn drift_update(&mut self) {
        let Some(w) = self.drift.as_ref() else { return };
        let (phase, save_subframes) = (w.phase, w.save_use_subframes);
        match w.mode {
            Mode::Drift => {
                self.cam.use_subframes = save_subframes;
                self.drift_ignore_lost_star(false);
                if self.drift.as_ref().is_some_and(|w| w.drifting) {
                    return;
                }
                let mount = match (&self.ao, &self.scope) {
                    (Some(a), _) => Some((a.sg.mount().is_connected(), a.sg.mount().is_calibrated())),
                    (None, Some(s)) => Some((s.scope.mount.is_connected(), s.scope.is_calibrated())),
                    (None, None) => None,
                };
                let idle = match mount {
                    _ if !self.camera_connected() => Some("Please connect a camera and a mount"),
                    None | Some((false, _)) => Some("Please connect a camera and a mount"),
                    Some((_, false)) => Some("Please calibrate before starting drift alignment"),
                    _ => None,
                };
                if let Some(text) = idle {
                    self.drift.as_mut().expect("open").mode = Mode::Idle;
                    self.drift_idle(text);
                    return;
                }
                if !self.ctrl.capture_active() {
                    self.drift_status("Start Looping...");
                    self.drift_loop();
                    return;
                }
                match self.guider.state() {
                    GuiderState::Uninitialized | GuiderState::Calibrated | GuiderState::Selecting => {
                        if !self.guider.multistar().is_locked() {
                            self.drift_status("Auto-selecting a star");
                            // The guider's own AutoSelect, without the frame's state check.
                            self.call(|c, cx, ev| {
                                let mut gev = Vec::new();
                                let _ = cx.guider.auto_select(None, &mut cx.gcx, &mut gev);
                                c.forward(gev, cx, ev);
                            });
                        }
                    }
                    GuiderState::CalibratingPrimary | GuiderState::CalibratingSecondary => {
                        if !self.primary_mount().is_some_and(|m| m.is_calibrated()) {
                            self.drift_status("Waiting for calibration to complete...");
                        }
                    }
                    GuiderState::Selected => {
                        self.drift_status("Start guiding...");
                        // `GuideButtonClick(false)`: `StartGuiding`.
                        self.call(|c, cx, ev| c.start_guiding(cx, ev));
                    }
                    GuiderState::Guiding => {
                        if !self.drift.as_ref().expect("open").need_end_dec_drift {
                            let mut mev = Vec::new();
                            if let Some(s) = self.scope.as_mut() {
                                s.scope.start_dec_drift(&mut mev);
                            }
                            self.emit_mount_events(self.scope_role(), mev);
                            self.drift.as_mut().expect("open").need_end_dec_drift = true;
                        }
                        self.drift_status("Drifting... click Adjust when done drifting");
                        let w = self.drift.as_mut().expect("open");
                        w.trend.clear();
                        w.drifting = true;
                    }
                    GuiderState::Stop => {}
                }
            }
            Mode::Adjust => {
                self.drift.as_mut().expect("open").drifting = false;
                self.drift_status(da::adjust_status(phase));
                self.cam.use_subframes = false;
                if self.guider.is_guiding() {
                    self.drift_loop();
                    // The lock on the drifted star: the polar align circle's centre.
                    if let Some(pos) = self.guider.current_position() {
                        self.call(|c, cx, ev| {
                            let mut gev = Vec::new();
                            cx.guider.set_lock_position(pos, &mut cx.gcx, &mut gev);
                            c.forward(gev, cx, ev);
                        });
                    }
                    self.drift_ignore_lost_star(true);
                }
            }
            Mode::Idle => self.drift_idle(""),
        }
    }

    /// `UpdateModeState`'s idle branch.
    fn drift_idle(&mut self, status: &str) {
        let save = self.drift.as_ref().expect("open").save_use_subframes;
        self.drift.as_mut().expect("open").drifting = false;
        self.drift_status(status);
        self.cam.use_subframes = save;
        self.drift_ignore_lost_star(false);
        if self.guider.is_guiding() {
            self.drift_loop();
        }
    }

    /// `OnAppStateNotify`, after each command and frame: Drift goes on.
    pub(super) fn drift_check(&mut self) {
        if self.drift.as_ref().is_some_and(|w| w.mode == Mode::Drift && !w.drifting) {
            self.drift_update();
        }
    }

    /// A guide step: the graph's point.
    pub(super) fn drift_guide_step(&mut self, step: &GuideStepInfo) {
        let now = self.now_ms();
        if let Some(w) = self.drift.as_mut().filter(|w| w.drifting && !step.is_ao) {
            w.trend.push(now, step.mount_offset.x, step.mount_offset.y);
        }
    }

    /// `OnSlew`, with the fields; they are saved after the slew starts.
    fn drift_slew(&mut self, ra: Option<f64>, dec: Option<f64>) {
        let w = self.drift.as_mut().expect("open");
        let (ra, dec) = (ra.map_or(w.slew.0, f64::floor), dec.map_or(w.slew.1, f64::floor));
        w.slew = (ra, dec);
        let phase = w.phase;
        let Some((cur_ra, cur_dec, st)) = self.pointing().and_then(|p| p.coordinates()) else {
            self.debug("Drift tool: slew failed to get scope coordinates");
            return;
        };
        let slew_ra = da::slew_ra(st, ra);
        self.debug(&format!(
            "Drift tool: slew from ra {}, dec {} to ra {}, dec {}",
            fmt_f(cur_ra, 2),
            fmt_f(cur_dec, 1),
            fmt_f(slew_ra, 2),
            fmt_f(dec, 1)
        ));
        let started = pointing_mut(&mut self.scope, &mut self.aux, &mut self.manual_pointing)
            .is_some_and(|p| p.can_slew_async() && p.slew_to_async(slew_ra, dec));
        if !started {
            self.drift_status("Slew failed! Make sure scope is tracking at sidereal rate");
        }
        da::save_slew(&mut self.store.global(), phase, ra, dec);
    }

    /// `OnClose`.
    fn drift_close(&mut self) {
        self.debug("Drift tool: Close DriftTool");
        let Some(w) = self.drift.take() else { return };
        if w.need_end_dec_drift {
            let mut mev = Vec::new();
            if let Some(s) = self.scope.as_mut() {
                s.scope.end_dec_drift(&mut mev);
            }
            self.emit_mount_events(self.scope_role(), mev);
        }
        if w.save_sticky {
            self.guider.set_lock_pos_is_sticky(true);
        }
        self.cam.use_subframes = w.save_use_subframes;
        self.drift_ignore_lost_star(false);
    }

    /// The window, for `guider_pa`'s `drift_align`.
    pub(super) fn drift_align_view(&self) -> Value {
        let Some(w) = self.drift.as_ref() else { return Value::Null };
        let (title, instructions) = da::texts(w.phase);
        let coords = self.pointing().and_then(|p| p.coordinates());
        let current = coords.map(|(ra, dec, st)| da::current(ra, dec, st));
        let correction = match (current.as_ref(), coords, w.site_lat) {
            (Some(c), Some((_, dec, _)), Some(lat)) => da::circle_correction(w.phase, c.0, dec, lat),
            _ => 1.0,
        };
        let dec = self.pointing().and_then(|p| p.declination_radians());
        let lines = w.trend.lines(self.pixel_scale(), dec);
        // Round the star while drifting, round the lock while adjusting.
        let centre = if self.guider.is_calibrating_or_guiding() {
            self.guider.current_position()
        } else {
            self.guider.lock_position()
        };
        json!({
            "phase": format!("{:?}", w.phase),
            "mode": format!("{:?}", w.mode),
            "drifting": w.drifting,
            "title": title,
            "instructions": instructions,
            "status": w.status,
            "can_slew": w.can_slew,
            "slew": {"ra": w.slew.0, "dec": w.slew.1},
            "current": current.map(|c| json!({"ra": c.1, "dec": c.2})),
            "trend": lines.map(|l| json!({
                "points": l.points,
                "ra": [l.ra.0, l.ra.1],
                "dec": [l.dec.0, l.dec.1],
                "error_arcmin": l.error_arcmin,
            })),
            "circle": lines.and_then(|l| l.radius_px).zip(centre).map(|(r, c)| json!({
                "x": c.x, "y": c.y, "radius": r, "correction": correction, "error_px": r * correction,
            })),
        })
    }
}
