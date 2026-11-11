//! PHD2's debug log, and what it shares with the guide log: the file names,
//! the imaging day both roll over on, and the clean-up of old files.
//!
//! Every record is `HH:MM:SS.mmm SS.mmm <thread> <text>`: the wall time, the
//! time since the previous record (total seconds, `-` in front when the
//! clock went back) and the writing thread's id. PHD2 flushes after each
//! record so the file is whole after a crash; the writer does the same.
//!
//! Both logs of a session carry one timestamp in their names
//! ([`LogFileStamp`], PHD2's `m_logFileTime`). At start-up it is the newest
//! debug log's when that one belongs to the current imaging day, so a
//! restart in the night appends to the night's files ([`log_file_time`]);
//! the first frame of a new imaging day rolls both logs over
//! ([`LogFileStamp::rollover`]). An imaging day runs from 09:00 to 09:00.
//!
//! Nothing here touches the disk: the writer appends to a `Write` sink the
//! caller opens, and the clean-up gets a folder listing and a remover.
//!
//! Ported from PHD2 src/debuglog.cpp, src/debuglog.h, src/logger.cpp, src/logger.h, src/phd.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::io::{self, Write};

use chrono::{DateTime, Days, Local, NaiveDate, NaiveDateTime, TimeDelta, Timelike};

/// PHD2's `FULLVER`, as both logs print it.
pub const PHD2_VERSION: &str = "2.6.14";

/// Days a debug log is kept (`debuglog.cpp:40`).
pub const RETENTION_DAYS: u64 = 30;

/// `PhdApp::ImagingDay` (`phd.cpp:715`): a time before 09:00 belongs to the
/// previous day's night.
pub fn imaging_day(t: &DateTime<Local>) -> NaiveDate {
    let day = t.date_naive();
    if t.hour() >= 9 { day } else { day.pred_opt().unwrap_or(day) }
}

/// `PhdApp::IsSameImagingDay`.
pub fn is_same_imaging_day(a: &DateTime<Local>, b: &DateTime<Local>) -> bool {
    imaging_day(a) == imaging_day(b)
}

/// What names both log files: PHD2's instance number (`m_instance`) and the
/// shared log-file time (`m_logFileTime`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogFileStamp {
    /// From 2 on, the names get an `N_` qualifier so that two daemons never
    /// write the same file.
    pub instance: u32,
    pub time: DateTime<Local>,
}

impl LogFileStamp {
    /// `PHD2_<kind>_[N_]YYYY-mm-dd_HHMMSS.txt`, `kind` being `GuideLog` or
    /// `DebugLog` (`guidinglog.cpp:271`, `debuglog.cpp:113`).
    pub fn file_name(&self, kind: &str) -> String {
        let qualifier = if self.instance > 1 { format!("{}_", self.instance) } else { String::new() };
        format!("PHD2_{kind}_{qualifier}{}", self.time.format("%Y-%m-%d_%H%M%S.txt"))
    }

    /// `PhdApp::CheckLogRollover` (`phd.cpp:729`), run on every frame: when
    /// `now` falls in a new imaging day it becomes the stamp and this returns
    /// true. The caller then reopens both logs as `OpenLogs(true)` does
    /// (`phd.cpp:405`): [`DebugLog::init`] with the debug log's current
    /// state and `force_open`, [`execution_banner`]`(true)` and the library
    /// lines, then [`GuideLog::close`](super::guide_log::GuideLog::close)
    /// and, if it was on, [`GuideLog::enable`](super::guide_log::GuideLog::enable).
    pub fn rollover(&mut self, now: DateTime<Local>) -> bool {
        if is_same_imaging_day(&self.time, &now) {
            return false;
        }
        self.time = now;
        true
    }
}

