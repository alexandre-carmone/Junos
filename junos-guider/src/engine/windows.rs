//! PHD2's tool windows as the engine drives them: the Guiding Assistant
//! (its measurements fed from the guide steps, its backlash test from the
//! guider's measurement mode, its countdown from the 1 s tick, and what it
//! asks of the rest of PHD2 carried out), the Darks dialog's builds (a
//! dark frame per [`Engine::step`] while one runs, then the files), and the
//! Polar Drift tool (its samples from the selected star's frames).
//!
//! Ported from PHD2 src/guiding_assistant.cpp (the window's calls into the frame), src/darks_dialog.cpp (OnStart, CreateMasterDarkFrame), src/myframe_events.cpp (OnDark), src/polardrift_toolwin.cpp (OnStart, OnClose, UpdateState) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::json;

use super::algo::Axis;
use super::assistant::{self, ApplyCx, GaCx, GaEvent, GuidingAssistant, RecEnv, RecKind, StepEnv};
use super::backlash::BltStep;
use super::calibration::ProfileAccess;
use super::camera::{CaptureKind, capture_frame};
use super::command::{CmdError, DarksBuildKind, Reply};
use super::darks::{self, COVER_GUIDE_SCOPE, DarksBuild, DarksBuildState, build_filtered_dark};
use super::event::{DarksView, GaView, PolarDriftView};
use super::guider::{MountRole, PaTools, ScheduledMove};
use super::mount::{AlertKind, GuideOutputs, GuideStepInfo};
use super::tools::{self, polar_drift::PolarDrift};
use super::traits::Point;
use super::{Engine, Pending};

/// A dark-library or defect-map build under way.
pub(super) struct DarksJob {
    build: DarksBuild,
    note: String,
    defect_map: bool,
    new_library: bool,
}

impl Engine {
    // ------------------------------------------------- the Guiding Assistant

    /// pMount's and pSecondaryMount's guide outputs.
    fn guide_outputs(&self) -> GuideOutputs {
        match (&self.ao, &self.scope) {
            (Some(a), s) => GuideOutputs {
                primary: Some(a.sg.mount().guiding_enabled()),
                secondary: s.as_ref().map(|s| s.scope.mount.guiding_enabled()),
            },
            (None, Some(s)) => GuideOutputs { primary: Some(s.scope.mount.guiding_enabled()), secondary: None },
            (None, None) => GuideOutputs::default(),
        }
    }

    /// Turn the guide outputs given on or off.
    fn set_guide_outputs(&mut self, o: GuideOutputs) {
        if let Some(on) = o.primary {
            self.set_primary_guiding_enabled(on);
        }
        if let (Some(on), true) = (o.secondary, self.ao.is_some()) {
            let algo = self.algo_ctx();
            let mut mev = Vec::new();
            if let Some(s) = self.scope.as_mut() {
                s.scope.set_guiding_enabled(on, &algo, &mut mev);
            }
            self.emit_mount_events(MountRole::Secondary, mev);
        }
    }

    /// What the recommendations read from the camera, the guider and
    /// pMount.
    fn rec_env(&self) -> RecEnv {
        let primary = match (&self.ao, &self.scope) {
            (Some(a), _) => Some(a.sg.mount()),
            (None, Some(s)) => Some(&s.scope.mount),
            (None, None) => None,
        };
        let min_move = |axis: Axis| primary.map_or(-1.0, |m| m.algo(axis).min_move());
        RecEnv {
            pixel_scale: self.pixel_scale(),
            focal_length: self.focal_length,
            pixel_size: self.cam.pixel_size,
            binning: self.binning() as u32,
            max_binning: self.camera.as_ref().map_or(1, |c| c.max_binning()) as u32,
            hp_encoders: self.ao.is_none() && self.scope.as_ref().is_some_and(|s| s.scope.has_hp_encoders()),
            ra_min_move: min_move(Axis::X),
            dec_min_move: min_move(Axis::Y),
            dec_algo_class: primary.map_or("", |m| m.algo(Axis::Y).class_name()),
            star_hfd: self.guider.primary_star().hfd,
        }
    }

