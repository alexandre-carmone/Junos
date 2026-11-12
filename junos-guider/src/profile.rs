//! The profile store: PHD2's configuration (`PhdConfig`, `ConfigSection`)
//! kept in one JSON file.
//!
//! PHD2 keeps every setting in one wxConfig tree. Global keys are absolute
//! (`/ServerMode`, `/frame/LogDir`, `/currentProfile`, `ConfigVersion`); an
//! equipment profile is the group `/profile/<id>`, whose `name` entry names
//! it, and [`ProfileStore::profile`] reads and writes under the current one.
//! Key names are PHD2's, verbatim, so an export from one imports in the
//! other.
//!
//! On Linux that wxConfig is a wxFileConfig, which keeps every value as text
//! (verified against PHD2's own `phdconfig.cpp` built with wx 3.2.11), and
//! the store keeps its rules although its JSON values are typed:
//!
//! - a double is written as its `%g` text, so it reads back rounded to six
//!   significant digits (`0.0123456789` → `0.0123457`);
//! - a getter parses whatever text is there: `GetInt` and `GetBoolean` with
//!   `strtol` after trimming trailing blanks (a bool is `!= 0`), `GetDouble`
//!   with `strtod` on the whole text, so `"750"` is a true boolean and
//!   `"1e+06"` no integer;
//! - keys are case-insensitive and a new key joins an existing group
//!   whatever case it names it with;
//! - entries and groups enumerate in case-insensitive order;
//! - wxFileConfig has no entry types, so PHD2 on Linux exports every entry
//!   as type 1 (String) — the import still takes all four types;
//! - reading a key creates its groups, empty: reading an unknown profile's
//!   name (`set_profile` with a bad id) leaves an empty `/profile/<id>` that
//!   `get_profiles` lists as "Profile <id>" until the next start, since an
//!   empty group is never saved.
//!
//! `${VAR}` expansion of read strings (wxConfig's default) is not
//! reproduced, nor are `strtod`'s hexadecimal forms.
//!
//! The file is `config.json` (`config-N.json` for instance N > 1) in a
//! directory the daemon picks: `{version, current_profile, global{key:
//! value}, profiles{"<id>": {name, values{key: value}}}}`, `version` being
//! `ConfigVersion` and `current_profile` `/currentProfile`. Like wxConfig's
//! `Flush`, [`ProfileStore::persist`] writes it only when something changed,
//! atomically (a temporary file renamed over it); [`ProfileStore::flush`] is
//! PHD2's `PhdConfig::Flush`, which also logs, for PHD2's four call sites
//! only — the daemon's own writes go through `persist`.
//!
//! Every change counts as PHD2's `EvtServer.NotifyConfigurationChange()`;
//! the daemon polls [`ProfileStore::take_change`] and sends one
//! `ConfigurationChange` event when it is set — PHD2's debouncer sends the
//! first change at once and swallows the rest until its event loop turns.
//! PHD2 drops the notifications made before its event server starts (the
//! debouncer does not exist yet, event_server.cpp:3021), so the daemon calls
//! `take_change` once when it starts the server, dropping the start-up's.
//! The getters' and the profile functions' debug-log lines collect in
//! [`ProfileStore::take_debug`]; [`ProfileStore::open`]'s are dropped, PHD2
//! building its config before it opens the debug log (phd.cpp:532, 557).
//! [`ProfileStore::profiles`] and the `*_entries` listings log nothing, for
//! the snapshots the daemon rebuilds.
//!
//! Ported from PHD2 src/phdconfig.h, src/phdconfig.cpp, src/phd.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use crate::engine::calibration::ProfileAccess;
use crate::engine::darks::{dark_lib_file_name, defect_map_file_name};
use crate::engine::log::cfmt::{fmt_f, fmt_g};

/// `PhdConfig::DefaultProfileName`.
pub const DEFAULT_PROFILE_NAME: &str = "My Equipment";

/// `PhdConfig::CURRENT_CONFIG_VERSION`, stored as `ConfigVersion`.
pub const CURRENT_CONFIG_VERSION: i64 = 2001;

/// First line of a profile export (`WriteProfile`).
pub const PROFILE_HEADER: &str = "PHD Profile 1";

/// First line of a settings export (`SaveAll`).
pub const CONFIG_HEADER: &str = "PHD Config 1";

/// PHD2's configuration: the global keys and every profile.
pub struct ProfileStore {
    path: PathBuf,
    /// Every entry by its case-folded absolute key: the key as first written
    /// (wxFileConfig keeps that spelling) and its value.
    entries: BTreeMap<String, (String, Value)>,
    /// The empty `/profile/<g>` groups reads created (folded names).
    phantoms: RefCell<BTreeSet<String>>,
    /// `Profile.m_prefix`: `/profile/<id>`, empty until a profile is selected.
    profile_prefix: String,
    /// `m_currentProfileId`.
    current: i32,
    is_new_instance: bool,
    dirty: bool,
    changes: u64,
    change_pending: bool,
    debug_lines: RefCell<Vec<String>>,
}

/// `ConfigSection`: the getters and setters of `pConfig->Global` (absolute
/// keys) or `pConfig->Profile` (keys under the current profile). The
/// [`ProfileAccess`] methods are the int / double / string ones.
pub struct Section<'a> {
    store: &'a mut ProfileStore,
    prefix: String,
}

impl Section<'_> {
    fn path(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    /// `GetLong`.
    pub fn get_i64(&self, name: &str, default: i64) -> i64 {
        self.store.get_long_at(&self.path(name), default)
    }

    /// `SetLong`.
    pub fn set_i64(&mut self, name: &str, value: i64) {
        let path = self.path(name);
        self.store.set_at(&path, Value::from(value));
    }

    /// `DeleteEntry`.
    pub fn delete_entry(&mut self, name: &str) {
        let key = fold(&absolute(&self.path(name)));
        // wxFileConfig is dirty only when an entry went; PHD2 notifies anyway.
        if self.store.entries.remove(&key).is_some() {
            self.store.dirty = true;
        }
        self.store.notify();
    }
}

impl ProfileAccess for Section<'_> {
    fn has(&self, key: &str) -> bool {
        self.store.entries.contains_key(&fold(&absolute(&self.path(key))))
    }

    fn get_f64(&self, key: &str, default: f64) -> f64 {
        let path = self.path(key);
        let value = self.store.read_double(&path).unwrap_or(default);
        self.store.debug(format!("GetDouble(\"{path}\", {}) returns {}", fmt_f(default, 6), fmt_f(value, 6)));
        value
    }

    fn get_i32(&self, key: &str, default: i32) -> i32 {
        let path = self.path(key);
        // GetInt reads a long and casts it, wrapping.
        let value = self.store.read_long(&path).unwrap_or(default as i64) as i32;
        self.store.debug(format!("GetInt(\"{path}\", {default}) returns {value}"));
        value
    }

    fn get_str(&self, key: &str, default: &str) -> String {
        self.store.get_str_at(&self.path(key), default)
    }

    fn set_f64(&mut self, key: &str, value: f64) {
        let path = self.path(key);
        self.store.set_at(&path, double_value(value));
    }

    /// `SetInt` is `SetLong`.
    fn set_i32(&mut self, key: &str, value: i32) {
        self.set_i64(key, value as i64);
    }

    fn set_str(&mut self, key: &str, value: &str) {
        let path = self.path(key);
        self.store.set_at(&path, Value::String(value.to_string()));
    }

    /// `GetBoolean`: wxFileConfig reads a long, `!= 0`.
    fn get_bool(&self, key: &str, default: bool) -> bool {
        let path = self.path(key);
        let value = self.store.read_long(&path).map_or(default, |l| l != 0);
        self.store.debug(format!("GetBoolean(\"{path}\", {}) returns {}", default as i32, value as i32));
        value
    }

    fn set_bool(&mut self, key: &str, value: bool) {
        let path = self.path(key);
        self.store.set_at(&path, Value::Bool(value));
    }

    /// `GetGroupNames`, in wxFileConfig's (case-insensitive) order.
    fn group_names(&self, group: &str) -> Vec<String> {
        self.store.children(&self.path(group)).0
    }

    fn delete_group(&mut self, group: &str) {
        let path = self.path(group);
        self.store.remove_group(&path);
        self.store.notify();
    }
}