/// `DebugLog::GetLogFileTime` (`debuglog.cpp:63`), the stamp at start-up:
/// the time in the newest `PHD2_DebugLog_YYYY-mm-dd_HHMMSS.txt` among
/// `names` (the log folder's files) when it falls in `now`'s imaging day,
/// else `now`. Only an exact 35-character name counts, so an `N_` instance
/// never finds its own files: it reuses instance 1's stamp or starts anew.
pub fn log_file_time<'a>(names: impl IntoIterator<Item = &'a str>, now: DateTime<Local>) -> DateTime<Local> {
    let latest = names
        .into_iter()
        .filter(|n| n.len() == 35 && wild_match("PHD2_DebugLog_*.txt", n))
        .filter_map(|n| NaiveDateTime::parse_from_str(&n[14..31], "%Y-%m-%d_%H%M%S").ok())
        .filter_map(|t| t.and_local_timezone(Local).earliest())
        .max();
    match latest {
        Some(t) if is_same_imaging_day(&t, &now) => t,
        _ => now,
    }
}

/// `OpenLogs`' first debug-log record (`phd.cpp:411`). PHD2 follows it with
/// one indented line (`   …`) per OS and library version.
pub fn execution_banner(rollover: bool) -> String {
    format!("PHD2 version {PHD2_VERSION} {} execution with:\n", if rollover { "continues" } else { "begins" })
}

/// `wxMatchWild` for the one-`*` patterns the logs use, case-sensitive as
/// on Linux.
fn wild_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        Some((head, tail)) => name.len() >= head.len() + tail.len() && name.starts_with(head) && name.ends_with(tail),
        None => pattern == name,
    }
}

/// `Logger::RemoveMatchingFiles` (`logger.cpp:138`): of `files` (the log
/// folder's regular files and their modification times), deletes through
/// `remove` every one matching `pattern` last modified more than `days_old`
/// calendar days before `now`. Returns the debug-log record PHD2 writes
/// when it removed any.
pub fn remove_matching_files(
    files: &[(String, DateTime<Local>)],
    pattern: &str,
    days_old: u64,
    now: DateTime<Local>,
    remove: &mut dyn FnMut(&str),
) -> Option<String> {
    // wxDateSpan::Days counts calendar days: the wall time is kept across a
    // DST change.
    let oldest = now.checked_sub_days(Days::new(days_old)).unwrap_or(now - TimeDelta::days(days_old as i64));
    let mut hits = 0;
    for (name, mtime) in files {
        // wxFileModificationTime has whole seconds.
        if wild_match(pattern, name) && mtime.with_nanosecond(0).unwrap_or(*mtime) < oldest {
            hits += 1;
            remove(name);
        }
    }
    (hits > 0).then(|| format!("Removed {hits} files of pattern: {pattern}\n"))
}

/// `DebugLog::RemoveOldFiles`: debug logs older than [`RETENTION_DAYS`],
/// any instance (PHD2 runs it at start-up).
pub fn remove_old_files(
    files: &[(String, DateTime<Local>)],
    now: DateTime<Local>,
    remove: &mut dyn FnMut(&str),
) -> Option<String> {
    remove_matching_files(files, "PHD2_DebugLog*.txt", RETENTION_DAYS, now, remove)
}

/// `wxTimeSpan::Format("%S.%l")`: total seconds (two digits at least) and
/// milliseconds, a negative span spelled `-` then its magnitude.
fn span_text(ms: i64) -> String {
    let sign = if ms < 0 { "-" } else { "" };
    let ms = ms.unsigned_abs();
    format!("{sign}{:02}.{:03}", ms / 1000, ms % 1000)
}

/// PHD2's `DebugLog` (`debuglog.h`). `W` is the file (append mode) in the
/// daemon, a buffer in the tests.
pub struct DebugLog<W: Write = Box<dyn Write + Send>> {
    enabled: bool,
    file: Option<W>,
    /// `m_lastWriteTime`, kept in whole milliseconds like `wxDateTime::UNow`.
    last_write_ms: i64,
    /// `m_path`: the name of the file opened last, empty before the first.
    path: String,
}