    /// A call on the open assistant with its [`GaCx`]; `None` when it is
    /// not open. Its events are carried out once it returns.
    fn with_ga<R>(
        &mut self,
        f: impl FnOnce(&mut GuidingAssistant, &mut GaCx<'_, dyn ProfileAccess + '_>, &mut Vec<GaEvent>) -> R,
    ) -> Option<R> {
        let rec = self.rec_env();
        let frame_size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
        let (now_ms, star) = (self.now_ms(), self.guider.current_position());
        let (max_move_px, use_subframes) = (self.guider.max_move_pixels(), self.cam.use_subframes);
        let mut ev = Vec::new();
        let r = {
            let Engine { ga, scope, store, .. } = self;
            let ga = ga.as_mut()?;
            let mut prof = store.profile();
            let mut cx = GaCx {
                rec: &rec,
                profile: &mut prof as &mut dyn ProfileAccess,
                scope: scope.as_mut().map(|s| &mut s.scope),
                frame_size,
                max_move_px,
                now_ms,
                star,
                use_subframes,
            };
            f(ga, &mut cx, &mut ev)
        };
        self.emit_ga(ev);
        Some(r)
    }

    /// `GuidingAsstWin()`: the window opens (variable delay off), and
    /// starts measuring at once when guiding.
    pub(super) fn ga_open(&mut self) -> Reply {
        if self.ga.is_some() {
            return Ok(json!(0));
        }
        let guiding = self.guider.is_guiding();
        let blt_cal = match &self.scope {
            Some(s) => Some(s.scope.mount.get_last_calibration(&self.store.profile())),
            None => None,
        };
        let hp = self.ao.is_none() && self.scope.as_ref().is_some_and(|s| s.scope.has_hp_encoders());
        let v = self.ctrl.var_delay();
        let var = assistant::VarDelayCfg { enabled: v.enabled, short_ms: v.short_ms, long_ms: v.long_ms };
        let mut ev = Vec::new();
        self.ga = Some(GuidingAssistant::new(
            blt_cal.as_ref(),
            hp,
            self.guider.multistar().cfg.multi_star,
            var,
            guiding,
            &mut ev,
        ));
        self.emit_ga(ev);
        if guiding {
            self.ga_start()?;
        }
        Ok(json!(0))
    }

    /// Start (`OnStart`): refused unless guiding and idle.
    pub(super) fn ga_start(&mut self) -> Reply {
        let guiding = self.guider.is_guiding();
        let exposure = self.ctrl.requested_exposure_duration(self.camera_connected());
        let outputs = self.guide_outputs();
        let (wall, now_ms) = (self.wall(), self.now_ms());
        let Some(ga) = self.ga.as_mut() else { return Err(CmdError::new(1, "Guiding Assistant not open")) };
        let mut ev = Vec::new();
        let started = ga.start(guiding, exposure, outputs, wall, now_ms, &mut ev);
        self.emit_ga(ev);
        if started { Ok(json!(0)) } else { Err(CmdError::new(1, "cannot start the Guiding Assistant")) }
    }

    /// The rest of the window's controls.
    pub(super) fn ga_command(&mut self, cmd: GaCommand) -> Reply {
        if self.ga.is_none() {
            return Err(CmdError::new(1, "Guiding Assistant not open"));
        }
        match cmd {
            GaCommand::Stop => {
                self.with_ga(|ga, cx, ev| ga.stop(cx, ev));
            }
            GaCommand::EndWait(accept) => {
                self.with_ga(|ga, cx, ev| ga.end_wait(accept, cx, ev));
            }
            GaCommand::MeasureBacklash(on) => {
                if let Some(ga) = self.ga.as_mut() {
                    ga.set_measure_backlash(on);
                }
            }
            GaCommand::Apply(kind) => {
                if !self.ga_apply(kind) {
                    return Err(CmdError::new(1, "could not apply the recommendation"));
                }
            }
            GaCommand::Review(ts) => {
                let Engine { ga, store, .. } = self;
                if let Some(ga) = ga.as_mut() {
                    ga.review(&store.profile(), &ts);
                }
            }
            GaCommand::Close => {
                let mut ev = Vec::new();
                if let Some(mut ga) = self.ga.take() {
                    ga.close(&mut ev);
                }
                self.emit_ga(ev);
            }
        }
        Ok(json!(0))
    }

