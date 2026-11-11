//! The engine's base vocabulary: geometry, directions, move options,
//! outcomes, C int casts, time and cancellation. Device traits (camera, guide output, pointing, AO, rotator)
//! are declared next to the modules that first need them.
//!
//! Ported from PHD2 `src/guide_direction.h`, `src/point.h`, `src/mount.h`,
//! `src/phd.h` (BSD-3-Clause, see THIRD_PARTY.md).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A rectangle with `wxRect` semantics: `right() == x + w - 1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w - 1
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h - 1
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x <= self.right() && y >= self.y && y <= self.bottom()
    }

    /// `wxRect::Intersect`: the overlap, or an empty rect.
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 < x0 || y1 < y0 {
            Rect::default()
        } else {
            Rect::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
        }
    }
}

/// A camera-plane point in pixels. `None` stands for PHD2's invalid `PHD_Point`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(&self, other: &Point) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    /// `atan2(y, x)` of the vector from `from` to `self`.
    pub fn angle(&self, from: &Point) -> f64 {
        (self.y - from.y).atan2(self.x - from.x)
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

impl std::ops::Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

/// PHD2's `GUIDE_DIRECTION`: `UP == NORTH`, `DOWN == SOUTH`, `RIGHT == EAST`,
/// `LEFT == WEST`. The discriminants are PHD2's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    North = 0,
    South = 1,
    East = 2,
    West = 3,
}

impl Dir {
    pub const UP: Dir = Dir::North;
    pub const DOWN: Dir = Dir::South;
    pub const RIGHT: Dir = Dir::East;
    pub const LEFT: Dir = Dir::West;

    pub fn opposite(self) -> Dir {
        match self {
            Dir::North => Dir::South,
            Dir::South => Dir::North,
            Dir::East => Dir::West,
            Dir::West => Dir::East,
        }
    }

    /// Mount wording, as in the `GuideStep` event and the guide log.
    pub fn mount_label(self) -> &'static str {
        match self {
            Dir::North => "North",
            Dir::South => "South",
            Dir::East => "East",
            Dir::West => "West",
        }
    }

    /// AO wording (`Up`/`Down`/`Right`/`Left`).
    pub fn ao_label(self) -> &'static str {
        match self {
            Dir::North => "Up",
            Dir::South => "Down",
            Dir::East => "Right",
            Dir::West => "Left",
        }
    }

    /// The one-letter form of the guide-log rows (`N`, `S`, `E`, `W`).
    pub fn log_char(self) -> char {
        match self {
            Dir::North => 'N',
            Dir::South => 'S',
            Dir::East => 'E',
            Dir::West => 'W',
        }
    }

    /// Parse any spelling the event server accepts (`n`, `north`, `up`, …).
    pub fn parse(s: &str) -> Option<Dir> {
        match s.to_ascii_lowercase().as_str() {
            "n" | "north" | "up" | "u" => Some(Dir::North),
            "s" | "south" | "down" | "d" => Some(Dir::South),
            "e" | "east" | "right" | "r" => Some(Dir::East),
            "w" | "west" | "left" | "l" => Some(Dir::West),
            _ => None,
        }
    }
}

/// PHD2's `Mount::MOVE_RESULT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveResult {
    Ok,
    Error,
    Slewing,
    AoLimit,
}

/// PHD2's `MountMoveOptionBits` and `MOVEOPTS_*` sets (`mount.h:125-142`):
/// how a move is filtered, compensated and shown. Defined here, below both
/// the mount and the backlash compensation that read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MoveOpts(pub u32);

impl MoveOpts {
    /// Filter the move through the guide algorithm.
    pub const ALGO_RESULT: MoveOpts = MoveOpts(1 << 0);
    /// Let the algorithm deduce the move (paused, or the star lost).
    pub const ALGO_DEDUCE: MoveOpts = MoveOpts(1 << 1);
    /// Apply backlash compensation to the move.
    pub const USE_BLC: MoveOpts = MoveOpts(1 << 2);
    /// Show the move on the graphs.
    pub const GRAPH: MoveOpts = MoveOpts(1 << 3);
    /// A manual move, allowed even with guiding disabled.
    pub const MANUAL: MoveOpts = MoveOpts(1 << 4);

    pub const CALIBRATION_MOVE: MoveOpts = MoveOpts(0);
    pub const GUIDE_STEP: MoveOpts = MoveOpts(Self::ALGO_RESULT.0 | Self::USE_BLC.0 | Self::GRAPH.0);
    pub const DEDUCED_MOVE: MoveOpts = MoveOpts(Self::ALGO_DEDUCE.0 | Self::USE_BLC.0 | Self::GRAPH.0);
    pub const RECOVERY_MOVE: MoveOpts = Self::USE_BLC;
    pub const AO_BUMP: MoveOpts = Self::USE_BLC;

    /// PHD2's `moveOptions & bit` test.
    pub fn has(self, bit: MoveOpts) -> bool {
        self.0 & bit.0 != 0
    }
}