impl<W: Write> DebugLog<W> {
    /// Off and with no file. `now` is what the first record's delta counts
    /// from: PHD2 starts it at construction.
    pub fn new(now: DateTime<Local>) -> Self {
        DebugLog { enabled: false, file: None, last_write_ms: now.timestamp_millis(), path: String::new() }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// `DebugLog::Enable`: turns writing on or off, file untouched; returns
    /// the previous state.
    pub fn enable(&mut self, enable: bool) -> bool {
        std::mem::replace(&mut self.enabled, enable)
    }

    /// `InitDebugLog` (`debuglog.cpp:98`): closes the file when logging was
    /// on, then, when `enable` and either no file was ever opened or
    /// `force_open`, opens `stamp`'s through `open` (append mode). Quirk
    /// kept: initialising again, enabled, without `force_open` closes the
    /// file and opens none, so the records that follow are lost. A failed
    /// open leaves logging on with no file, as in PHD2 (which says so in a
    /// message box); the error comes back.
    pub fn init(
        &mut self,
        enable: bool,
        force_open: bool,
        stamp: &LogFileStamp,
        open: impl FnOnce(&str) -> io::Result<W>,
    ) -> io::Result<()> {
        if self.enabled {
            if let Some(f) = &mut self.file {
                let _ = f.flush();
            }
            self.file = None;
            self.enabled = false;
        }
        let mut res = Ok(());
        if enable && (self.path.is_empty() || force_open) {
            self.path = stamp.file_name("DebugLog");
            match open(&self.path) {
                Ok(f) => self.file = Some(f),
                Err(e) => res = Err(e),
            }
        }
        self.enabled = enable;
        res
    }

    /// `DebugLog::Write`: one record, flushed. `text` carries its own
    /// newline; one without runs into the next record's prefix, as in PHD2.
    /// `thread` is the writing thread's id.
    pub fn write(&mut self, now: DateTime<Local>, thread: u64, text: &str) {
        if !self.enabled {
            return;
        }
        let now_ms = now.timestamp_millis();
        let delta = now_ms - std::mem::replace(&mut self.last_write_ms, now_ms);
        if let Some(f) = &mut self.file {
            let line = format!("{} {} {thread} {text}", now.format("%H:%M:%S%.3f"), span_text(delta));
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    /// `DebugLog::AddLine`: [`write`](Self::write) with a newline added.
    pub fn add_line(&mut self, now: DateTime<Local>, thread: u64, text: &str) {
        self.write(now, thread, &format!("{text}\n"));
    }

    /// `DebugLog::ChangeDirLog` (`debuglog.cpp:126`), after the caller tried
    /// to make `new_dir` the log folder (`dir_ok`, PHD2's `SetLogDir`): two
    /// records in the old file — "Changed log dir" even on failure, as in
    /// PHD2 — then the file reopened through `open`, in the new folder or
    /// the old one.
    pub fn change_dir_log(
        &mut self,
        now: DateTime<Local>,
        thread: u64,
        new_dir: &str,
        dir_ok: bool,
        stamp: &LogFileStamp,
        open: impl FnOnce(&str) -> io::Result<W>,
    ) -> io::Result<()> {
        if !dir_ok {
            self.write(now, thread, &format!("Error: unable to set new log dir {new_dir}\n"));
        }
        self.write(now, thread, &format!("Changed log dir to {new_dir}\n"));
        let enabled = self.enabled;
        self.init(enabled, true, stamp, open)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A shared buffer standing in for a file.
    #[derive(Clone, Default)]
    pub(in crate::engine::log) struct Buf(pub Rc<RefCell<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        pub(in crate::engine::log) fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    pub(in crate::engine::log) fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ms: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap() + TimeDelta::milliseconds(ms as i64)
    }

    #[test]
    fn imaging_day_turns_at_nine() {
        assert_eq!(imaging_day(&t(2026, 11, 13, 8, 59, 59, 999)), NaiveDate::from_ymd_opt(2026, 11, 12).unwrap());
        assert_eq!(imaging_day(&t(2026, 11, 13, 9, 0, 0, 0)), NaiveDate::from_ymd_opt(2026, 11, 13).unwrap());
        assert_eq!(imaging_day(&t(2026, 1, 1, 0, 0, 0, 0)), NaiveDate::from_ymd_opt(2025, 12, 31).unwrap());
        let mut stamp = LogFileStamp { instance: 1, time: t(2026, 11, 12, 20, 0, 0, 0) };
        assert!(!stamp.rollover(t(2026, 11, 13, 8, 59, 59, 0)));
        assert!(stamp.rollover(t(2026, 11, 13, 9, 0, 1, 0)));
        assert_eq!(stamp.time, t(2026, 11, 13, 9, 0, 1, 0));
    }

    #[test]
    fn file_names() {
        let mut stamp = LogFileStamp { instance: 1, time: t(2026, 11, 13, 1, 2, 3, 456) };
        assert_eq!(stamp.file_name("DebugLog"), "PHD2_DebugLog_2026-11-13_010203.txt");
        stamp.instance = 2;
        assert_eq!(stamp.file_name("GuideLog"), "PHD2_GuideLog_2_2026-11-13_010203.txt");
        assert_eq!(execution_banner(false), "PHD2 version 2.6.14 begins execution with:\n");
        assert_eq!(execution_banner(true), "PHD2 version 2.6.14 continues execution with:\n");
    }

    #[test]
    fn log_file_time_reuses_the_nights_newest_debug_log() {
        // The harness's folder (PHD2's GetLogFileTime on it).
        let names = [
            "PHD2_DebugLog_2026-11-12_200000.txt",
            "PHD2_DebugLog_2026-11-12_235959.txt",
            "PHD2_DebugLog_2_2026-11-13_010000.txt",
            "PHD2_DebugLog_2026-11-13_09000.txt",
            "PHD2_DebugLog_2026-11-12_080000.txt",
            "PHD2_GuideLog_2026-11-13_020000.txt",
        ];
        let at = |now| log_file_time(names, now);
        assert_eq!(at(t(2026, 11, 13, 8, 59, 59, 0)), t(2026, 11, 12, 23, 59, 59, 0));
        assert_eq!(at(t(2026, 11, 13, 9, 0, 0, 0)), t(2026, 11, 13, 9, 0, 0, 0));
        // PHD2 takes a "later" file of the same imaging day too.
        assert_eq!(at(t(2026, 11, 12, 9, 0, 0, 0)), t(2026, 11, 12, 23, 59, 59, 0));
    }

    #[test]
    fn records_and_deltas_match_phd2() {
        // The harness's run of PHD2's DebugLog, record for record.
        let buf = Buf::default();
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 13, 1, 2, 3, 0) };
        let mut log = DebugLog::new(t(2026, 11, 13, 1, 2, 3, 0));
        let mut opened = String::new();
        log.init(true, false, &stamp, |n| {
            opened = n.to_string();
            Ok(buf.clone())
        })
        .unwrap();
        assert_eq!(opened, "PHD2_DebugLog_2026-11-13_010203.txt");
        log.write(t(2026, 11, 13, 1, 2, 3, 42), 4242, &execution_banner(false));
        log.add_line(t(2026, 11, 13, 1, 2, 5, 3), 4242, "second line");
        log.write(t(2026, 11, 13, 1, 4, 12, 0), 4242, "after two minutes\n");
        log.write(t(2026, 11, 13, 1, 4, 11, 900), 4242, "clock stepped back\n");
        log.write(t(2026, 11, 13, 4, 6, 0, 7), 4242, "hours later\n");
        assert!(log.enable(false));
        log.write(t(2026, 11, 13, 4, 6, 0, 500), 4242, "dropped\n");
        log.enable(true);
        log.write(t(2026, 11, 13, 4, 6, 1, 7), 4242, "no newline");
        log.write(t(2026, 11, 13, 4, 6, 1, 7), 4242, " glued\n");
        assert_eq!(
            buf.text(),
            "01:02:03.042 00.042 4242 PHD2 version 2.6.14 begins execution with:\n\
             01:02:05.003 01.961 4242 second line\n\
             01:04:12.000 126.997 4242 after two minutes\n\
             01:04:11.900 -00.100 4242 clock stepped back\n\
             04:06:00.007 10908.107 4242 hours later\n\
             04:06:01.007 01.000 4242 no newline04:06:01.007 00.000 4242  glued\n"
        );
    }

    #[test]
    fn init_without_force_open_loses_the_file() {
        let buf = Buf::default();
        let stamp = LogFileStamp { instance: 1, time: t(2026, 11, 13, 1, 2, 3, 0) };
        let mut log = DebugLog::new(stamp.time);
        log.init(true, false, &stamp, |_| Ok(buf.clone())).unwrap();
        log.init(true, false, &stamp, |_| -> io::Result<Buf> { panic!("not reopened") }).unwrap();
        assert!(log.is_enabled());
        log.write(stamp.time, 1, "lost\n");
        assert_eq!(buf.text(), "");
        let moved = Buf::default();
        log.change_dir_log(stamp.time, 1, "/tmp/x", false, &stamp, |_| Ok(moved.clone())).unwrap();
        log.write(stamp.time, 1, "here\n");
        assert_eq!(moved.text(), "01:02:03.000 00.000 1 here\n");
    }

    #[test]
    fn retention_removes_old_matching_files() {
        // The harness's folder at 2026-11-13 12:00: PHD2 removed the first
        // three guide logs and the first debug log.
        let files: Vec<(String, DateTime<Local>)> = [
            ("PHD2_GuideLog_2026-09-01_200000.txt", t(2026, 9, 14, 11, 59, 59, 0)),
            ("PHD2_GuideLog_2_2026-09-02_200000.txt", t(2026, 8, 1, 0, 0, 0, 0)),
            ("PHD2_GuideLogX.txt", t(2026, 1, 1, 0, 0, 0, 0)),
            ("PHD2_GuideLog_2026-09-14_200000.txt", t(2026, 9, 14, 12, 0, 0, 0)),
            ("PHD2_GuideLog_2026-09-01_200000.log", t(2026, 1, 1, 0, 0, 0, 0)),
            ("PHD2_DebugLog_2026-10-13_200000.txt", t(2026, 10, 14, 11, 59, 59, 0)),
            ("PHD2_DebugLog_2026-10-14_200000.txt", t(2026, 10, 14, 12, 0, 1, 0)),
            ("phd2_debuglog_2026-01-01_000000.txt", t(2026, 1, 1, 0, 0, 0, 0)),
        ]
        .into_iter()
        .map(|(n, m)| (n.to_string(), m))
        .collect();
        let now = t(2026, 11, 13, 12, 0, 0, 0);
        let mut removed = Vec::new();
        let line = super::super::guide_log::remove_old_files(&files, now, &mut |n| removed.push(n.to_string()));
        assert_eq!(line.as_deref(), Some("Removed 3 files of pattern: PHD2_GuideLog*.txt\n"));
        let line = remove_old_files(&files, now, &mut |n| removed.push(n.to_string()));
        assert_eq!(line.as_deref(), Some("Removed 1 files of pattern: PHD2_DebugLog*.txt\n"));
        assert_eq!(
            removed,
            [
                "PHD2_GuideLog_2026-09-01_200000.txt",
                "PHD2_GuideLog_2_2026-09-02_200000.txt",
                "PHD2_GuideLogX.txt",
                "PHD2_DebugLog_2026-10-13_200000.txt"
            ]
        );
        assert_eq!(remove_old_files(&files[..5], now, &mut |_| panic!()), None);
    }
}
