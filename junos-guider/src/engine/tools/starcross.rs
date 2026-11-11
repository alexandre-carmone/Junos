//! The star-cross test: the mount is pulsed west, east, back west, then
//! north, south and back north, while the main camera takes one long
//! exposure; the trails show the axes' orthogonality and backlash.
//!
//! [`StarCross::next`] is one turn of `ExecuteTest`'s loop: the engine
//! makes the pulse it returns (`MoveAxis(…, MOVEOPTS_CALIBRATION_MOVE)` on
//! `TheScope()`) and reports how it went, until the test ends or is
//! cancelled between two pulses (the dialog's Stop).
//!
//! Ported from PHD2 `src/starcross_test.cpp` — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::ProfileAccess;
use crate::engine::traits::Dir;

/// The profile keys the test saves when it completes; the guide speed is
/// the calibration step calculator's.
pub mod keys {
    pub const PULSE_COUNT: &str = "/SCT/PulseCount";
    pub const PULSE_SIZE: &str = "/SCT/PulseSize";
    pub const GUIDE_SPEED: &str = "/CalStepCalc/GuideSpeed";
}

const DEFAULT_PULSE_SIZE: f64 = 1000.0;
const DEFAULT_PULSE_COUNT: f64 = 25.0;
const DEFAULT_GUIDESPEED: f64 = 0.5;

/// The dialog's spinners' ranges: pulses per direction, pulse ms.
pub const PULSE_COUNT_RANGE: (i32, i32) = (5, 40);
pub const PULSE_SIZE_RANGE: (i32, i32) = (500, 5000);

/// `SCT_STATE_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    None,
    West,
    East,
    WestReturn,
    North,
    South,
    NorthReturn,
    Done,
}

/// The dialog's fields when it opens: pulses per direction, pulse ms, the
/// guide speed (× sidereal). Without a completed test PHD2 suggests the
/// pulses from the guide speed (`SuggestParams`).
pub fn defaults(p: &impl ProfileAccess) -> (i32, i32, f64) {
    let speed = p.get_f64(keys::GUIDE_SPEED, DEFAULT_GUIDESPEED);
    if !p.has(keys::PULSE_COUNT) {
        return (suggested_pulses(speed), 1000, speed);
    }
    let count = p.get_f64(keys::PULSE_COUNT, DEFAULT_PULSE_COUNT) as i32;
    let size = p.get_f64(keys::PULSE_SIZE, DEFAULT_PULSE_SIZE) as i32;
    (count, size, speed)
}

/// `SuggestParams`: room for 25 s of backlash at 0.5× sidereal.
pub fn suggested_pulses(guide_speed: f64) -> i32 {
    (24.0 * 0.5 / guide_speed).ceil() as i32
}

/// The test under way (`StarCrossDialog::ExecuteTest`'s locals).
#[derive(Clone, Debug)]
pub struct StarCross {
    amount: i32,
    per_dir: i32,
    state: State,
    dir: Option<Dir>,
    pulse_count: i32,
    dir_count: i32,
    count: i32,
    done: bool,
    cancel: bool,
    error: bool,
    ao: bool,
    status: String,
    log: Vec<String>,
}

impl StarCross {
    /// Start with the spinners' values (clamped to their ranges, as the
    /// controls hold them); `ao`: pMount is an AO, whose direction names
    /// the debug log uses.
    pub fn new(pulse_count: i32, pulse_size: i32, ao: bool) -> Self {
        Self {
            amount: pulse_size.clamp(PULSE_SIZE_RANGE.0, PULSE_SIZE_RANGE.1),
            per_dir: pulse_count.clamp(PULSE_COUNT_RANGE.0, PULSE_COUNT_RANGE.1),
            state: State::None,
            dir: None,
            pulse_count: 0,
            dir_count: 0,
            count: 0,
            done: false,
            cancel: false,
            error: false,
            ao,
            status: String::new(),
            log: Vec::new(),
        }
    }

    /// `NextStep`.
    fn next_step(&mut self) {
        let n = self.per_dir;
        let (state, dir, count) = match self.state {
            State::None => (State::West, Some(Dir::West), n),
            State::West => (State::East, Some(Dir::East), 2 * n),
            State::East => (State::WestReturn, Some(Dir::West), n),
            State::WestReturn => (State::North, Some(Dir::North), n),
            State::North => (State::South, Some(Dir::South), 2 * n),
            State::South => (State::NorthReturn, Some(Dir::North), n),
            State::NorthReturn | State::Done => (State::Done, None, 0),
        };
        (self.state, self.dir, self.pulse_count) = (state, dir, count);
    }

    /// `Explanation`.
    fn explanation(&self) -> String {
        let what = match self.state {
            State::None => "",
            State::West => "WEST",
            State::East => "EAST",
            State::WestReturn => "WEST toward starting point",
            State::North => "NORTH",
            State::South => "SOUTH",
            State::NorthReturn => "NORTH toward starting point",
            State::Done => return "Test completed".to_string(),
        };
        format!("{} ms move {what}, step {} of {}", self.amount, self.dir_count, self.pulse_count)
    }