impl ProfileStore {
    /// `PhdConfig::PhdConfig`: load [`file_path`](Self::file_path) when it
    /// exists, and stamp a new config with `ConfigVersion`. Nothing is
    /// written until [`persist`](Self::persist); a file that is not the
    /// store's JSON is an `InvalidData` error.
    pub fn open(dir: &Path, instance: u32) -> io::Result<ProfileStore> {
        let mut store = ProfileStore {
            path: Self::file_path(dir, instance),
            entries: BTreeMap::new(),
            phantoms: RefCell::default(),
            profile_prefix: String::new(),
            current: 0,
            is_new_instance: false,
            dirty: false,
            changes: 0,
            change_pending: false,
            debug_lines: RefCell::default(),
        };
        match fs::read_to_string(&store.path) {
            Ok(text) => store.load_json(&text)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if store.get_long_at("ConfigVersion", 0) == 0 {
            store.is_new_instance = true;
            store.debug(format!("Initializing a new config, m_pConfig={:p}", &store.entries));
            store.set_at("ConfigVersion", Value::from(CURRENT_CONFIG_VERSION));
        }
        // PHD2 builds its config before the debug log is open, which drops
        // these lines (debuglog.cpp:184).
        store.debug_lines.take();
        Ok(store)
    }

    /// The file [`open`](Self::open) reads in `dir`: `config.json`, or
    /// `config-<instance>.json` from instance 2.
    pub fn file_path(dir: &Path, instance: u32) -> PathBuf {
        dir.join(if instance > 1 { format!("config-{instance}.json") } else { "config.json".to_string() })
    }

    /// `pConfig->Global`.
    pub fn global(&mut self) -> Section<'_> {
        Section { store: self, prefix: String::new() }
    }

