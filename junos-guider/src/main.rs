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
    let mut store = match ProfileStore::open(&dir, cfg.instance) {
        Ok(store) => store,
        Err(e) => {
            tracing::error!(dir = %dir.display(), "cannot read the config: {e}");
            exit(1);
        }
    };
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
        store.delete_all();
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
