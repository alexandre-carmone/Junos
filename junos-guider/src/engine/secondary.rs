//! PHD2's secondary tools as the engine drives them: what each window does
//! with the gear, the guider and the profile, around the pure state
//! machines of [`super::tools`]. Their state reaches the UI as one JSON
//! view ([`Engine::tools_view`]).
//!
//! Ported from PHD2 src/starcross_test.cpp (OnStart, ExecuteTest), src/nudge_lock.cpp (UpdateLockPos, DoMove), src/comet_tool.cpp (OnStart, OnStop, CalcRate, OnAppStateNotify), src/guider.cpp (the bookmarks) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::{Value, json};

use super::command::{BookmarkOp, CmdError, INVALID_REQUEST, Reply};
use super::guider::ShiftUnits;
use super::tools::bookmarks;
use super::tools::comet::{self, CometTraining};
use super::tools::starcross::{self, StarCross};
use super::traits::{MoveResult, Point};
use super::{Engine, Pending};

/// The tool windows of this module, each while open or running.
#[derive(Default)]
pub(super) struct Tools {
    starcross: Option<StarCross>,
    /// `m_bookmarks`, read with the guider's profile settings.
    bookmarks: Vec<Point>,
    /// The comet tool's training.
    comet: Option<CometTraining>,
}

impl Engine {
    /// `pMount && pMount->IsConnected()`.
    pub(super) fn pmount_connected(&self) -> bool {
        match (&self.ao, &self.scope) {
            (Some(a), _) => a.sg.mount().is_connected(),
            (None, Some(s)) => s.scope.mount.is_connected(),
            (None, None) => false,
        }
    }

    /// Whether a tool has pulses to make: [`Engine::step`] makes them in
    /// place of exposures.
    pub(super) fn tool_pulsing(&self) -> bool {
        self.tools.starcross.as_ref().is_some_and(|t| t.running())
    }

    /// What the tools read with the guider's profile settings
    /// (`Guider::LoadProfileSettings`).
    pub(super) fn load_tool_settings(&mut self) {
        self.tools.bookmarks = bookmarks::load(&self.store.profile());
    }

    // ---------------------------------------------------------- the lock

    /// `UpdateLockPos`: a position inside the frame becomes the lock; a
    /// comet training takes its rate from it (`NotifyUpdateLockPos`).
    fn update_lock_pos(&mut self, p: Point) -> bool {
        let now_ms = self.now_ms();
        let training = self.tools.comet;
        self.call(|c, cx, ev| {
            let mut gev = Vec::new();
            let set = cx.guider.is_valid_lock_position(p) && cx.guider.set_lock_position(p, &mut cx.gcx, &mut gev);
            if set && let (Some(t), Some(lock)) = (training, cx.guider.lock_position()) {
                let rate = t.rate(lock, now_ms);
                cx.guider.set_lock_shift_rate(rate, ShiftUnits::Pixels, false, &mut cx.gcx, &mut gev);
            }
            c.forward(gev, cx, ev);
            set
        })
    }

    // -------------------------------------------------------- comet tool

    /// The comet tool's Start: only while guiding (PHD2's button is off
    /// otherwise). The lock shift goes on.
    pub(super) fn comet_train_start(&mut self) -> Reply {
        if !self.guider.is_guiding() {
            return Err(CmdError::new(1, "not guiding"));
        }
        let Some(lock) = self.guider.lock_position() else { return Err(CmdError::new(1, "no lock position")) };
        self.tools.comet = Some(CometTraining::new(lock, self.now_ms()));
        let upd = self.call(|c, cx, ev| {
            let mut gev = Vec::new();
            let upd = cx.guider.enable_lock_shift(true, &mut cx.gcx, &mut gev);
            c.forward(gev, cx, ev);
            upd
        });
        self.queue_moves(&upd.moves);
        Ok(json!(0))
    }

    /// Its Stop: the rate stays as trained.
    pub(super) fn comet_train_stop(&mut self) -> Reply {
        self.tools.comet = None;
        Ok(json!(0))
    }

    /// `OnAppStateNotify`: training stops when guiding does.
    pub(super) fn comet_check(&mut self) {
        if self.tools.comet.is_some() && !self.guider.is_guiding() {
            self.tools.comet = None;
        }
    }

    /// The Adjust Lock Position tool's arrows (`DoMove`).
    pub(super) fn nudge_lock(&mut self, dx: f64, dy: f64) -> Reply {
        let Some(lock) = self.guider.lock_position() else { return Err(CmdError::new(1, "no lock position")) };
        match self.update_lock_pos(Point::new(lock.x + dx, lock.y + dy)) {
            true => Ok(json!(0)),
            false => Err(CmdError::new(INVALID_REQUEST, "could not set lock position")),
        }
    }

    /// Its Save (`OnSaveLockPosClicked`).
    pub(super) fn save_lock(&mut self) -> Reply {
        let Some(lock) = self.guider.lock_position() else { return Err(CmdError::new(1, "no lock position")) };
        bookmarks::save_lock(&mut self.store.profile(), lock);
        Ok(json!(0))
    }