    /// `pConfig->Profile`: keys under the current profile.
    pub fn profile(&mut self) -> Section<'_> {
        let prefix = self.profile_prefix.clone();
        Section { store: self, prefix }
    }

    /// `IsNewInstance`: the config had no `ConfigVersion` (first start, or
    /// after [`delete_all`](Self::delete_all)).
    pub fn is_new_instance(&self) -> bool {
        self.is_new_instance
    }

    /// `ConfigurationChange` notifications so far.
    pub fn change_count(&self) -> u64 {
        self.changes
    }

    /// Whether anything changed since the last call: the daemon's cue to send
    /// one `ConfigurationChange` event.
    pub fn take_change(&mut self) -> bool {
        std::mem::take(&mut self.change_pending)
    }

    /// The debug-log lines written since the last call, without newlines.
    pub fn take_debug(&mut self) -> Vec<String> {
        self.debug_lines.take()
    }

    /// `PhdConfig::Flush`: its debug line, then [`persist`](Self::persist).
    pub fn flush(&mut self) -> io::Result<()> {
        self.debug("PhdConfig flush");
        self.persist()
    }

    /// wxFileConfig's write: the file when something changed — to a
    /// temporary file first, renamed over the old one so a crash never
    /// leaves half a config. Logs nothing.
    pub fn persist(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(&self.to_json()).map_err(io::Error::other)?;
        // One temporary name per process, so another writer never truncates ours.
        let name = self.path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        let written = (|| {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        // The rename lasts once the directory is on disk too.
        fs::File::open(dir)?.sync_all()?;
        self.dirty = false;
        Ok(())
    }

    /// `InitializeProfile`: select `/currentProfile`, else the first profile,
    /// else a new "My Equipment".
    pub fn initialize_profile(&mut self) {
        let mut current = self.global().get_i32("/currentProfile", 0);
        if current <= 0 {
            current = self.first_profile();
        }
        if current <= 0 {
            self.create_profile(DEFAULT_PROFILE_NAME);
            current = self.profile_id(DEFAULT_PROFILE_NAME);
        }
        self.select(current);
        // In case it was just created.
        self.global().set_i32("/currentProfile", current);
    }

    /// `DeleteAll` (PHD2's `-R`): forget everything, then start over with a
    /// fresh "My Equipment". `ConfigVersion` is not written back, so the
    /// next start sees a new instance, as PHD2's does.
    pub fn delete_all(&mut self) {
        self.debug("Deleting all configuration data");
        self.clear();
        self.initialize_profile();
        self.is_new_instance = true;
    }

    /// `PhdApp::ResetConfiguration` (PHD2's `-R`): the dark library and the
    /// defect map of each profile, from `darks_dir`, then
    /// [`delete_all`](Self::delete_all). Quirk kept: PHD2 counts ids from 0
    /// to `NumProfiles() - 1` instead of taking the profiles' own, so a lone
    /// profile 1 keeps its files. The defect map's master darks stay, as in
    /// PHD2.
    pub fn reset_configuration(&mut self, darks_dir: &Path, instance: u32) {
        for id in 0..self.num_profiles() as i32 {
            let lib = darks_dir.join(dark_lib_file_name(instance as i32, id));
            if lib.exists() {
                self.debug(format!("Removing dark library file: {}", lib.display()));
                let _ = fs::remove_file(&lib);
            }
            let map = darks_dir.join(defect_map_file_name(instance as i32, id));
            if map.exists() {
                self.debug(format!("Removing defect map file: {}", map.display()));
                let _ = fs::remove_file(&map);
            }
        }
        self.delete_all();
    }

    /// `GetCurrentProfileId`.
    pub fn current_profile_id(&self) -> i32 {
        self.current
    }

    /// `GetCurrentProfile`: the current profile's name.
    pub fn current_profile(&self) -> String {
        self.profile_name(self.current)
    }

    /// `SetCurrentProfile`: select the profile named `name` (any case),
    /// creating it when there is none. True when it could not be created.
    pub fn set_current_profile(&mut self, name: &str) -> bool {
        if fold(&self.profile_name(self.current)) == fold(name) {
            return false;
        }
        let mut id = self.profile_id(name);
        if id <= 0 {
            self.create_profile(name);
            id = self.profile_id(name);
            if id <= 0 {
                self.debug(format!("failed to create profile [{name}]!"));
                return true;
            }
        }
        self.select(id);
        self.global().set_i32("/currentProfile", id);
        false
    }

    /// `GetProfileId`: the id of the profile named `name` (any case), 0 when
    /// there is none.
    pub fn profile_id(&self, name: &str) -> i32 {
        for g in self.children("/profile").0 {
            if let Some(id) = to_long(&g)
                && fold(&self.profile_name(id as i32)) == fold(name)
            {
                return id as i32;
            }
        }
        0
    }

    /// `FirstProfile`: the first numeric `/profile` group in wxFileConfig's
    /// order — a string order, so `10` comes before `2`. 0 when none.
    pub fn first_profile(&self) -> i32 {
        self.children("/profile").0.iter().find_map(|g| to_long(g)).map_or(0, |id| id as i32)
    }

    /// `GetProfileName`: the stored name, or "Profile <id>" without one.
    pub fn profile_name(&self, id: i32) -> String {
        let name = self.get_str_at(&format!("/profile/{id}/name"), "");
        if name.is_empty() { format!("Profile {id}") } else { name }
    }

    /// [`Self::current_profile`] without its debug line, for the engine's
    /// snapshot: PHD2 has no such read, and its `GetString` line, written
    /// several times a frame, would fill the debug log.
    pub fn current_profile_quiet(&self) -> String {
        self.quiet_name(self.current)
    }

    fn quiet_name(&self, id: i32) -> String {
        let key = fold(&format!("/profile/{id}/name"));
        let name = self.entries.get(&key).map(|(_, v)| wx_text(v)).unwrap_or_default();
        if name.is_empty() { format!("Profile {id}") } else { name }
    }

    /// `ProfileExists`: the profile has a name.
    pub fn profile_exists(&self, id: i32) -> bool {
        !self.get_str_at(&format!("/profile/{id}/name"), "").is_empty()
    }

    /// `CreateProfile`: a new profile on the first free id from 1. True when
    /// one of that name exists already.
    pub fn create_profile(&mut self, name: &str) -> bool {
        if self.profile_id(name) > 0 {
            return true;
        }
        let mut id = 1;
        while self.has_group(&format!("/profile/{id}")) {
            id += 1;
        }
        self.put(&format!("/profile/{id}/name"), Value::String(name.to_string()));
        self.notify();
        false
    }

    /// `CloneProfile`: a new profile `dest` holding a copy of `source`'s
    /// keys. True when `source` is missing or `dest` exists.
    pub fn clone_profile(&mut self, dest: &str, source: &str) -> bool {
        let src = self.profile_id(source);
        if src <= 0 {
            self.debug(format!("Clone profile could not clone {source}: profile not found"));
            return true;
        }
        if self.profile_id(dest) > 0 {
            self.debug(format!("Clone profile could not clone {source}: destination profile {dest} already exists"));
            return true;
        }
        if self.create_profile(dest) {
            return true;
        }
        let dst = self.profile_id(dest);
        if dst <= 0 {
            return true;
        }
        let depth = 2; // `/profile/<id>`
        let prefix = format!("/profile/{src}/");
        let copies: Vec<(String, Value)> = self
            .entries
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(_, (key, v))| (key.split('/').skip(depth + 1).collect::<Vec<_>>().join("/"), v.clone()))
            .collect();
        for (rest, v) in copies {
            self.put(&format!("/profile/{dst}/{rest}"), v);
        }
        // The copy overwrote the name.
        self.set_at(&format!("/profile/{dst}/name"), Value::String(dest.to_string()));
        false
    }

    /// `DeleteProfile`: drop the profile named `name`; the last one goes
    /// with a fresh "My Equipment" in its place, and the current one makes
    /// the first profile current.
    pub fn delete_profile(&mut self, name: &str) {
        self.debug(format!("Delete profile {name}"));
        let id = self.profile_id(name);
        if id <= 0 {
            return;
        }
        self.remove_group(&format!("/profile/{id}"));
        if self.num_profiles() == 0 {
            self.debug("Last profile deleted... create a new one");
            self.create_profile(DEFAULT_PROFILE_NAME);
        }
        if id == self.current {
            let first = self.first_profile();
            self.select(first);
            self.global().set_i32("/currentProfile", first);
        }
        self.notify();
    }

    /// `RenameProfile`. True when `new` is taken (any case, the profile
    /// itself included) or `old` does not exist.
    pub fn rename_profile(&mut self, old: &str, new: &str) -> bool {
        if self.profile_id(new) > 0 {
            self.debug(format!("error renaming profile {old} to {new}: new name already exists"));
            return true;
        }
        let id = self.profile_id(old);
        if id <= 0 {
            self.debug(format!("error renaming profile {old} to {new}: profile does not exist"));
            return true;
        }
        self.put(&format!("/profile/{id}/name"), Value::String(new.to_string()));
        self.notify();
        false
    }

    /// `ProfileNames`, in `/profile` group order.
    pub fn profile_names(&self) -> Vec<String> {
        self.children("/profile").0.iter().filter_map(|g| to_long(g)).map(|id| self.profile_name(id as i32)).collect()
    }

    /// `NumProfiles`: the numeric `/profile` groups.
    pub fn num_profiles(&self) -> u32 {
        self.children("/profile").0.iter().filter(|g| to_long(g).is_some()).count() as u32
    }

    /// `get_profiles` (event_server.cpp:654) without the debug lines its
    /// getters write: `(id, name, selected)` per numeric `/profile` group,
    /// the id being `GetProfileId(name)` — two profiles of one name both
    /// list the first one's id — and a profile whose id comes back 0 left
    /// out. Empty groups a read left are listed, as `Profile <id>`.
    pub fn profiles(&self) -> Vec<(i32, String, bool)> {
        let ids: Vec<i32> = self.children("/profile").0.iter().filter_map(|g| to_long(g)).map(|id| id as i32).collect();
        let name = |id: i32| self.quiet_name(id);
        ids.iter()
            .filter_map(|&g| {
                let n = name(g);
                let id = ids.iter().copied().find(|&i| fold(&name(i)) == fold(&n))?;
                (id != 0).then(|| (id, n, id == self.current))
            })
            .collect()
    }

    /// The current profile's entries, keys relative to it (`/scope/…`), in
    /// the store's (case-insensitive) order; no debug line.
    pub fn profile_entries(&self) -> impl Iterator<Item = (&str, &Value)> {
        let prefix = format!("/profile/{}/", self.current);
        self.entries.iter().filter_map(move |(folded, (key, v))| {
            folded.starts_with(&prefix).then_some(())?;
            Some((key.get(prefix.len() - 1..)?, v))
        })
    }

    /// The global entries (all but `/profile`), absolute keys; no debug line.
    pub fn global_entries(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().filter(|(folded, _)| !folded.starts_with("/profile/")).map(|(_, (key, v))| (key.as_str(), v))
    }

    /// `WriteProfile`: the current profile as a `PHD Profile 1` file —
    /// `key\ttype\tvalue` lines, keys relative to the profile, groups before
    /// entries at each level.
    pub fn write_profile(&self) -> String {
        let mut out = format!("{PROFILE_HEADER}\n");
        let group = format!("/profile/{}", self.current);
        self.write_group(&mut out, &group, &group);
        out
    }

    /// `ReadProfile`: import the `PHD Profile 1` file `file` read as `text`,
    /// as the profile named after the file without its folder and extension
    /// (`wxFileName::GetName`), replacing a profile of that name, and make it
    /// current. Its `/name` line is skipped; bad lines are skipped with a
    /// debug line. True on a wrong first line.
    pub fn read_profile(&mut self, file: &str, text: &str) -> bool {
        // wxTextInputStream::ReadLine ends a line at \n, \r or \r\n; the
        // empty pieces this split leaves are skipped like empty lines.
        let mut lines = text.split(['\r', '\n']);
        if lines.next() != Some(PROFILE_HEADER) {
            self.debug(format!("invalid profile file '{file}'"));
            return true;
        }
        let stem = Path::new(file).file_stem().map(|s| s.to_string_lossy().into_owned());
        let name = stem.as_deref().unwrap_or(file);
        let id = self.profile_id(name);
        if id > 0 {
            self.remove_group(&format!("/profile/{id}"));
        }
        self.create_profile(name);
        self.set_current_profile(name);
        let prefix = self.profile_prefix.clone();
        for line in lines {
            let Some((key, typ, val)) = parse_line(line) else { continue };
            // The file name names the profile.
            if key == "/name" {
                continue;
            }
            self.load_val(&prefix, line, &key, &typ, &val);
        }
        false
    }

    /// `SaveAll` (`export_config_settings`, PHD2's `-s`): every key as a
    /// `PHD Config 1` file.
    pub fn save_all(&self) -> String {
        let mut out = format!("{CONFIG_HEADER}\n");
        self.write_group(&mut out, "", "");
        out
    }

    /// [`save_all`](Self::save_all) to a file. True when it cannot be written.
    pub fn save_all_file(&self, path: &Path) -> bool {
        fs::write(path, self.save_all()).is_err()
    }

    /// `RestoreAll` (PHD2's `-l`): replace every key with a `PHD Config 1`
    /// file's. The selected profile stays as it was. True when the file
    /// cannot be read or its first line is wrong.
    pub fn restore_all_file(&mut self, path: &Path) -> bool {
        let shown = path.display();
        // wxMBConvUTF8 reads past bad bytes; a lossy decode comes closest.
        let Ok(bytes) = fs::read(path) else {
            self.debug(format!("Cannot open file '{shown}'"));
            return true;
        };
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.split(['\r', '\n']);
        if lines.next() != Some(CONFIG_HEADER) {
            self.debug(format!("invalid config file '{shown}'"));
            return true;
        }
        self.clear();
        for line in lines {
            let Some((key, typ, val)) = parse_line(line) else { continue };
            self.load_val("", line, &key, &typ, &val);
        }
        self.notify();
        false
    }

    // --- internals: the wxFileConfig tree over the flat map ---

    /// `EvtServer.NotifyConfigurationChange()`.
    fn notify(&mut self) {
        self.changes += 1;
        self.change_pending = true;
    }

    fn debug(&self, line: impl Into<String>) {
        self.debug_lines.borrow_mut().push(line.into());
    }

    /// `Profile.SelectProfile`.
    fn select(&mut self, id: i32) {
        self.current = id;
        self.profile_prefix = format!("/profile/{id}");
    }

    /// wxConfig's `DeleteAll`: no key, no group left.
    fn clear(&mut self) {
        self.entries.clear();
        self.phantoms.borrow_mut().clear();
        self.dirty = true;
    }

    /// A section setter: write, then notify.
    fn set_at(&mut self, path: &str, value: Value) {
        self.put(path, value);
        self.notify();
    }

    /// wxFileConfig's `Write`: an existing key (any case) keeps its spelling,
    /// a new one joins the existing groups on its path.
    fn put(&mut self, path: &str, value: Value) {
        let key = absolute(path);
        let folded = fold(&key);
        match self.entries.get_mut(&folded) {
            Some(entry) => entry.1 = value,
            None => {
                let key = self.canonical(&key);
                self.entries.insert(folded, (key, value));
            }
        }
        self.dirty = true;
    }

    /// `key` with its deepest existing group spelled as stored.
    fn canonical(&self, key: &str) -> String {
        let parts: Vec<&str> = key.split('/').collect();
        for depth in (2..parts.len()).rev() {
            let prefix = format!("{}/", fold(&parts[..depth].join("/")));
            if let Some((folded, (stored, _))) = self.entries.range(prefix.clone()..).next()
                && folded.starts_with(&prefix)
            {
                let mut out: Vec<&str> = stored.split('/').take(depth).collect();
                out.extend(&parts[depth..]);
                return out.join("/");
            }
        }
        key.to_string()
    }

    /// The value at `path`. A miss under a missing `/profile/<g>` leaves
    /// that group behind, empty, as wxFileConfig's getters do.
    fn lookup(&self, path: &str) -> Option<&Value> {
        let key = absolute(path);
        let found = self.entries.get(&fold(&key)).map(|(_, v)| v);
        if found.is_none() {
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() > 3 && fold(parts[1]) == "profile" && !self.has_group(&parts[..3].join("/")) {
                self.phantoms.borrow_mut().insert(fold(parts[2]));
            }
        }
        found
    }

    /// wxFileConfig's `DoReadLong`: the text with its trailing blanks
    /// trimmed, through `strtol`.
    fn read_long(&self, path: &str) -> Option<i64> {
        let v = self.lookup(path)?;
        v.as_i64().or_else(|| to_long(wx_text(v).trim_end_matches(is_c_space)))
    }

    /// wxConfigBase's `DoReadDouble`: the whole text through `strtod`.
    fn read_double(&self, path: &str) -> Option<f64> {
        match self.lookup(path)? {
            Value::Number(n) => n.as_f64(),
            v => to_double(&wx_text(v)),
        }
    }

    /// `GetLong` on a full path.
    fn get_long_at(&self, path: &str, default: i64) -> i64 {
        let value = self.read_long(path).unwrap_or(default);
        self.debug(format!("GetLong(\"{path}\", {default}) returns {value}"));
        value
    }

    /// `GetString` on a full path.
    fn get_str_at(&self, path: &str, default: &str) -> String {
        let value = self.lookup(path).map_or_else(|| default.to_string(), wx_text);
        self.debug(format!("GetString(\"{path}\", \"{default}\") returns \"{value}\""));
        value
    }

    /// `HasGroup`: an entry under `path`, or an empty group a read left.
    fn has_group(&self, path: &str) -> bool {
        let prefix = format!("{}/", fold(&absolute(path)));
        self.entries.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix))
            || prefix
                .strip_prefix("/profile/")
                .and_then(|g| g.strip_suffix('/'))
                .is_some_and(|g| self.phantoms.borrow().contains(g))
    }

    /// The child groups and entries of `group` ("" is the root), each in
    /// case-insensitive order, as wxFileConfig enumerates them.
    fn children(&self, group: &str) -> (Vec<String>, Vec<(String, &Value)>) {
        let group = group.trim_end_matches('/');
        let group = if group.is_empty() { String::new() } else { absolute(group) };
        let depth = group.split('/').count() - 1;
        let prefix = format!("{}/", fold(&group));
        // The map's folded-key order sorts the entries of one group; the
        // groups go through a map of their own, a folded full key comparing
        // `a-b/x` before `a/x`.
        let mut groups = BTreeMap::new();
        let mut entries = Vec::new();
        for (_, (key, v)) in self.entries.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)) {
            let parts: Vec<&str> = key.split('/').collect();
            if parts.len() == depth + 2 {
                entries.push((parts[depth + 1].to_string(), v));
            } else {
                let name = parts[depth + 1];
                groups.entry(fold(name)).or_insert_with(|| name.to_string());
            }
        }
        if fold(&group) == "/profile" {
            for g in self.phantoms.borrow().iter() {
                groups.entry(g.clone()).or_insert_with(|| g.clone());
            }
        }
        (groups.into_values().collect(), entries)
    }

    /// wxConfig's `DeleteGroup`: `path` and everything under it.
    fn remove_group(&mut self, path: &str) {
        let path = absolute(path);
        let prefix = format!("{}/", fold(&path));
        self.entries.retain(|k, _| !k.starts_with(&prefix));
        let folded = fold(&path);
        let mut phantoms = self.phantoms.borrow_mut();
        if folded == "/profile" {
            phantoms.clear();
        } else if let Some(g) = folded.strip_prefix("/profile/") {
            phantoms.remove(g);
        }
        self.dirty = true;
    }

    /// `WriteGroup`: the subgroups first, then the entries. wxFileConfig has
    /// no entry types (`GetEntryType` answers String for any entry), so PHD2
    /// on Linux writes every one as type 1 in its stored text; the Bool, Int
    /// (`%lu`) and Float (`%g`) branches only run on a typed backend.
    fn write_group(&self, out: &mut String, group: &str, prefix: &str) {
        let (groups, entries) = self.children(group);
        for g in groups {
            self.write_group(out, &format!("{group}/{g}"), prefix);
        }
        for (name, v) in entries {
            let key = format!("{group}/{name}");
            out.push_str(&format!("{}\t1\t{}\n", &key[prefix.len()..], escape(&wx_text(v))));
        }
    }

    /// `LoadVal`: one imported line through the section setters.
    fn load_val(&mut self, prefix: &str, line: &str, name: &str, typ: &str, val: &str) {
        let path = format!("{prefix}{name}");
        // wxConfigBase::EntryType: 1 String, 2 Boolean, 3 Integer, 4 Float.
        match to_long(typ) {
            Some(1) => self.set_at(&path, typed_text(unescape(val))),
            Some(2) => match to_long(val) {
                Some(l) => self.set_at(&path, Value::Bool(l != 0)),
                None => self.debug(format!("bad bool val '{val}' in file; line = {line}")),
            },
            Some(3) => match to_long(val) {
                Some(l) => self.set_at(&path, Value::from(l)),
                None => self.debug(format!("bad int val '{val}' in file; line = {line}")),
            },
            Some(4) => match to_double(val) {
                Some(d) => self.set_at(&path, double_value(d)),
                None => self.debug(format!("bad float val '{val}' in file; line = {line}")),
            },
            _ => self.debug(format!("bad type '{typ}' in file; line = {line}")),
        }
    }

    fn load_json(&mut self, text: &str) -> io::Result<()> {
        let file: ConfigFile = serde_json::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut put = |key: &str, v: Value| {
            if matches!(v, Value::String(_) | Value::Bool(_) | Value::Number(_)) {
                self.put(key, v);
            }
        };
        if let Some(v) = file.version {
            put("/ConfigVersion", v);
        }
        if let Some(v) = file.current_profile {
            put("/currentProfile", v);
        }
        for (k, v) in file.global {
            put(&k, v);
        }
        for (g, p) in file.profiles {
            if let Some(v) = p.name {
                put(&format!("/profile/{g}/name"), v);
            }
            for (k, v) in p.values {
                put(&format!("/profile/{g}{}", absolute(&k)), v);
            }
        }
        self.dirty = false;
        Ok(())
    }

    fn to_json(&self) -> ConfigFile {
        let mut file = ConfigFile::default();
        for (folded, (key, v)) in &self.entries {
            let v = v.clone();
            match folded.as_str() {
                "/configversion" => file.version = Some(v),
                "/currentprofile" => file.current_profile = Some(v),
                _ => match key.splitn(4, '/').collect::<Vec<_>>()[..] {
                    ["", group, g, rest] if fold(group) == "profile" => {
                        let p = file.profiles.entry(g.to_string()).or_default();
                        if fold(rest) == "name" {
                            p.name = Some(v);
                        } else {
                            p.values.insert(format!("/{rest}"), v);
                        }
                    }
                    _ => {
                        file.global.insert(key.clone(), v);
                    }
                },
            }
        }
        file
    }
}

