//! The engine's files on the disk ([`DiskFiles`]) and the log folder PHD2
//! picks at start-up ([`log_dir`]).
//!
//! Folders are made as PHD2 makes them: the log folder by `GetLogDir`, the
//! darks folder before each write (`GetDarksDir`), so a write or an append
//! makes its file's folder first.
//!
//! Ported from PHD2 src/logger.cpp (GetLogDir, DefaultDir) — BSD-3-Clause, see THIRD_PARTY.md

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::engine::Files;

/// The engine's files, on the disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskFiles;

/// `mkdir -p` of the folder `path` goes in.
fn make_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir),
        _ => Ok(()),
    }
}

impl Files for DiskFiles {
    fn append(&mut self, path: &str, tail: u64) -> io::Result<(Box<dyn Write + Send>, Vec<u8>)> {
        let path = Path::new(path);
        make_parent(path)?;
        let mut f = File::options().create(true).append(true).read(true).open(path)?;
        let len = f.metadata()?.len();
        let mut last = Vec::new();
        if tail > 0 && len > 0 {
            // Appends go to the end whatever the read position.
            f.seek(SeekFrom::Start(len.saturating_sub(tail)))?;
            (&mut f).take(tail).read_to_end(&mut last)?;
        }
        Ok((Box::new(f), last))
    }

    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    fn write(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        make_parent(Path::new(path))?;
        fs::write(path, bytes)
    }

    fn remove(&mut self, path: &str) {
        let _ = fs::remove_file(path);
    }

