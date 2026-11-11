//! Lock-position bookmarks (the Bookmarks menu, shift-click on the frame)
//! and the Adjust Lock Position tool's saved lock position.
//!
//! Ported from PHD2 `src/guider.cpp` (SaveBookmarks, LoadBookmarks, FindBookmark, ToggleBookmark, BookmarkPos), `src/nudge_lock.cpp` (OnSaveLockPosClicked, OnRestoreLockPosClicked) — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::ProfileAccess;
use crate::engine::traits::Point;

pub mod keys {
    /// `x y ` pairs, `%.5f` each.
    pub const BOOKMARKS: &str = "/guider/bookmarks";
    pub const SAVED_LOCK_X: &str = "/NudgeLock/SavedLockPosX";
    pub const SAVED_LOCK_Y: &str = "/NudgeLock/SavedLockPosY";
}

/// Two bookmarks this close on each axis are one (`FindBookmark`).
const TOLERANCE: f64 = 6.0;

/// `LoadBookmarks`: pairs read with `istream >>` until one fails or the
/// text ends right after it (`!is.good()`), so a pair needs something
/// after it — `SaveBookmarks` ends each with a space.
pub fn load(p: &impl ProfileAccess) -> Vec<Point> {
    parse(&p.get_str(keys::BOOKMARKS, ""))
}

fn parse(s: &str) -> Vec<Point> {
    let mut v = Vec::new();
    let mut rest = s;
    loop {
        let mut num = || {
            let t = rest.trim_start();
            let end = t.find(char::is_whitespace).unwrap_or(t.len());
            let n = t[..end].parse::<f64>().ok()?;
            rest = &t[end..];
            Some(n)
        };
        let (Some(x), Some(y)) = (num(), num()) else { break };
        if rest.is_empty() {
            break;
        }
        v.push(Point::new(x, y));
    }
    v
}

/// `SaveBookmarks`.
pub fn save(p: &mut impl ProfileAccess, v: &[Point]) {
    let s: String = v.iter().map(|b| format!("{:.5} {:.5} ", b.x, b.y)).collect();
    p.set_str(keys::BOOKMARKS, &s);
}

/// `FindBookmark`.
pub fn find(v: &[Point], pos: Point) -> Option<usize> {
    v.iter().position(|b| (b.x - pos.x).abs() <= TOLERANCE && (b.y - pos.y).abs() <= TOLERANCE)
}

/// `BookmarkPos`: a bookmark near `pos` goes, `pos` is added last.
pub fn add(v: &mut Vec<Point>, pos: Point) {
    if let Some(i) = find(v, pos) {
        v.remove(i);
    }
    v.push(pos);
}

/// `ToggleBookmark` (shift-click): the bookmark near `pos` goes, or `pos`
/// is added.
pub fn toggle(v: &mut Vec<Point>, pos: Point) {
    match find(v, pos) {
        Some(i) => {
            v.remove(i);
        }
        None => v.push(pos),
    }
}

/// `OnSaveLockPosClicked`.
pub fn save_lock(p: &mut impl ProfileAccess, lock: Point) {
    p.set_f64(keys::SAVED_LOCK_X, lock.x);
    p.set_f64(keys::SAVED_LOCK_Y, lock.y);
}

/// `OnRestoreLockPosClicked`'s read: `None` before a save (−1).
pub fn saved_lock(p: &impl ProfileAccess) -> Option<Point> {
    let (x, y) = (p.get_f64(keys::SAVED_LOCK_X, -1.0), p.get_f64(keys::SAVED_LOCK_Y, -1.0));
    (x >= 0.0 && y >= 0.0).then(|| Point::new(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmarks_round_trip_and_edit() {
        let mut v = parse("10.50000 20.25000 300.00000 40.00000 ");
        assert_eq!(v, [Point::new(10.5, 20.25), Point::new(300.0, 40.0)]);
        // The last pair needs something after it; a bad token stops.
        assert_eq!(parse("1 2 3 4").len(), 1);
        assert_eq!(parse("1 2 x 4 5 6 ").len(), 1);
        assert!(parse("").is_empty());
        add(&mut v, Point::new(14.0, 26.0));
        assert_eq!(v, [Point::new(300.0, 40.0), Point::new(14.0, 26.0)]);
        toggle(&mut v, Point::new(298.0, 35.0));
        assert_eq!(v, [Point::new(14.0, 26.0)]);
        toggle(&mut v, Point::new(100.0, 100.0));
        assert_eq!(v.len(), 2);
        assert_eq!(find(&v, Point::new(20.0, 32.0)), Some(0));
        assert_eq!(find(&v, Point::new(20.1, 26.0)), None);
        let s: String = v.iter().map(|b| format!("{:.5} {:.5} ", b.x, b.y)).collect();
        assert_eq!(s, "14.00000 26.00000 100.00000 100.00000 ");
        assert_eq!(parse(&s), v);
    }
}