/// The JSON file.
#[derive(Default, Serialize, Deserialize)]
struct ConfigFile {
    /// `ConfigVersion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<Value>,
    /// `/currentProfile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    current_profile: Option<Value>,
    #[serde(default)]
    global: BTreeMap<String, Value>,
    #[serde(default)]
    profiles: BTreeMap<String, ProfileFile>,
}

#[derive(Default, Serialize, Deserialize)]
struct ProfileFile {
    /// `/profile/<id>/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    /// The other keys, relative to the profile.
    #[serde(default)]
    values: BTreeMap<String, Value>,
}

/// wxFileConfig's keys are case-insensitive.
fn fold(s: &str) -> String {
    s.to_lowercase()
}

/// A key relative to the root (`ConfigVersion`) is that absolute one.
fn absolute(key: &str) -> String {
    if key.starts_with('/') { key.to_string() } else { format!("/{key}") }
}

fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// wxString's `ToLong` (base 10): `strtol` over the whole text, leading
/// blanks allowed, an overflow refused.
fn to_long(s: &str) -> Option<i64> {
    s.trim_start_matches(is_c_space).parse().ok()
}

/// wxString's `ToCDouble`: `strtod` over the whole text, leading blanks
/// allowed (no hexadecimal form).
fn to_double(s: &str) -> Option<f64> {
    s.trim_start_matches(is_c_space).parse().ok()
}