    /// The next pulse to make, or `None` once the test is over (completed,
    /// cancelled or failed). `mount_connected`: `pMount->IsConnected()`,
    /// checked on every turn, the last one included, as PHD2 does.
    pub fn next(&mut self, mount_connected: bool) -> Option<Dir> {
        if self.done || self.cancel {
            return None;
        }
        if self.dir_count == self.pulse_count {
            self.next_step();
            self.dir_count = 0;
            if self.state == State::Done {
                self.log.push("Star-cross test completed".into());
                self.status = self.explanation();
                self.done = true;
            }
        }
        if !mount_connected {
            self.log.push("Star-cross error, mount connection lost, test cancelled".into());
            self.status = "Mount connection lost, test cancelled".into();
            (self.cancel, self.error) = (true, true);
            self.end();
            return None;
        }
        if self.done {
            return None;
        }
        self.count += 1;
        self.dir_count += 1;
        let dir = self.dir.expect("a direction until done");
        let name = if self.ao { dir.ao_label() } else { dir.mount_label() };
        self.log.push(format!("Star-cross move {}/{}, {name} for {} ms", self.count, 8 * self.per_dir, self.amount));
        self.status = self.explanation();
        Some(dir)
    }

    /// The pulse [`next`](Self::next) asked for was made: `ok` when it was
    /// `MOVE_OK`.
    pub fn moved(&mut self, ok: bool) {
        if !ok {
            self.log.push("Star-cross move failed, test cancelled".into());
            self.status = "Star-cross moved failed, test cancelled".into();
            (self.cancel, self.error) = (true, true);
            self.end();
        }
    }

    /// `OnCancel`: the loop stops before the next pulse.
    pub fn stop(&mut self) {
        if !self.done && !self.cancel {
            self.cancel = true;
            self.end();
        }
    }

    fn end(&mut self) {
        if self.error {
            self.log.push("Star-cross test cancelled because of exception".into());
        } else {
            self.status = "Test cancelled".into();
            self.log.push("Star-cross test cancelled by user".into());
        }
    }

    /// The pulses of a completed test into the profile, as doubles.
    pub fn save(&self, p: &mut impl ProfileAccess) {
        if self.done {
            p.set_f64(keys::PULSE_COUNT, self.per_dir as f64);
            p.set_f64(keys::PULSE_SIZE, self.amount as f64);
        }
    }

    pub fn running(&self) -> bool {
        !self.done && !self.cancel
    }

    pub fn completed(&self) -> bool {
        self.done
    }

    /// (pulses made, pulses in all).
    pub fn progress(&self) -> (i32, i32) {
        (self.count, 8 * self.per_dir)
    }

    /// (pulses per direction, pulse ms).
    pub fn params(&self) -> (i32, i32) {
        (self.per_dir, self.amount)
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn take_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tools::test_util::Mem;

    fn run(t: &mut StarCross, fail_at: Option<usize>) -> Vec<Dir> {
        let mut dirs = Vec::new();
        while let Some(d) = t.next(true) {
            dirs.push(d);
            t.moved(Some(dirs.len()) != fail_at);
        }
        dirs
    }

    #[test]
    fn the_cross_and_its_texts() {
        let mut t = StarCross::new(5, 1000, false);
        let dirs = run(&mut t, None);
        let mut want = vec![Dir::West; 5];
        want.extend([Dir::East; 10]);
        want.extend([Dir::West; 5]);
        want.extend([Dir::North; 5]);
        want.extend([Dir::South; 10]);
        want.extend([Dir::North; 5]);
        assert_eq!(dirs, want);
        assert!(t.completed() && !t.running());
        assert_eq!(t.status(), "Test completed");
        let log = t.take_log();
        assert_eq!(log[0], "Star-cross move 1/40, West for 1000 ms");
        assert_eq!(log[39], "Star-cross move 40/40, North for 1000 ms");
        assert_eq!(log[40], "Star-cross test completed");
        assert_eq!(log.len(), 41);
        // A failed pulse ends it there.
        let mut t = StarCross::new(5, 1000, false);
        assert_eq!(run(&mut t, Some(3)).len(), 3);
        assert!(!t.completed() && !t.running());
    }

    #[test]
    fn explanation_failure_stop_and_clamps() {
        let mut t = StarCross::new(1, 100, true);
        assert_eq!(t.params(), (5, 500));
        assert_eq!(t.next(true), Some(Dir::West));
        assert_eq!(t.status(), "500 ms move WEST, step 1 of 5");
        assert_eq!(t.take_log(), ["Star-cross move 1/40, Left for 500 ms"]);
        t.moved(true);
        for _ in 0..8 {
            t.next(true);
        }
        assert_eq!(t.status(), "500 ms move EAST, step 4 of 10");
        t.moved(false);
        assert_eq!(t.status(), "Star-cross moved failed, test cancelled");
        assert!(t.take_log().ends_with(&[
            "Star-cross move failed, test cancelled".to_string(),
            "Star-cross test cancelled because of exception".to_string()
        ]));
        assert_eq!(t.next(true), None);

        let mut t = StarCross::new(5, 1000, false);
        t.next(true);
        t.stop();
        assert_eq!(t.next(true), None);
        assert_eq!(t.status(), "Test cancelled");
        assert_eq!(t.take_log().last().unwrap(), "Star-cross test cancelled by user");

        let mut t = StarCross::new(5, 1000, false);
        assert_eq!(t.next(false), None);
        assert_eq!(t.status(), "Mount connection lost, test cancelled");

        assert_eq!(suggested_pulses(0.5), 24);
        assert_eq!(suggested_pulses(0.3), 40);
        assert_eq!(defaults(&Mem::default()), (24, 1000, 0.5));
    }
}