    /// Its Restore (`OnRestoreLockPosClicked`; PHD2 asks first, the UI
    /// does): the saved position, as set.
    pub(super) fn restore_lock(&mut self) -> Reply {
        if self.guider.lock_position().is_none() {
            return Err(CmdError::new(1, "no lock position"));
        }
        let Some(p) = bookmarks::saved_lock(&self.store.profile()) else {
            return Err(CmdError::new(1, "no saved lock position"));
        };
        match self.update_lock_pos(p) {
            true => Ok(json!([p.x, p.y])),
            false => Err(CmdError::new(INVALID_REQUEST, "could not set lock position")),
        }
    }

    /// The Bookmarks menu.
    pub(super) fn bookmark(&mut self, op: BookmarkOp) -> Reply {
        let v = &mut self.tools.bookmarks;
        match op {
            BookmarkOp::Lock | BookmarkOp::Star => {
                let pos = match op {
                    BookmarkOp::Lock => self.guider.lock_position(),
                    _ => self.guider.current_position(),
                };
                let Some(pos) = pos else { return Err(CmdError::new(1, "no position to bookmark")) };
                bookmarks::add(v, pos);
            }
            BookmarkOp::Toggle(p) => bookmarks::toggle(v, p),
            BookmarkOp::Delete(p) => match bookmarks::find(v, p) {
                Some(i) => {
                    v.remove(i);
                }
                None => return Err(CmdError::new(1, "no bookmark there")),
            },
            // PHD2 saves nothing when there was nothing to delete.
            BookmarkOp::Clear if v.is_empty() => return Ok(json!(0)),
            BookmarkOp::Clear => v.clear(),
        }
        bookmarks::save(&mut self.store.profile(), &self.tools.bookmarks);
        Ok(json!(0))
    }

    // ------------------------------------------------------ star-cross test

    /// The dialog's Start (`OnStart`): the user has started the main
    /// camera's exposure. PHD2 starts looping first so something shows.
    /// Without values, the dialog's (the last completed test's, else the
    /// suggestion from the guide speed).
    pub(super) fn starcross_start(&mut self, pulse_count: Option<i32>, pulse_size: Option<i32>) -> Reply {
        if self.tool_pulsing() {
            return Err(CmdError::new(1, "Star-cross test running"));
        }
        if !self.pmount_connected() {
            return Err(CmdError::new(1, "Mount connection must be restored"));
        }
        if self.camera_connected() {
            self.call(|c, cx, ev| c.start_looping(cx, ev));
        }
        let (count, size, _) = starcross::defaults(&self.store.profile());
        let (count, size) = (pulse_count.unwrap_or(count), pulse_size.unwrap_or(size));
        self.tools.starcross = Some(StarCross::new(count, size, self.ao.is_some()));
        self.cancel.reset();
        Ok(json!(0))
    }

    /// The dialog's Stop.
    pub(super) fn starcross_stop(&mut self) -> Reply {
        let Some(t) = self.tools.starcross.as_mut() else { return Err(CmdError::new(1, "Star-cross test not running")) };
        t.stop();
        self.drain_tool_logs();
        Ok(json!(0))
    }

    /// One pulse of a running tool, made at once on `TheScope()`. Deviation:
    /// PHD2 pulses on its main thread while its worker loops; here the
    /// engine's one thread makes the pulses back to back, the exposures
    /// waiting until the test ends, so the test takes the time PHD2's
    /// prompt counts on.
    pub(super) fn tool_step(&mut self) {
        let connected = self.pmount_connected();
        let role = self.scope_role();
        let Some(t) = self.tools.starcross.as_mut() else { return };
        let next = t.next(connected);
        self.drain_tool_logs();
        if let Some(dir) = next {
            let amount = self.tools.starcross.as_ref().map_or(0, |t| t.params().1);
            let r = self.make_move(Pending::Tool { role, dir, amount });
            if let Some(t) = self.tools.starcross.as_mut() {
                t.moved(r == MoveResult::Ok);
            }
            self.drain_tool_logs();
        }
        if let Some(t) = self.tools.starcross.as_ref().filter(|t| t.completed()) {
            t.save(&mut self.store.profile());
        }
    }

    fn drain_tool_logs(&mut self) {
        let lines = self.tools.starcross.as_mut().map(|t| t.take_log()).unwrap_or_default();
        for l in lines {
            self.debug(&l);
        }
    }

    /// The tools' state for the UI (`guider_tools`).
    pub(super) fn tools_view(&self) -> Value {
        let sct = self.tools.starcross.as_ref().map(|t| {
            json!({
                "running": t.running(),
                "completed": t.completed(),
                "status": t.status(),
                "progress": [t.progress().0, t.progress().1],
                "pulse_count": t.params().0,
                "pulse_size": t.params().1,
            })
        });
        json!({
            "bookmarks": self.tools.bookmarks.iter().map(|b| [b.x, b.y]).collect::<Vec<_>>(),
            "starcross": sct,
            "comet": {
                "training": self.tools.comet.is_some(),
                "status": comet::status(self.tools.comet.as_ref(), self.now_ms()),
            },
        })
    }
}