/// What `SetDouble` stores: wxConfig writes `%g` text, so the value is
/// rounded to six significant digits. A non-finite one stays text ("nan",
/// "inf"), which JSON cannot hold as a number.
fn double_value(v: f64) -> Value {
    let text = fmt_g(v);
    match text.parse().ok().and_then(Number::from_f64) {
        Some(n) => Value::Number(n),
        None => Value::String(text),
    }
}

/// A type-1 (String) line's value. PHD2 on Linux exports every entry as
/// type 1, so a number comes back as a JSON number when its text is the one
/// the store writes for it (`2001`, `0.5`, `1e+06`): the getters and the
/// exports read the same text either way, and the JSON keeps its types.
fn typed_text(s: String) -> Value {
    if let Ok(i) = s.parse::<i64>()
        && i.to_string() == s
    {
        return Value::from(i);
    }
    if let Some(n) = s.parse::<f64>().ok().filter(|&f| fmt_g(f) == s).and_then(Number::from_f64) {
        return Value::Number(n);
    }
    Value::String(s)
}

/// The text wxFileConfig would hold for a value: what the getters parse
/// and the exports write. A double is its `%g` text — exact for every
/// double [`double_value`] stored; a longer one from a hand-edited file
/// keeps all its digits.
fn wx_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap_or(0.0);
            let g = fmt_g(f);
            if g.parse::<f64>().ok() == Some(f) { g } else { n.to_string() }
        }
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// `escape_string`.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\r', "\\r").replace('\n', "\\n")
}

/// `unescape_string`: `\\`, `\t`, `\r`, `\n`; any other backslash stays.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let esc = match (c, chars.peek()) {
            ('\\', Some('\\')) => Some('\\'),
            ('\\', Some('t')) => Some('\t'),
            ('\\', Some('r')) => Some('\r'),
            ('\\', Some('n')) => Some('\n'),
            _ => None,
        };
        match esc {
            Some(e) => {
                out.push(e);
                chars.next();
            }
            None => out.push(c),
        }
    }
    out
}