    /// An Apply button.
    fn ga_apply(&mut self, kind: RecKind) -> bool {
        let algo = self.algo_ctx();
        let mut ev = Vec::new();
        let applied = {
            let Engine { ga, scope, ao, store, .. } = self;
            let Some(ga) = ga.as_mut() else { return false };
            let mut prof = store.profile();
            let cx = ApplyCx {
                scope: scope.as_mut().map(|s| &mut s.scope),
                ao: ao.as_mut().map(|a| a.sg.mount_mut()),
                profile: &mut prof as &mut dyn ProfileAccess,
                algo: &algo,
            };
            ga.apply(kind, cx, &mut ev)
        };
        self.emit_ga(ev);
        applied
    }

    /// The 1 s tick: the countdown.
    pub(super) fn ga_tick(&mut self) {
        self.with_ga(|ga, cx, ev| ga.tick(cx, ev));
    }

    /// A guide step logged (`GuidingAssistant::NotifyGuideStep`); the
    /// assistant counts the graphed ones.
    pub(super) fn ga_guide_step(&mut self, step: &GuideStepInfo) {
        if !self.ga.as_ref().is_some_and(|g| g.is_measuring()) {
            return;
        }
        let exposure_ms = self.ctrl.requested_exposure_duration(self.camera_connected());
        let (now_ms, pixel_scale) = (self.now_ms(), self.pixel_scale());
        let dec_rad = self.pointing().and_then(|p| p.declination_radians());
        let mut ev = Vec::new();
        {
            let Engine { ga, scope, ao, .. } = self;
            // `TheScope()`'s calibration turns an AO step into scope axes.
            let ao_scope_cal = match (ao.is_some(), scope.as_ref()) {
                (true, Some(s)) => Some(s.scope.mount.mount_cal()),
                _ => None,
            };
            let env = StepEnv { now_ms, exposure_ms, pixel_scale, dec_rad, ao_scope_cal };
            if let Some(ga) = ga.as_mut() {
                ga.guide_step(step, &env, &mut ev);
            }
        }
        self.emit_ga(ev);
    }

    /// The guider's measurement-mode frame, the star there; `None` when
    /// guiding ended during the backlash test.
    pub(super) fn ga_backlash(&mut self, star: Option<Point>) {
        self.with_ga(|ga, cx, ev| match star {
            Some(p) => ga.backlash_step(p, cx, ev),
            None => ga.backlash_error(cx, ev),
        });
    }

    /// `OnAppStateNotify`: guiding started or stopped.
    pub(super) fn ga_app_state(&mut self, guiding: bool) {
        let mut ev = Vec::new();
        if let Some(ga) = self.ga.as_mut() {
            ga.app_state_changed(guiding, &mut ev);
        }
        self.emit_ga(ev);
    }

    /// What the assistant asks of the rest of PHD2, in its order.
    fn emit_ga(&mut self, ev: Vec<GaEvent>) {
        for e in ev {
            match e {
                GaEvent::Debug(l) => self.debug(&l),
                GaEvent::Result(msg) => {
                    let line = self.glog.ga_result(&msg);
                    if let Some(l) = line {
                        self.events.push(super::EngineEvent::GuideLogLine(l));
                    }
                }
                GaEvent::Completed => self.glog.ga_completed(),
                GaEvent::Mount(m) => self.emit_mount_events(MountRole::Primary, vec![m]),
                GaEvent::GuideOutputs(o) => self.set_guide_outputs(o),
                GaEvent::MultiStar(on) => {
                    let upd = self.call(|c, cx, ev| {
                        let mut gev = Vec::new();
                        let upd = cx.guider.set_multi_star_mode(on, &mut cx.gcx, &mut gev);
                        c.forward(gev, cx, ev);
                        upd
                    });
                    self.queue_moves(&upd.moves);
                }
                GaEvent::VariableDelay(v) => {
                    let mut cev = Vec::new();
                    let mut prof = self.store.profile();
                    self.ctrl.set_variable_delay_config(v.enabled, v.short_ms, v.long_ms, &mut prof, &mut cev);
                    drop(prof);
                    self.emit_control(cev);
                }
                GaEvent::UseSubframes(on) => self.cam.use_subframes = on,
                GaEvent::Backlash(step) => self.backlash_step(step),
            }
        }
    }

