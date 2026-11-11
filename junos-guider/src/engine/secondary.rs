//! PHD2's secondary tools as the engine drives them: what each window does
//! with the gear, the guider and the profile, around the pure state
//! machines of [`super::tools`]. Their state reaches the UI as one JSON
//! view ([`Engine::tools_view`]).
//!
//! Ported from PHD2 src/starcross_test.cpp (OnStart, ExecuteTest) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::{Value, json};

use super::command::{CmdError, Reply};
use super::tools::starcross::{self, StarCross};
use super::{Engine, Pending};
use super::traits::MoveResult;

/// The tool windows of this module, each while open or running.
#[derive(Default)]
pub(super) struct Tools {
    starcross: Option<StarCross>,
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
            "starcross": sct,
        })
    }
}
