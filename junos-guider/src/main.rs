use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;
use std::process::exit;

use clap::Parser;
use junos_guider::config::Config;
use junos_guider::profile::ProfileStore;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    let cfg = Config::parse();
    let dir = cfg.config_dir();
    // PHD2 refuses a second process of the same instance before anything
    // else (phd.cpp:508-517): one would lose the other's settings.
    let lock = ProfileStore::file_path(&dir, cfg.instance).with_extension("lock");
    let _lock = match lock_instance(&dir, &lock) {
        Ok(Some(f)) => Some(f),
        Ok(None) => {
            tracing::error!(
                "junos-guider instance {} is already running. Use the --instance INSTANCE_NUM command-line \
                 option to start a different instance.",
                cfg.instance
            );
            exit(1);
        }
        Err(e) => {
            tracing::warn!(lock = %lock.display(), "cannot take the instance lock: {e}");
            None
        }
    };
    let mut store = open_store(&dir, cfg.instance);
    // PHD2's -s and -l (phd.cpp:534-555) run first and exit, -s winning
    // when both are given. A save leaves the config file as it was: PHD2
    // exits before it would write it back.
    if let Some(file) = &cfg.save {
        exit(store.save_all_file(file) as i32);
    }
    if let Some(file) = &cfg.load {
        let err = store.restore_all_file(file) || store.flush().is_err();
        exit(err as i32);
    }
    if cfg.reset {
        store.reset_configuration(&cfg.darks_dir(), cfg.instance);
    }
    store.initialize_profile();
    // PHD2 writes its config at exit and at its own Flush calls; a daemon
    // writes it now, without PHD2's "PhdConfig flush" line.
    if let Err(e) = store.persist() {
        tracing::warn!(dir = %dir.display(), "cannot write the config: {e}");
    }
    // PHD2's event server drops the changes made before it starts.
    store.take_change();
    tracing::info!(
        port = cfg.event_server_port(),
        ws = %cfg.ws_addr,
        profile = %store.current_profile(),
        "junos-guider starting"
    );
}

/// An exclusive lock on `path`, held as long as the file stays open; `None`
/// when another process holds it.
fn lock_instance(dir: &Path, path: &Path) -> io::Result<Option<File>> {
    fs::create_dir_all(dir)?;
    let f = File::options().create(true).truncate(false).write(true).open(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// The profile store. A file that is not the store's JSON is set aside as
/// `<file>.bad` and the guider starts as a new instance, the way PHD2's
/// wxFileConfig skips what it cannot read and starts anyway: exiting would
/// leave `--load` and `--reset` no way in, and a service restarting for ever.
fn open_store(dir: &Path, instance: u32) -> ProfileStore {
    let file = ProfileStore::file_path(dir, instance);
    let err = match ProfileStore::open(dir, instance) {
        Ok(store) => return store,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            let bad = file.with_extension("json.bad");
            tracing::error!(file = %file.display(), "the config is not valid ({e}); moved to {}", bad.display());
            match fs::rename(&file, &bad) {
                Ok(()) => match ProfileStore::open(dir, instance) {
                    Ok(store) => return store,
                    Err(e) => e,
                },
                Err(e) => e,
            }
        }
        Err(e) => e,
    };
    tracing::error!(file = %file.display(), "cannot read the config: {err}");
    exit(1);
}