/// A C `(int)` cast of a double on x86-64 (`cvttsd2si`): truncation toward
/// zero, with NaN and out-of-range values giving `INT_MIN` where Rust's `as`
/// would give 0 or saturate.
pub fn c_int(x: f64) -> i32 {
    if x.is_nan() || x >= 2147483648.0 || x < -2147483648.0 { i32::MIN } else { x as i32 }
}

/// PHD2's `ROUND` (`phd.h:122`): `(int) floor(x + 0.5)`. Not `f64::round`,
/// which rounds halves away from zero.
pub fn phd_round(x: f64) -> i32 {
    c_int((x + 0.5).floor())
}

/// PHD2's `PierSide` (`PIER_SIDE_UNKNOWN = -1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PierSide {
    #[default]
    Unknown = -1,
    East = 0,
    West = 1,
}

impl PierSide {
    pub fn label(self) -> &'static str {
        match self {
            PierSide::Unknown => "Unknown",
            PierSide::East => "East",
            PierSide::West => "West",
        }
    }

    pub fn opposite(self) -> PierSide {
        match self {
            PierSide::East => PierSide::West,
            PierSide::West => PierSide::East,
            PierSide::Unknown => PierSide::Unknown,
        }
    }
}

/// PHD2's `GuideParity` (`EVEN = 1`, `ODD = -1`, `UNKNOWN = 0`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Parity {
    #[default]
    Unknown = 0,
    Even = 1,
    Odd = -1,
}

impl Parity {
    pub fn label(self) -> &'static str {
        match self {
            Parity::Even => "Even",
            Parity::Odd => "Odd",
            Parity::Unknown => "N/A",
        }
    }

    pub fn flipped(self) -> Parity {
        match self {
            Parity::Even => Parity::Odd,
            Parity::Odd => Parity::Even,
            Parity::Unknown => Parity::Unknown,
        }
    }
}

/// The engine's only source of time. A monotonic clock for durations
/// (PHD2's `wxStopWatch` / `steady_clock`), local wall time for the logs,
/// and the worker-thread sleep. The simulator's clock advances on sleep and
/// on each exposure, so the tests are deterministic.
pub trait Clock {
    /// Monotonic seconds.
    fn mono(&self) -> f64;
    fn wall(&self) -> chrono::DateTime<chrono::Local>;
    fn sleep_ms(&self, ms: u64);
}

/// The real clock.
pub struct SystemClock {
    start: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self { start: std::time::Instant::now() }
    }
}

impl Clock for SystemClock {
    fn mono(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn wall(&self) -> chrono::DateTime<chrono::Local> {
        chrono::Local::now()
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Set from another thread to interrupt a blocking device wait (PHD2's
/// `WorkerThread::InterruptRequested`).
#[derive(Clone, Default, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_wx_semantics() {
        let r = Rect::new(10, 20, 5, 4);
        assert_eq!(r.right(), 14);
        assert_eq!(r.bottom(), 23);
        assert!(r.contains(14, 23));
        assert!(!r.contains(15, 23));
        let i = r.intersect(&Rect::new(12, 0, 100, 22));
        assert_eq!(i, Rect::new(12, 20, 3, 2));
        assert!(r.intersect(&Rect::new(100, 100, 1, 1)).is_empty());
    }

    #[test]
    fn directions() {
        assert_eq!(Dir::UP, Dir::North);
        assert_eq!(Dir::LEFT, Dir::West);
        assert_eq!(Dir::parse("Left"), Some(Dir::West));
        assert_eq!(Dir::West.opposite(), Dir::East);
        assert_eq!(Dir::South.log_char(), 'S');
    }

    #[test]
    fn move_opts_sets() {
        assert_eq!(MoveOpts::GUIDE_STEP, MoveOpts(13));
        assert_eq!(MoveOpts::DEDUCED_MOVE, MoveOpts(14));
        assert!(MoveOpts::GUIDE_STEP.has(MoveOpts::USE_BLC));
        assert!(!MoveOpts::RECOVERY_MOVE.has(MoveOpts::ALGO_RESULT));
        assert!(!MoveOpts::CALIBRATION_MOVE.has(MoveOpts::USE_BLC));
    }

    #[test]
    fn c_casts_match_x86() {
        assert_eq!(c_int(f64::NAN), i32::MIN);
        assert_eq!(c_int(f64::INFINITY), i32::MIN);
        assert_eq!(c_int(3e9), i32::MIN);
        assert_eq!(c_int(812.9), 812);
        assert_eq!(c_int(-0.5), 0);
        assert_eq!(phd_round(2.5), 3);
        assert_eq!(phd_round(-2.5), -2);
        assert_eq!(phd_round(f64::NAN), i32::MIN);
    }

    #[test]
    fn cancel_token_is_shared() {
        let a = CancelToken::new();
        let b = a.clone();
        b.cancel();
        assert!(a.is_cancelled());
        a.reset();
        assert!(!b.is_cancelled());
    }
}