    /// A backlash-tool step: its lines, the measurement mode (the scope's
    /// guide output back on, its line before the step's last), its pulse
    /// on `TheScope()` (`ScheduleAxisMove`, a calibration move).
    fn backlash_step(&mut self, step: BltStep) {
        let n = step.log.len();
        for (i, l) in step.log.iter().enumerate() {
            if step.enable_measurement && i + 1 == n {
                self.enable_measurement();
            }
            self.debug(l);
        }
        if step.enable_measurement && n == 0 {
            self.enable_measurement();
        }
        if step.cleanup {
            self.guider.enable_measurement_mode(false);
        }
        if let Some((dir, amount)) = step.pulse {
            let role = self.scope_role();
            self.cancel.reset();
            self.moves.push_back(Pending::Move(ScheduledMove::Calibration { role, dir, amount }));
        }
    }

    /// `Scope::SetGuidingEnabled(true)`, then `EnableMeasurementMode(true)`.
    fn enable_measurement(&mut self) {
        let algo = self.algo_ctx();
        let mut mev = Vec::new();
        if let Some(s) = self.scope.as_mut() {
            s.scope.set_guiding_enabled(true, &algo, &mut mev);
        }
        let role = self.scope_role();
        self.emit_mount_events(role, mev);
        self.guider.enable_measurement_mode(true);
    }

    pub(super) fn ga_view(&self) -> Option<GaView> {
        self.ga.as_ref().map(|g| GaView {
            state: g.state(),
            instructions: g.instructions().to_string(),
            status: g.status().to_string(),
            measuring: g.is_measuring(),
            measuring_backlash: g.is_measuring_backlash(),
            wait: g.wait(),
            recommendations: g.recommendations().to_vec(),
            details: g.details().clone(),
        })
    }

    // ------------------------------------------------------------ the darks

    /// The Darks dialog's Start (`OnDark`, `DarksDialog::OnStart`): the
    /// camera's shutter closed (or the "Cover guide scope" box), then a
    /// dark frame per step. Refused while capturing.
    pub(super) fn darks_build(&mut self, kind: DarksBuildKind) -> Reply {
        if !self.camera_connected() {
            return Err(CmdError::new(1, "Please connect to a camera first"));
        }
        if self.ctrl.capture_active() || self.darks_job.is_some() {
            return Err(CmdError::new(1, "cannot build darks while capturing"));
        }
        let job = match kind {
            DarksBuildKind::Library { exposures_ms, count, new_library, note } => DarksJob {
                build: DarksBuild::dark_library(&mut self.darks, exposures_ms, count, new_library),
                note,
                defect_map: false,
                new_library,
            },
            DarksBuildKind::DefectMap { exp_s, count, note } => {
                DarksJob { build: DarksBuild::defect_map(exp_s, count), note, defect_map: true, new_library: false }
            }
        };
        if !self.camera.as_ref().is_some_and(|c| c.has_shutter()) {
            self.events.push(super::EngineEvent::Alert {
                msg: COVER_GUIDE_SCOPE.into(),
                kind: AlertKind::Info,
                button: None,
                dont_show: None,
            });
        }
        self.cam.shutter_closed = true;
        // The dialog captures outside the worker thread: a stop the loop
        // left behind does not reach it.
        self.cancel.reset();
        self.status(job.build.status());
        self.darks_job = Some(job);
        Ok(json!(0))
    }