    /// `wxFileExists`: a file, not a folder.
    fn exists(&mut self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn mkdir(&mut self, path: &str) -> bool {
        fs::create_dir_all(path).is_ok() && Path::new(path).is_dir()
    }

    /// `wxDir::GetAllFiles(dir, …, wxDIR_FILES)`: the files directly in
    /// `dir`, links followed; names that are not UTF-8 left out (no PHD2
    /// log has one).
    fn list(&mut self, dir: &str) -> Vec<(String, DateTime<Local>)> {
        let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
        entries
            .flatten()
            .filter_map(|e| {
                let meta = fs::metadata(e.path()).ok().filter(|m| m.is_file())?;
                let name = e.file_name().into_string().ok()?;
                Some((name, DateTime::<Local>::from(meta.modified().ok()?)))
            })
            .collect()
    }
}

/// `$HOME`, `.` without one.
fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// `Logger::GetLogDir` at start-up: `setting` (`/frame/LogDir`, or the
/// `--log-dir` that stands for it), made with `mkdir -p` when missing;
/// when it is empty or cannot be made, `~/Documents/PHD2` (`DefaultDir`).
pub fn log_dir(setting: &str) -> PathBuf {
    resolve_log_dir(&home(), setting)
}

fn resolve_log_dir(home: &Path, setting: &str) -> PathBuf {
    let dir = PathBuf::from(setting);
    if !setting.is_empty() && (dir.is_dir() || fs::create_dir_all(&dir).is_ok()) {
        return dir;
    }
    // `DefaultDir`: the documents folder's PHD2, the documents folder
    // itself when that cannot be made.
    let docs = home.join("Documents");
    let dir = docs.join("PHD2");
    if dir.is_dir() || fs::create_dir_all(&dir).is_ok() { dir } else { docs }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::engine::log::debug_log::{LogFileStamp, is_same_imaging_day};
    use crate::engine::traits::SystemClock;
    use crate::engine::{Engine, EngineConfig};
    use crate::profile::ProfileStore;

    /// A fresh folder under `target/`.
    fn dir(name: &str) -> PathBuf {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-daemon").join(name);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn s(p: &Path) -> String {
        p.to_str().unwrap().to_string()
    }

    fn names(f: &mut DiskFiles, dir: &Path) -> Vec<String> {
        let mut n: Vec<String> = f.list(&s(dir)).into_iter().map(|(n, _)| n).collect();
        n.sort();
        n
    }

    #[test]
    fn files_on_the_disk() {
        let d = dir("files");
        let mut f = DiskFiles;
        let log = s(&d.join("logs/a.txt"));
        // A new file: its folder made, no tail.
        let (mut w, tail) = f.append(&log, 4).unwrap();
        assert!(tail.is_empty());
        w.write_all(b"first line\n").unwrap();
        drop(w);
        // Open again: the last bytes back, the next write at the end.
        let (mut w, tail) = f.append(&log, 5).unwrap();
        assert_eq!(tail, b"line\n");
        w.write_all(b"second\n").unwrap();
        drop(w);
        assert_eq!(f.read(&log).unwrap(), b"first line\nsecond\n");
        let (_, tail) = f.append(&log, 1000).unwrap();
        assert_eq!(tail, b"first line\nsecond\n");

        let dark = s(&d.join("darks/PHD2_dark_lib_1.fit"));
        f.write(&dark, b"SIMPLE").unwrap();
        assert!(f.exists(&dark));
        assert!(!f.exists(&s(&d.join("darks"))), "a folder is not a file");
        assert!(f.mkdir(&s(&d.join("frames/a/b"))));
        assert!(!f.mkdir(&s(&d.join("darks/PHD2_dark_lib_1.fit/x"))));

        f.write(&s(&d.join("logs/b.txt")), b"").unwrap();
        assert_eq!(names(&mut f, &d.join("logs")), ["a.txt", "b.txt"]);
        // Only files, not the sub-folders; a missing folder lists nothing.
        assert!(names(&mut f, &d).is_empty());
        assert!(f.list(&s(&d.join("nowhere"))).is_empty());

        f.remove(&dark);
        assert!(f.read(&dark).is_none());
        f.remove(&dark);
    }

    #[test]
    fn the_log_folder_falls_back_to_documents() {
        let d = dir("logdir");
        let home = d.join("home");
        // Never set: ~/Documents/PHD2, made.
        assert_eq!(resolve_log_dir(&home, ""), home.join("Documents/PHD2"));
        assert!(home.join("Documents/PHD2").is_dir());
        // A setting is made when missing.
        let mine = d.join("mine/logs");
        assert_eq!(resolve_log_dir(&home, mine.to_str().unwrap()), mine);
        assert!(mine.is_dir());
        // One that cannot be made (under a file): the default.
        fs::write(d.join("file"), b"").unwrap();
        assert_eq!(resolve_log_dir(&home, d.join("file/logs").to_str().unwrap()), home.join("Documents/PHD2"));
    }

    /// Ages a file's modification time by `days`.
    fn age(path: &Path, days: u64) {
        let t = SystemTime::now() - Duration::from_secs(days * 86_400);
        File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn start_up_removes_old_logs_and_reuses_the_nights() {
        let d = dir("startup");
        let logs = d.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let old_debug = logs.join("PHD2_DebugLog_2020-01-01_200000.txt");
        let kept_guide = logs.join("PHD2_GuideLog_2020-02-01_200000.txt");
        let old_guide = logs.join("PHD2_GuideLog_2020-01-01_200000.txt");
        let other = logs.join("notes.txt");
        for p in [&old_debug, &kept_guide, &old_guide, &other] {
            fs::write(p, b"x\n").unwrap();
        }
        // Debug logs are kept 30 days, guide logs 60.
        age(&old_debug, 31);
        age(&kept_guide, 59);
        age(&old_guide, 61);
        age(&other, 400);
        // The night's debug log, a second ago.
        let earlier = Local::now() - chrono::TimeDelta::seconds(1);
        let tonight = LogFileStamp { instance: 1, time: earlier }.file_name("DebugLog");
        fs::write(logs.join(&tonight), b"before\n").unwrap();

        let mut store = ProfileStore::open(&d.join("config"), 1).unwrap();
        store.initialize_profile();
        let cfg = EngineConfig { instance: 1, log_dir: s(&logs), darks_dir: s(&d.join("darks")), data_dir: s(&d) };
        drop(Engine::new(cfg, store, Box::new(SystemClock::default()), Box::new(DiskFiles)));

        assert!(!old_debug.exists() && !old_guide.exists());
        assert!(kept_guide.exists() && other.exists());
        let debug_name = if is_same_imaging_day(&earlier, &Local::now()) {
            // Appended to, not a new file.
            tonight
        } else {
            names(&mut DiskFiles, &logs).into_iter().rfind(|n| n.starts_with("PHD2_DebugLog_")).unwrap()
        };
        let debug = fs::read_to_string(logs.join(debug_name)).unwrap();
        assert!(debug.contains("PHD2 version 2.6.14 begins execution with:"), "{debug}");
        assert!(debug.contains("Removed 1 files of pattern: PHD2_DebugLog*.txt"), "{debug}");
        assert!(debug.contains("Removed 1 files of pattern: PHD2_GuideLog*.txt"), "{debug}");
    }
}
