//! Comet tracking's training: while guiding, the user keeps the comet
//! centred with the Adjust Lock Position controls; each lock move sets the
//! lock-shift rate to the lock's drift since training started.
//!
//! Ported from PHD2 `src/comet_tool.cpp` (OnStart, OnStop, CalcRate, UpdateStatus) — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::traits::Point;

/// A training under way (`m_startPos`, `m_startTime`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CometTraining {
    start: Point,
    start_ms: i64,
}

impl CometTraining {
    /// `OnStart`: from the lock position, at UTC `now_ms`.
    pub fn new(lock: Point, now_ms: i64) -> Self {
        Self { start: lock, start_ms: now_ms }
    }

    /// `CalcRate`: px per hour since the start (camera axes).
    pub fn rate(&self, lock: Point, now_ms: i64) -> Point {
        let dt = (now_ms - self.start_ms) as f64 / 3_600_000.0;
        let d = lock - self.start;
        Point::new(d.x / dt, d.y / dt)
    }

    /// Seconds since the start, as the status shows them.
    pub fn elapsed_s(&self, now_ms: i64) -> i64 {
        (now_ms - self.start_ms) / 1000
    }
}

/// `UpdateStatus`'s text.
pub fn status(training: Option<&CometTraining>, now_ms: i64) -> String {
    match training {
        Some(t) => format!(
            "Training, elapsed time {}s.\nUse the \"Adjust Lock Position\" controls to center the comet\nin the imaging \
             camera and click Stop to complete training.",
            t.elapsed_s(now_ms)
        ),
        None => "Center the comet in the imaging camera.\nSelect a guide star and start Guiding.\nThen, click Start to \
                 begin training."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_from_two_lock_positions() {
        let t = CometTraining::new(Point::new(100.0, 200.0), 1_000_000);
        // 3 px right and 1.5 px up in 30 min: 6 and -3 px/h.
        assert_eq!(t.rate(Point::new(103.0, 198.5), 1_000_000 + 1_800_000), Point::new(6.0, -3.0));
        assert!(status(Some(&t), 1_062_500).starts_with("Training, elapsed time 62s.\nUse the \"Adjust Lock"));
        assert!(status(None, 0).ends_with("Then, click Start to begin training."));
    }
}