    /// One dark frame (`CreateMasterDarkFrame`'s loop), or the end of the
    /// build.
    pub(super) fn darks_step(&mut self) {
        let Some(exp) = self.darks_job.as_ref().and_then(|j| j.build.next_capture()) else {
            self.darks_end();
            return;
        };
        let res = match self.camera.as_deref_mut() {
            Some(camera) => {
                capture_frame(camera, &self.cam, exp as u32, CaptureKind::Dark, None, &self.cancel, &*self.clock)
            }
            None => Err(super::camera::CaptureError::Failed(None)),
        };
        self.drain_camera_debug();
        let requested = self.ctrl.requested_exposure_duration(self.camera_connected());
        match res {
            Ok(img) => {
                let Engine { darks_job, darks, .. } = self;
                if let Some(job) = darks_job.as_mut() {
                    job.build.add_frame(darks, img);
                }
            }
            // The daemon cut the wait short for a Stop.
            Err(super::camera::CaptureError::Interrupted) => {
                if let Some(job) = self.darks_job.as_mut() {
                    job.build.cancel();
                }
            }
            Err(e) => {
                if let Some((msg, _)) = e.alert(requested, &self.cam) {
                    self.alert(msg, AlertKind::Warning);
                }
                if let Some(job) = self.darks_job.as_mut() {
                    job.build.capture_failed();
                }
            }
        }
        let status = self.darks_job.as_ref().map(|j| j.build.status().to_string()).unwrap_or_default();
        self.status(&status);
        if self.darks_job.as_ref().is_some_and(|j| j.build.state() != DarksBuildState::Running) {
            self.darks_end();
        }
    }

    /// Stop (`OnStop`): the build ends now; the frame in flight is not
    /// wanted.
    pub(super) fn darks_stop(&mut self) -> Reply {
        let Some(job) = self.darks_job.as_mut() else { return Err(CmdError::new(1, "no dark build in progress")) };
        job.build.cancel();
        self.darks_end();
        Ok(json!(0))
    }

    /// The end of `OnStart`, and `OnDark`'s dark selection once the dialog
    /// closes.
    fn darks_end(&mut self) {
        let Some(mut job) = self.darks_job.take() else { return };
        let state = job.build.state();
        match (state, job.defect_map) {
            (DarksBuildState::Done, false) => {
                let bytes = self.darks.to_fits(&job.note);
                let path = self.dark_lib_path();
                self.debug("saving dark library");
                if self.files.write(&path, &bytes).is_err() {
                    self.alert(format!("Error saving darks FITS file {path}"), AlertKind::Warning);
                }
                self.load_dark_handler(true);
                self.debug(if job.new_library {
                    "Dark library - new dark lib created from scratch."
                } else {
                    "Dark library - dark lib modified/extended."
                });
            }
            (DarksBuildState::Done, true) => {
                if let Some(master) = job.build.take_master() {
                    self.debug("Starting construction of filtered master dark file");
                    let filtered = build_filtered_dark(&master);
                    self.debug("Completed construction of filtered master dark file");
                    let (inst, id) = (self.cfg.instance as i32, self.store.current_profile_id());
                    let dir = self.cfg.darks_dir.clone();
                    let master_path = format!("{dir}/{}", darks::defect_map_master_file_name(inst, id));
                    let filter_path = format!("{dir}/{}", darks::defect_map_filter_file_name(inst, id));
                    for (img, path) in [(&master, master_path), (&filtered, filter_path)] {
                        if self.save_fits_note(img, &path, &job.note).is_err() {
                            self.debug(&format!("cannot save {path}"));
                        }
                    }
                }
            }
            (_, false) => {
                let size = self.camera.as_ref().map_or((0, 0), |c| c.frame_size());
                if self.dark_lib_file_usable(size) {
                    let restored = self.load_dark_handler(true);
                    self.debug(if restored {
                        "Dark library abort, dark library restored."
                    } else {
                        "Dark library abort, dark library still invalid."
                    });
                }
            }
            (_, true) => {}
        }
        self.set_dark_menu_state();
        if matches!(state, DarksBuildState::Done | DarksBuildState::Failed) {
            self.cam.shutter_closed = false;
        }
        let has_shutter = self.camera.as_ref().is_some_and(|c| c.has_shutter());
        if let Some(msg) = job.build.wrapup(has_shutter) {
            self.events.push(super::EngineEvent::Alert { msg, kind: AlertKind::Info, button: None, dont_show: None });
        }
        let status = job.build.status().to_string();
        self.status(&status);
        let requested = self.ctrl.requested_exposure_duration(self.camera_connected());
        self.darks.select_dark(requested);
    }

