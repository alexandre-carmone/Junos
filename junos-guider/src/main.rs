use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;
use std::process::exit;

use clap::Parser;
use junos_guider::config::Config;
use junos_guider::daemon::files::{self, DiskFiles};
use junos_guider::daemon::{Daemon, DaemonOptions};
use junos_guider::engine::EngineConfig;
use junos_guider::engine::calibration::ProfileAccess;
use junos_guider::profile::ProfileStore;
use junos_guider::server::{EventServer, ServerOptions};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    let cfg = Config::parse();
    // Before anything else, --reset included.
    if cfg.sim && !cfg!(feature = "sim") {
        tracing::error!("--sim needs junos-guider built with the `sim` feature");
        exit(1);
    }
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
    // `Logger::GetLogDir`: --log-dir stands for the `/frame/LogDir` setting.
    let setting = match &cfg.log_dir {
        Some(dir) => dir.clone(),
        None => store.global().get_str("/frame/LogDir", ""),
    };
    // PHD2 starts its servers only in server mode (myframe.cpp:228, 353).
    let server_mode = store.global().get_bool("/ServerMode", true);
    let path = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
    let opts = DaemonOptions {
        engine: EngineConfig {
            instance: cfg.instance,
            log_dir: path(files::log_dir(&setting)),
            darks_dir: path(cfg.darks_dir()),
            data_dir: path(cfg.data_path()),
        },
        sim: cfg.sim,
        indi_host: cfg.indi_host.clone(),
        indi_port: cfg.indi_port,
    };
    tracing::info!(
        port = cfg.event_server_port(),
        ws = %cfg.ws_addr(),
        profile = %store.current_profile(),
        logs = %opts.engine.log_dir,
        "junos-guider starting"
    );
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!("cannot start the runtime: {e}");
            exit(1);
        }
    };
    rt.block_on(async {
        let daemon = Daemon::start(opts, store, Box::new(DiskFiles));
        // Guiding goes on without the relay's socket (Ekos and NINA use the
        // event server): a bind failure is only logged.
        let ws_addr = cfg.ws_addr();
        match junos_guider::ws::spawn(daemon.clone(), &ws_addr).await {
            Ok(addr) => tracing::info!(%addr, "relay socket listening"),
            Err(e) => tracing::error!(addr = %ws_addr, "cannot listen for the relay: {e}"),
        }
        let server = server_mode.then(|| {
            let port = cfg.event_server_port();
            let opts = ServerOptions { port, instance: cfg.instance, shutdown_exits: cfg.shutdown_exits };
            EventServer::start(daemon.clone(), opts)
        });
        if server.is_none() {
            tracing::info!("server mode is off (/ServerMode): no event server");
        }
        let exit = async {
            match &server {
                Some(s) => s.exit_requested().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = wait_for_signal() => {}
            _ = exit => tracing::info!("shutdown asked by a client"),
            // The engine thread panicked (logged, its logs closed): exit
            // with an error, so that a service is restarted.
            _ = daemon.failed() => std::process::exit(1),
        }
        tracing::info!("junos-guider stopping");
        daemon.shutdown().await;
        tokio::task::spawn_blocking(move || daemon.close()).await.ok();
    });
}

/// SIGINT or SIGTERM (a service stop).
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
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