/// `ParseLine`: name, type and value of a `key\ttype\tvalue` line, `None`
/// for an empty one. wxStringTokenizer on "\t\r\n" skips empty tokens; the
/// value is the rest after the type's delimiter, trailing blanks trimmed.
fn parse_line(s: &str) -> Option<(String, String, String)> {
    if s.is_empty() {
        return None;
    }
    let delim = |c: char| matches!(c, '\t' | '\r' | '\n');
    let mut rest = s;
    let mut token = || {
        let t = rest.trim_start_matches(delim);
        if t.is_empty() {
            return String::new();
        }
        let (tok, after) = match t.find(delim) {
            Some(i) => (&t[..i], &t[i + 1..]),
            None => (t, ""),
        };
        rest = after;
        tok.to_string()
    };
    let name = token();
    let typ = token();
    Some((name, typ, rest.trim_end_matches(is_c_space).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::algo::MountClass;
    use crate::engine::mount::Mount;

    fn dir(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-profile").join(name)
    }

    /// A store that is never flushed, after `InitializeProfile`.
    fn fresh() -> ProfileStore {
        let mut s = ProfileStore::open(&dir("never-written"), 1).unwrap();
        s.initialize_profile();
        s
    }

    /// `[name=id]` per profile, as the harness prints them.
    fn names(s: &ProfileStore) -> Vec<String> {
        s.profile_names().iter().map(|n| format!("{n}={}", s.profile_id(n))).collect()
    }

    /// The values the harness (PHD2's phdconfig.cpp on wx 3.2.11) set
    /// before exporting.
    fn harness_values(s: &mut ProfileStore) {
        let mut p = s.profile();
        p.set_str("/camera/LastMenuChoice", "INDI Camera [CCD Simulator]");
        p.set_str("/weird", "a\tb\\c\nd\re");
        p.set_bool("/scope/UseDecComp", true);
        p.set_bool("/guider/FastRecenter", false);
        p.set_i32("/ExposureDurationMs", -1);
        p.set_i32("/scope/CalibrationDuration", 750);
        p.set_i64("/scope/calibration/raGuideParity", -1);
        p.set_f64("/scope/calibration/xRate", 0.0123456789012345);
        p.set_f64("/scope/calibration/xAngle", -1.5707963267948966);
        p.set_f64("/frame/big", 1e6);
        p.set_f64("/frame/hundredk", 100000.0);
        p.set_f64("/frame/tiny", 1e-5);
        p.set_f64("/frame/half", 0.5);
        p.set_f64("/frame/third", 1.0 / 3.0);
        p.set_f64("/frame/huge", 1.5e300);
        p.set_f64("/frame/whole", 2.0);
        p.set_str("/scope/GuideAlgorithm/X/Hysteresis/minMove", "0.25");
        p.set_str("/Zeta", "z");
        p.set_str("/alpha", "a");
        p.set_str("/Beta/x", "b");
        let mut g = s.global();
        g.set_bool("/ServerMode", true);
        g.set_str("/frame/LogDir", "/home/u/PHD2");
    }

    /// PHD2's `WriteProfile` of [`harness_values`], from the harness.
    const HARNESS_EXPORT: &str = "PHD Profile 1
/Beta/x\t1\tb
/camera/LastMenuChoice\t1\tINDI Camera [CCD Simulator]
/frame/big\t1\t1e+06
/frame/half\t1\t0.5
/frame/huge\t1\t1.5e+300
/frame/hundredk\t1\t100000
/frame/third\t1\t0.333333
/frame/tiny\t1\t1e-05
/frame/whole\t1\t2
/guider/FastRecenter\t1\t0
/scope/calibration/raGuideParity\t1\t-1
/scope/calibration/xAngle\t1\t-1.5708
/scope/calibration/xRate\t1\t0.0123457
/scope/GuideAlgorithm/X/Hysteresis/minMove\t1\t0.25
/scope/CalibrationDuration\t1\t750
/scope/UseDecComp\t1\t1
/alpha\t1\ta
/ExposureDurationMs\t1\t-1
/name\t1\tMy Equipment
/weird\t1\ta\\tb\\\\c\\nd\\re
/Zeta\t1\tz
";

    /// A profile export written by hand in `phdconfig.cpp`'s format: the
    /// four entry types, escapes, and every kind of bad line.
    const HAND_EXPORT: &str = concat!(
        "PHD Profile 1\n",
        "/name\t1\tIgnored Name\n",
        "/camera/LastMenuChoice\t1\tINDI Camera [CCD Simulator]\n",
        "/weird\t1\ta\\tb\\\\c\\nd\\re\\q\\\n",
        "/scope/UseDecComp\t2\t1\n",
        "/guider/FastRecenter\t2\t0\n",
        "/b7\t2\t7\n",
        "/bbad\t2\tyes\n",
        "/scope/CalibrationDuration\t3\t750\n",
        "/neg\t3\t18446744073709551615\n",
        "/neg2\t3\t-1\n",
        "/ibad\t3\t12x\n",
        "/f\t4\t0.0123457\n",
        "/fbad\t4\tabc\n",
        "/t0\t0\tx\n",
        "/t9\t9\tx\n",
        "/tbad\tx\tx\n",
        "\n",
        "/notype\n",
        "/sp\t1\t  lead and trail  \n",
        "/empty\t1\t\n",
        "/tabs\t1\tv1\tv2\n",
        "/crlf\t3\t42\r\n",
        "/dbltab\t\t1\tq\n",
        "/a/b/c\t1\tdeep\n",
        "/last\t1\tno newline",
    );

    /// PHD2's `WriteProfile` after importing [`HAND_EXPORT`], from the
    /// harness.
    const HAND_REEXPORT: &str = "PHD Profile 1
/a/b/c\t1\tdeep
/camera/LastMenuChoice\t1\tINDI Camera [CCD Simulator]
/guider/FastRecenter\t1\t0
/scope/CalibrationDuration\t1\t750
/scope/UseDecComp\t1\t1
/b7\t1\t1
/crlf\t1\t42
/dbltab\t1\tq
/empty\t1\t
/f\t1\t0.0123457
/last\t1\tno newline
/name\t1\tImported
/neg2\t1\t-1
/sp\t1\t  lead and trail
/tabs\t1\tv1\\tv2
/weird\t1\ta\\tb\\\\c\\nd\\re\\\\q\\\\
";

    #[test]
    fn a_new_config_gets_my_equipment() {
        let mut s = ProfileStore::open(&dir("never-written"), 1).unwrap();
        assert!(s.is_new_instance());
        s.initialize_profile();
        assert_eq!(names(&s), ["My Equipment=1"]);
        assert_eq!((s.current_profile_id(), s.current_profile()), (1, "My Equipment".to_string()));
        // ConfigVersion, CreateProfile, /currentProfile.
        assert_eq!(s.change_count(), 3);
        assert!(s.take_change());
        assert!(!s.take_change());
        // The constructor's lines never reach PHD2's debug log.
        let debug = s.take_debug();
        assert_eq!(debug[..2], [
            "GetInt(\"/currentProfile\", 0) returns 0",
            "GetString(\"/profile/1/name\", \"\") returns \"My Equipment\""
        ]);
        assert_eq!(s.global().get_i64("ConfigVersion", 0), 2001);
    }

    #[test]
    fn getters_parse_the_text_like_wx_file_config() {
        let mut s = fresh();
        harness_values(&mut s);
        assert_eq!(s.change_count(), 25);
        let mut p = s.profile();
        // Doubles come back as their %g text.
        assert_eq!(p.get_f64("/scope/calibration/xRate", 0.0), 0.0123457);
        assert_eq!(p.get_f64("/frame/third", 0.0), 0.333333);
        assert_eq!(p.get_str("/scope/calibration/xRate", "d"), "0.0123457");
        // A string through the numeric getters.
        assert_eq!(p.get_i32("/scope/GuideAlgorithm/X/Hysteresis/minMove", 7), 7);
        assert_eq!(p.get_f64("/scope/GuideAlgorithm/X/Hysteresis/minMove", 7.0), 0.25);
        assert_eq!(p.get_f64("/camera/LastMenuChoice", 3.5), 3.5);
        assert_eq!(p.get_i32("/camera/LastMenuChoice", 4), 4);
        assert!(p.get_bool("/camera/LastMenuChoice", true));
        // Ints, bools and doubles through the others.
        assert!(p.get_bool("/scope/CalibrationDuration", false));
        assert_eq!(p.get_i32("/scope/UseDecComp", 9), 1);
        assert_eq!(p.get_str("/scope/UseDecComp", "d"), "1");
        assert_eq!(p.get_i32("/frame/big", 5), 5);
        assert_eq!(p.get_i32("/frame/whole", 5), 2);
        assert_eq!(p.get_f64("/ExposureDurationMs", 5.0), -1.0);
        // Case-insensitive keys; a group is no entry.
        assert_eq!(p.get_str("/CAMERA/lastmenuchoice", "d"), "INDI Camera [CCD Simulator]");
        assert!(p.has("/CAMERA/lastmenuchoice"));
        assert!(!p.has("/camera"));
        // The harness's odd texts.
        for (text, int, double, boolean) in [
            ("", 3, 3.5, true),
            (" 12 ", 12, 3.5, true),
            ("+5", 5, 5.0, true),
            ("true", 3, 3.5, true),
            ("4294967297", 1, 4294967297.0, true),
        ] {
            p.set_str("/odd", text);
            assert_eq!(p.get_i32("/odd", 3), int, "{text:?}");
            assert_eq!(p.get_f64("/odd", 3.5), double, "{text:?}");
            assert_eq!(p.get_bool("/odd", true), boolean, "{text:?}");
        }
        assert!(!p.get_bool("/odd2", false));
        p.set_str("/odd2", "true");
        assert!(!p.get_bool("/odd2", false));
        assert_eq!(p.get_i64("/odd2", 7), 7);
        // Non-finite doubles stay text.
        p.set_f64("/inf", f64::INFINITY);
        assert_eq!(p.get_f64("/inf", 0.0), f64::INFINITY);
        assert_eq!(p.get_str("/inf", ""), "inf");
        let debug = s.take_debug();
        assert!(debug.contains(&"GetDouble(\"/profile/1/scope/calibration/xRate\", 0.000000) returns 0.012346".into()));
        assert!(debug.contains(&"GetBoolean(\"/profile/1/scope/CalibrationDuration\", 0) returns 1".into()));
        assert!(debug.contains(&"GetInt(\"/profile/1/frame/whole\", 5) returns 2".into()));
    }

    #[test]
    fn a_new_key_joins_the_existing_group_and_keeps_the_first_spelling() {
        let mut s = fresh();
        let mut p = s.profile();
        p.set_str("/camera/LastMenuChoice", "a");
        p.set_str("/CAMERA/lastmenuchoice", "b");
        p.set_str("/Camera/Binning", "2");
        p.set_str("/GA/2026-11-10 21:00:00/snr", "12");
        assert_eq!(
            s.write_profile(),
            "PHD Profile 1\n/camera/Binning\t1\t2\n/camera/LastMenuChoice\t1\tb\n\
             /GA/2026-11-10 21:00:00/snr\t1\t12\n/name\t1\tMy Equipment\n"
        );
    }

    #[test]
    fn exports_the_profile_as_phd2_does() {
        let mut s = fresh();
        harness_values(&mut s);
        assert_eq!(s.write_profile(), HARNESS_EXPORT);
    }

    #[test]
    fn profiles_are_managed_as_phd2_does() {
        let mut s = fresh();
        harness_values(&mut s);
        let before = s.change_count();
        assert!(!s.create_profile("Second"));
        assert!(s.create_profile("second"));
        assert_eq!(names(&s), ["My Equipment=1", "Second=2"]);
        assert!(!s.clone_profile("Clone", "My Equipment"));
        assert_eq!(names(&s), ["My Equipment=1", "Second=2", "Clone=3"]);
        assert_eq!(s.global().get_str("/profile/3/scope/calibration/xRate", "none"), "0.0123457");
        assert!(s.clone_profile("x", "nope"));
        assert!(s.clone_profile("clone", "Second"));
        assert!(!s.rename_profile("Second", "Third"));
        assert!(s.rename_profile("Third", "clone"));
        assert!(s.rename_profile("nope", "x"));
        assert!(s.rename_profile("Third", "THIRD"));
        assert_eq!(names(&s), ["My Equipment=1", "Third=2", "Clone=3"]);
        assert!(!s.set_current_profile("Third"));
        assert!(!s.set_current_profile("THIRD"));
        assert_eq!(s.current_profile_id(), 2);
        s.delete_profile("My Equipment");
        assert_eq!(names(&s), ["Third=2", "Clone=3"]);
        assert_eq!(s.current_profile_id(), 2);
        // The first free id.
        assert!(!s.create_profile("New"));
        assert_eq!(names(&s), ["New=1", "Third=2", "Clone=3"]);
        // Deleting the current profile selects the first one.
        s.delete_profile("Third");
        assert_eq!(names(&s), ["New=1", "Clone=3"]);
        assert_eq!(s.current_profile_id(), 1);
        s.delete_profile("Clone");
        s.delete_profile("New");
        assert_eq!(names(&s), ["My Equipment=1"]);
        assert_eq!((s.current_profile_id(), s.current_profile()), (1, "My Equipment".to_string()));
        assert_eq!(s.global().get_i32("/currentProfile", 0), 1);
        // Counted against the harness: 13 notifications.
        assert_eq!(s.change_count() - before, 13);
        // Only the name survives a profile's re-creation.
        assert_eq!(s.write_profile(), "PHD Profile 1\n/name\t1\tMy Equipment\n");
        let debug = s.take_debug();
        assert!(debug.contains(&"error renaming profile Third to clone: new name already exists".into()));
        assert!(debug.contains(&"error renaming profile nope to x: profile does not exist".into()));
        assert!(debug.contains(&"Last profile deleted... create a new one".into()));
    }

    #[test]
    fn reading_an_unknown_profile_leaves_an_empty_group() {
        let mut s = fresh();
        // `set_profile {id: 9}`: GearDialog::SetProfile asks ProfileExists.
        assert!(!s.profile_exists(9));
        assert_eq!(s.profile_name(9), "Profile 9");
        assert_eq!(names(&s), ["My Equipment=1", "Profile 9=9"]);
        assert_eq!(s.num_profiles(), 2);
        assert!(!s.profile_exists(9));
        // Ids 2 to 8 are free, 9 is taken.
        for i in 2..=9 {
            assert!(!s.create_profile(&format!("P{i}")));
        }
        assert_eq!(s.profile_id("P9"), 10);
        // Nothing of it is saved.
        assert!(!s.to_json().profiles.contains_key("9"));
        s.delete_profile("Profile 9");
        assert!(!names(&s).contains(&"Profile 9=9".to_string()));
    }

    #[test]
    fn imports_a_phd2_export() {
        let mut s = fresh();
        let before = s.change_count();
        assert!(!s.read_profile("Imported", HAND_EXPORT));
        assert_eq!(names(&s), ["My Equipment=1", "Imported=2"]);
        assert_eq!(s.current_profile_id(), 2);
        let expected = [
            ("/name", Some("Imported")),
            ("/camera/LastMenuChoice", Some("INDI Camera [CCD Simulator]")),
            ("/weird", Some("a\tb\\c\nd\re\\q\\")),
            ("/scope/UseDecComp", Some("1")),
            ("/guider/FastRecenter", Some("0")),
            ("/b7", Some("1")),
            ("/bbad", None),
            ("/scope/CalibrationDuration", Some("750")),
            // PHD2's %lu spelling of -1 overflows strtol.
            ("/neg", None),
            ("/neg2", Some("-1")),
            ("/ibad", None),
            ("/f", Some("0.0123457")),
            ("/fbad", None),
            ("/t0", None),
            ("/t9", None),
            ("/tbad", None),
            ("/notype", None),
            ("/sp", Some("  lead and trail")),
            ("/empty", Some("")),
            ("/tabs", Some("v1\tv2")),
            ("/crlf", Some("42")),
            ("/dbltab", Some("q")),
            ("/a/b/c", Some("deep")),
            ("/last", Some("no newline")),
        ];
        let p = s.profile();
        for (key, value) in expected {
            assert_eq!(p.has(key).then(|| p.get_str(key, "")), value.map(String::from), "{key}");
        }
        // Typed lines keep their type.
        assert_eq!(p.get_i32("/scope/CalibrationDuration", 0), 750);
        assert!(!p.get_bool("/guider/FastRecenter", true));
        assert_eq!(s.write_profile(), HAND_REEXPORT);
        // CreateProfile, SetCurrentProfile and the 15 good lines.
        assert_eq!(s.change_count() - before, 17);
        let debug = s.take_debug();
        for line in [
            "bad bool val 'yes' in file; line = /bbad\t2\tyes",
            "bad int val '18446744073709551615' in file; line = /neg\t3\t18446744073709551615",
            "bad int val '12x' in file; line = /ibad\t3\t12x",
            "bad float val 'abc' in file; line = /fbad\t4\tabc",
            "bad type '0' in file; line = /t0\t0\tx",
            "bad type '9' in file; line = /t9\t9\tx",
            "bad type 'x' in file; line = /tbad\tx\tx",
            "bad type '' in file; line = /notype",
        ] {
            assert!(debug.contains(&line.to_string()), "{line}");
        }
        // Re-importing replaces the profile of that name.
        assert!(!s.read_profile("imported", "PHD Profile 1\r\n/x\t1\ty\r\n"));
        assert_eq!(names(&s), ["My Equipment=1", "imported=2"]);
        assert_eq!(s.write_profile(), "PHD Profile 1\n/name\t1\timported\n/x\t1\ty\n");
        // A wrong first line changes nothing.
        assert!(s.read_profile("Bad", "PHD Profile 2\n/x\t1\ty\n"));
        assert_eq!(names(&s), ["My Equipment=1", "imported=2"]);
    }

    #[test]
    fn an_export_imports_back() {
        let mut s = fresh();
        harness_values(&mut s);
        let text = s.write_profile();
        let mut t = fresh();
        assert!(!t.read_profile("My Equipment", &text));
        assert_eq!(t.write_profile(), text);
        let p = t.profile();
        assert_eq!(p.get_f64("/scope/calibration/xRate", 0.0), 0.0123457);
        assert_eq!(p.get_i32("/scope/calibration/raGuideParity", 0), -1);
        assert!(p.get_bool("/scope/UseDecComp", false));
        assert_eq!(p.get_str("/weird", ""), "a\tb\\c\nd\re");
    }

    #[test]
    fn save_all_and_restore_all() {
        let mut s = fresh();
        let mut g = s.global();
        g.set_bool("/ServerMode", true);
        g.set_str("/frame/LogDir", "/home/u/PHD2");
        s.read_profile("Imported", HAND_EXPORT);
        s.read_profile("crlfhdr", "PHD Profile 1\r\n/x\t1\ty\r\n");
        // PHD2's SaveAll in the same state, from the harness.
        let golden = "PHD Config 1
/frame/LogDir\t1\t/home/u/PHD2
/profile/1/name\t1\tMy Equipment
/profile/2/a/b/c\t1\tdeep
/profile/2/camera/LastMenuChoice\t1\tINDI Camera [CCD Simulator]
/profile/2/guider/FastRecenter\t1\t0
/profile/2/scope/CalibrationDuration\t1\t750
/profile/2/scope/UseDecComp\t1\t1
/profile/2/b7\t1\t1
/profile/2/crlf\t1\t42
/profile/2/dbltab\t1\tq
/profile/2/empty\t1\t
/profile/2/f\t1\t0.0123457
/profile/2/last\t1\tno newline
/profile/2/name\t1\tImported
/profile/2/neg2\t1\t-1
/profile/2/sp\t1\t  lead and trail
/profile/2/tabs\t1\tv1\\tv2
/profile/2/weird\t1\ta\\tb\\\\c\\nd\\re\\\\q\\\\
/profile/3/name\t1\tcrlfhdr
/profile/3/x\t1\ty
/ConfigVersion\t1\t2001
/currentProfile\t1\t3
/ServerMode\t1\t1
";
        assert_eq!(s.save_all(), golden);

        let d = dir("save-all");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let file = d.join("phd2_settings.txt");
        assert!(!s.save_all_file(&file));
        let mut t = fresh();
        t.profile().set_str("/gone", "x");
        let before = t.change_count();
        assert!(!t.restore_all_file(&file));
        // 23 lines and the final notification.
        assert_eq!(t.change_count() - before, 24);
        assert_eq!(t.save_all(), golden);
        assert_eq!(t.profile_names(), ["My Equipment", "Imported", "crlfhdr"]);
        // The selection stays; /currentProfile is the file's.
        assert_eq!(t.current_profile_id(), 1);
        assert_eq!(t.global().get_i32("/currentProfile", 0), 3);
        assert!(!t.profile().has("/gone"));
        assert!(t.restore_all_file(&d.join("missing.txt")));
        fs::write(d.join("bad.txt"), "PHD Profile 1\n").unwrap();
        assert!(t.restore_all_file(&d.join("bad.txt")));
        assert_eq!(t.save_all(), golden);
    }

    #[test]
    fn delete_all_starts_over() {
        let mut s = fresh();
        harness_values(&mut s);
        s.create_profile("Second");
        s.delete_all();
        assert!(s.is_new_instance());
        assert_eq!(names(&s), ["My Equipment=1"]);
        assert_eq!(s.save_all(), "PHD Config 1\n/profile/1/name\t1\tMy Equipment\n/currentProfile\t1\t1\n");
    }

    #[test]
    fn reset_deletes_the_dark_files_of_ids_0_to_n_minus_1() {
        let d = dir("reset");
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let files = |id: i32| [dark_lib_file_name(2, id), defect_map_file_name(2, id)];
        for id in 0..=2 {
            for f in files(id) {
                fs::write(d.join(f), "x").unwrap();
            }
        }
        fs::write(d.join("PHD2_defect_map_master_2_1.fit"), "x").unwrap();
        let mut s = fresh();
        s.create_profile("Second");
        s.take_debug();
        s.reset_configuration(&d, 2);
        // Two profiles: ids 0 and 1 go, 2 stays, the master dark stays.
        for id in 0..=2 {
            for f in files(id) {
                assert_eq!(d.join(&f).exists(), id == 2, "{f}");
            }
        }
        assert!(d.join("PHD2_defect_map_master_2_1.fit").exists());
        let debug = s.take_debug();
        let lib = d.join("PHD2_dark_lib_2_0.fit");
        assert_eq!(debug[0], format!("Removing dark library file: {}", lib.display()));
        assert!(debug.contains(&"Deleting all configuration data".to_string()));
        assert_eq!(names(&s), ["My Equipment=1"]);
    }

    #[test]
    fn group_names_and_delete_group_for_the_ga_history() {
        let mut s = fresh();
        let mut p = s.profile();
        for ts in ["2026-11-10 21:00:00", "2026-11-09 22:30:00", "2026-11-11 01:15:00"] {
            p.set_f64(&format!("/GA/{ts}/snr"), 12.5);
        }
        p.set_str("/GA/BLT/x", "1");
        p.set_str("/GA/loose", "1");
        assert_eq!(
            p.group_names("/GA"),
            ["2026-11-09 22:30:00", "2026-11-10 21:00:00", "2026-11-11 01:15:00", "BLT"]
        );
        assert!(p.group_names("/nothing").is_empty());
        let before = p.store.change_count();
        p.delete_group("/GA/2026-11-09 22:30:00");
        assert_eq!(p.store.change_count() - before, 1);
        assert_eq!(p.group_names("/GA"), ["2026-11-10 21:00:00", "2026-11-11 01:15:00", "BLT"]);
        p.delete_entry("/GA/loose");
        assert!(!p.has("/GA/loose"));
        // Deleting what is not there notifies but leaves nothing to write.
        s.dirty = false;
        let before = s.change_count();
        s.profile().delete_entry("/GA/none");
        assert_eq!((s.change_count() - before, s.dirty), (1, false));
    }

    #[test]
    fn mount_new_writes_the_algorithm_keys() {
        let mut s = fresh();
        s.take_change();
        let mut ev = Vec::new();
        let _mount = Mount::new(MountClass::Scope, &mut s.profile(), 0.0, &mut ev);
        assert!(s.take_change());
        let p = s.profile();
        assert_eq!(p.get_i32("/scope/XGuideAlgorithm", -1), 1);
        assert_eq!(p.get_i32("/scope/YGuideAlgorithm", -1), 4);
        for (key, value) in [
            ("/scope/GuideAlgorithm/X/Hysteresis/minMove", 0.2),
            ("/scope/GuideAlgorithm/X/Hysteresis/hysteresis", 0.1),
            ("/scope/GuideAlgorithm/X/Hysteresis/aggression", 0.7),
            ("/scope/GuideAlgorithm/Y/ResistSwitch/minMove", 0.2),
            ("/scope/GuideAlgorithm/Y/ResistSwitch/aggression", 1.0),
            ("/scope/GuideAlgorithm/Y/ResistSwitch/fastSwitch", 1.0),
        ] {
            assert_eq!(p.get_f64(key, f64::NAN), value, "{key}");
        }
        // A second mount reads them back.
        let mut ev = Vec::new();
        let _again = Mount::new(MountClass::Scope, &mut s.profile(), 0.0, &mut ev);
        assert!(s.take_debug().contains(&"GetInt(\"/profile/1/scope/XGuideAlgorithm\", 1) returns 1".to_string()));
    }

    #[test]
    fn the_json_file_round_trips() {
        let d = dir("json");
        let _ = fs::remove_dir_all(&d);
        let mut s = ProfileStore::open(&d, 1).unwrap();
        s.initialize_profile();
        harness_values(&mut s);
        s.create_profile("Second");
        s.flush().unwrap();
        assert!(d.join("config.json").exists());
        let names: Vec<_> = fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(names, ["config.json"]);
        assert_eq!(s.take_debug().last().map(String::as_str), Some("PhdConfig flush"));
        // persist logs nothing.
        s.profile().set_i32("/x", 1);
        s.persist().unwrap();
        assert!(s.take_debug().is_empty());
        let json: Value = serde_json::from_str(&fs::read_to_string(d.join("config.json")).unwrap()).unwrap();
        assert_eq!(json["version"], 2001);
        assert_eq!(json["current_profile"], 1);
        assert_eq!(json["global"]["/ServerMode"], true);
        assert_eq!(json["global"]["/frame/LogDir"], "/home/u/PHD2");
        assert_eq!(json["profiles"]["1"]["name"], "My Equipment");
        assert_eq!(json["profiles"]["2"]["name"], "Second");
        let values = &json["profiles"]["1"]["values"];
        assert_eq!(values["/scope/CalibrationDuration"], 750);
        assert_eq!(values["/scope/UseDecComp"], true);
        assert_eq!(values["/scope/calibration/xRate"], 0.0123457);
        assert_eq!(values["/camera/LastMenuChoice"], "INDI Camera [CCD Simulator]");
        assert_eq!(json["profiles"]["2"]["values"], serde_json::json!({}));

        let mut t = ProfileStore::open(&d, 1).unwrap();
        assert!(!t.is_new_instance());
        t.initialize_profile();
        assert_eq!(t.save_all(), s.save_all());
        // Another instance has its own file.
        let mut u = ProfileStore::open(&d, 2).unwrap();
        assert!(u.is_new_instance());
        u.initialize_profile();
        u.flush().unwrap();
        assert!(d.join("config-2.json").exists());
        assert_eq!(ProfileStore::open(&d, 1).unwrap().save_all(), s.save_all());

        fs::write(d.join("config.json"), "not json").unwrap();
        assert_eq!(ProfileStore::open(&d, 1).err().map(|e| e.kind()), Some(io::ErrorKind::InvalidData));
    }

    #[test]
    fn imported_numbers_stay_numbers() {
        let mut s = fresh();
        let text = "PHD Profile 1\n/i\t1\t750\n/neg\t1\t-1\n/f\t1\t0.0123457\n/e\t1\t1e+06\n/plus\t1\t+5\n\
                    /lead\t1\t007\n/long\t1\t0.123456789\n/nan\t1\tnan\n/s\t1\tabc\n";
        assert!(!s.read_profile("/home/u/PHD2/rig.phd", text));
        assert_eq!(s.current_profile(), "rig");
        let p = s.profile();
        let got = |k: &str| p.store.entries[&fold(&format!("/profile/2{k}"))].1.clone();
        assert_eq!(got("/i"), serde_json::json!(750));
        assert_eq!(got("/neg"), serde_json::json!(-1));
        assert_eq!(got("/f"), serde_json::json!(0.0123457));
        assert_eq!(got("/e"), serde_json::json!(1e6));
        for k in ["/plus", "/lead", "/long", "/nan", "/s"] {
            assert!(got(k).is_string(), "{k}");
        }
        // The same text comes back out.
        let mut sent: Vec<_> = text.lines().collect();
        let out = s.write_profile();
        let mut back: Vec<_> = out.lines().filter(|l| !l.starts_with("/name\t")).collect();
        sent.sort();
        back.sort();
        assert_eq!(back, sent);
        assert!(s.read_profile("/tmp/x.phd", "PHD Profile 2\n"));
        assert_eq!(s.take_debug().last().map(String::as_str), Some("invalid profile file '/tmp/x.phd'"));
    }

    #[test]
    fn engine_booleans_go_through_get_boolean() {
        use crate::engine::log::image_log::ImageLoggerSettings;
        let mut s = fresh();
        s.take_debug();
        let mut settings = ImageLoggerSettings::load(&s.profile());
        assert!(s.take_debug().contains(&"GetBoolean(\"/profile/1/ImageLogger/LoggingEnabled\", 0) returns 0".into()));
        settings.logging_enabled = true;
        settings.save(&mut s.profile());
        assert_eq!(s.to_json().profiles["1"].values["/ImageLogger/LoggingEnabled"], Value::Bool(true));
        // GetInt wraps, GetBoolean does not.
        s.profile().set_str("/big", "4294967296");
        assert!(s.profile().get_bool("/big", false));
        assert_eq!(s.profile().get_i32("/big", 7), 0);
    }

    #[test]
    fn quiet_listings() {
        let mut s = fresh();
        s.create_profile("Second");
        s.create_profile("second-dup");
        // A second profile of the same name (case aside), as a hand edit leaves.
        s.put("/profile/3/name", Value::String("SECOND".into()));
        s.profile_name(9); // an empty group
        s.take_debug();
        assert_eq!(
            s.profiles(),
            [(1, "My Equipment".into(), true), (2, "Second".into(), false), (2, "SECOND".into(), false), (9, "Profile 9".into(), false)]
        );
        assert_eq!(s.current_profile_quiet(), "My Equipment");
        assert!(s.take_debug().is_empty());
        s.profile().set_i32("/scope/CalibrationDuration", 750);
        s.global().set_bool("/ServerMode", true);
        let prof: Vec<_> = s.profile_entries().map(|(k, v)| (k.to_string(), v.clone())).collect();
        assert_eq!(prof, [("/name".into(), Value::from("My Equipment")), ("/scope/CalibrationDuration".into(), Value::from(750))]);
        let global: Vec<_> = s.global_entries().map(|(k, _)| k.to_string()).collect();
        assert_eq!(global, ["/ConfigVersion", "/currentProfile", "/ServerMode"]);
        assert!(s.take_debug().is_empty());
    }

    #[test]
    fn line_helpers() {
        assert_eq!(escape("a\tb\\c\nd\re"), "a\\tb\\\\c\\nd\\re");
        assert_eq!(unescape("a\\tb\\\\c\\nd\\re\\q\\"), "a\tb\\c\nd\re\\q\\");
        assert_eq!(parse_line("/x\t1\t\tv \t"), Some(("/x".into(), "1".into(), "\tv".into())));
        assert_eq!(parse_line("\t/x\t\t3\t5"), Some(("/x".into(), "3".into(), "5".into())));
        assert_eq!(parse_line(""), None);
        assert_eq!(double_value(f64::NAN), Value::String("nan".into()));
        assert_eq!(wx_text(&serde_json::json!(0.0123456789)), "0.0123456789");
    }
}