    pub(super) fn darks_view(&self) -> Option<DarksView> {
        self.darks_job.as_ref().map(|j| DarksView {
            defect_map: j.defect_map,
            status: j.build.status().to_string(),
            progress: j.build.progress(),
        })
    }

    // ---------------------------------------------------------- polar drift

    /// `PolarDriftToolWin()` after `CreatePolarDriftToolWindow`'s checks:
    /// open, and loop when the camera is idle.
    pub(super) fn polar_drift_open(&mut self) -> Reply {
        tools::open_check(self.camera_connected(), self.guider.is_calibrating_or_guiding())
            .map_err(|e| CmdError::new(1, e))?;
        if self.polar_drift.is_none() {
            let scale = self.pixel_scale();
            self.polar_drift = Some(PolarDrift::new(&self.store.profile(), scale, None));
            self.drain_polar_drift_log();
        }
        if !self.ctrl.capture_active() {
            self.call(|c, cx, ev| c.start_looping(cx, ev));
        }
        Ok(json!(0))
    }

    /// Start: the guide outputs go off while the star drifts.
    pub(super) fn polar_drift_start(&mut self) -> Reply {
        let outputs = self.guide_outputs();
        let (busy, locked) = (self.guider.is_calibrating_or_guiding(), self.guider.multistar().is_locked());
        let Some(pd) = self.polar_drift.as_mut() else { return Err(CmdError::new(1, "Polar Drift tool not open")) };
        let res = pd.start(busy, locked, outputs, None);
        self.drain_polar_drift_log();
        res.map_err(|e| CmdError::new(1, e))?;
        self.set_guide_outputs(GuideOutputs {
            primary: outputs.primary.map(|_| false),
            secondary: outputs.secondary.map(|_| false),
        });
        Ok(json!(0))
    }

    /// Stop, or close (`OnClose` stops a drift first): the guide outputs
    /// back as they were.
    pub(super) fn polar_drift_stop(&mut self, close: bool) -> Reply {
        let Some(pd) = self.polar_drift.as_mut() else { return Err(CmdError::new(1, "Polar Drift tool not open")) };
        let restore = if close { pd.close(None) } else { pd.stop(None) };
        self.drain_polar_drift_log();
        if close {
            self.polar_drift = None;
        }
        if let Some(o) = restore {
            self.set_guide_outputs(o);
        }
        Ok(json!(0))
    }

    /// The polar tools' part of a SELECTED frame (`UpdateState`).
    pub(super) fn pa_tools(&mut self, t: PaTools) {
        if t != PaTools::UpdateState {
            return;
        }
        let (now_ms, pos) = (self.now_ms(), self.guider.current_position());
        if let (Some(pd), Some(pos)) = (self.polar_drift.as_mut(), pos) {
            pd.add_sample(now_ms as f64 / 1000.0, pos);
        }
        self.drain_polar_drift_log();
    }

    fn drain_polar_drift_log(&mut self) {
        let lines = self.polar_drift.as_mut().map(|p| p.take_log()).unwrap_or_default();
        for l in lines {
            self.debug(&l);
        }
    }

    pub(super) fn polar_drift_view(&self) -> Option<PolarDriftView> {
        self.polar_drift.as_ref().map(|p| PolarDriftView {
            phase: p.phase(),
            drifting: p.is_drifting(),
            status: p.status().clone(),
            result: p.result(),
        })
    }
}

/// The assistant's controls besides open and Start.
pub(super) enum GaCommand {
    Stop,
    EndWait(bool),
    MeasureBacklash(bool),
    Apply(RecKind),
    Review(String),
    Close,
}
