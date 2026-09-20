//! Server entry point: builds the shared state and mounts every route.

mod apps;
mod auth;
mod config;
mod dso_tiles;
mod files;
mod hub;
mod kstars_ws;
mod proxy;
mod skysurvey;
mod starfind;
mod tls;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::{
    extract::State,
    response::Json,
    routing::{delete, get, post},
    Router,
};
use clap::Parser;
use serde_json::json;
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::apps::AppManager;
use crate::config::Config;
use crate::hub::Hub;

#[derive(Clone)]
pub struct AppState {
    pub hub:         Hub,
    pub config:      Arc<Config>,
    pub app_manager: AppManager,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "junos_server=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(Config::parse());

    if let Ok(home) = std::env::var("HOME") {
        let seq_dir = std::path::Path::new(&home).join(".junos-sequences");
        let _ = std::fs::create_dir_all(&seq_dir);
    }

    info!("Serving frontend from: {}", config.dist_dir);

    // Files tab sandbox. Create it when missing so a fresh install (or a
    // service unit pointing at a folder KStars hasn't written to yet) doesn't
    // make every /api/files request fail with an opaque 500.
    {
        let captures = config.resolved_captures_dir();
        if !captures.is_dir() {
            if let Err(e) = std::fs::create_dir_all(&captures) {
                error!("Captures dir {} is missing and could not be created: {e} \
                        — the Files tab will not work", captures.display());
            }
        }
        match captures.canonicalize() {
            Ok(c) => info!("Files tab captures dir: {}", c.display()),
            Err(e) => error!("Files tab captures dir {} unusable: {e}", captures.display()),
        }
    }

    let hub = Hub::new();
    let app_manager = AppManager::new(hub.browser_tx.clone());
    app_manager.scan_existing().await;
    app_manager.clone().start_monitor();

    let state = AppState {
        hub,
        config: config.clone(),
        app_manager,
    };

    let dist_dir = config.dist_dir.clone();

    let app = Router::new()
        .route("/ws", get(proxy::ws_handler))
        .route("/api/config", get(api_config))
        .route("/api/authenticate", post(auth::authenticate))
        .route("/api/apps/launch", post(apps::launch_handler))
        .route("/api/apps/stop",   post(apps::stop_handler))
        .route("/api/apps/state",  get(apps::state_handler))
        .route("/api/skysurvey",   get(skysurvey::skysurvey))
        .route("/api/dso_tiles/index.json", get(dso_tiles::index))
        .route("/api/dso_tiles/:name",      get(dso_tiles::tile))
        .route("/message/ekos", get(kstars_ws::message_handler))
        .route("/media/ekos", get(kstars_ws::media_handler))
        .route("/api/files/list",     get(files::list))
        .route("/api/files/meta",     get(files::meta))
        .route("/api/files/thumb",    get(files::thumb))
        .route("/api/files/raw",      get(files::raw))
        .route("/api/files/download", get(files::download))
        .route("/api/files/rename",   post(files::rename))
        .route("/api/files/delete",   delete(files::delete))
        .route("/api/files/resolve",  get(files::resolve_abs))
        .route("/api/files/tilt",     get(files::tilt))
        .fallback_service(ServeDir::new(&dist_dir).append_index_html_on_directories(true))
        .with_state(state);

    // ── Bind before spawning ────────────────────────────────────────────
    //
    // Both listeners are bound here, in `main`, so that a failure to bind is a
    // startup error that propagates out of `main` with a non-zero exit status.
    // Binding inside the spawned tasks made a bind failure a `JoinError` that
    // `log_exit` only logged, after which `main` returned normally and the
    // process exited 0 — which `Restart=on-failure` in both packaged service
    // units (nix/module.nix, packaging/arch/junos-web.service) reads as a clean
    // shutdown and refuses to restart. A port still in TIME_WAIT after a reboot,
    // or a stray `cargo run`, then silently left the observatory unreachable.
    let http_addr: SocketAddr = config
        .http_addr
        .parse()
        .with_context(|| format!("--http-addr {:?} must parse as host:port", config.http_addr))?;
    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("failed to bind HTTP address {http_addr}"))?;
    info!("HTTP  (KStars)  → http://{}", http_addr);

    // TLS material and the HTTPS socket are also resolved up front, so a bad
    // cert path or an occupied :8443 fails the unit instead of leaving the
    // browser-facing listener quietly absent while HTTP keeps serving.
    let https_bound = if config.no_https {
        info!("HTTPS disabled (--no-https). Browsers will lose WebGPU support.");
        None
    } else {
        let tls_cfg = tls::ensure_cert(config.tls_cert.as_deref(), config.tls_key.as_deref())
            .await
            .context("failed to prepare TLS material")?;
        let https_addr: SocketAddr = config
            .https_addr
            .parse()
            .with_context(|| format!("--https-addr {:?} must parse as host:port", config.https_addr))?;
        // `std` listener: axum-server takes ownership and switches it to
        // non-blocking itself (`Listener::Std` in its `bind_incoming`).
        let https_listener = std::net::TcpListener::bind(https_addr)
            .with_context(|| format!("failed to bind HTTPS address {https_addr}"))?;
        info!("HTTPS (browser) → https://{}", https_addr);
        Some((https_listener, tls_cfg))
    };

    let http_app = app.clone();
    let http_task = tokio::spawn(async move {
        axum::serve(http_listener, http_app)
            .await
            .map_err(|e| anyhow::anyhow!("HTTP serve error: {e}"))
    });

    let https_task = https_bound.map(|(listener, tls_cfg)| {
        let https_app = app;
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls_cfg)
                .serve(https_app.into_make_service())
                .await
                .map_err(|e| anyhow::anyhow!("HTTPS serve error: {e}"))
        })
    });

    // If either listener exits (cleanly or otherwise), tear down the process.
    // We don't try to recover — both are critical.
    match https_task {
        Some(https) => tokio::select! {
            r = http_task  => log_exit("HTTP",  r),
            r = https      => log_exit("HTTPS", r),
        },
        None => {
            let r = http_task.await;
            log_exit("HTTP", r)
        }
    }
}

/// Turn a listener task's outcome into the process exit status. A listener that
/// errors or panics must leave `main` with `Err` so the exit code is non-zero;
/// only a clean exit returns `Ok`.
fn log_exit(
    name: &str,
    r: Result<Result<(), anyhow::Error>, tokio::task::JoinError>,
) -> anyhow::Result<()> {
    match r {
        Ok(Ok(())) => {
            info!("{name} listener exited cleanly");
            Ok(())
        }
        Ok(Err(e)) => {
            error!("{name} listener error: {e}");
            Err(e.context(format!("{name} listener failed")))
        }
        Err(e) => {
            error!("{name} task panicked: {e}");
            Err(anyhow::Error::new(e).context(format!("{name} listener task panicked")))
        }
    }
}

async fn api_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let dir = state.config.resolved_captures_dir();
    let dir = dir.canonicalize().unwrap_or(dir);
    Json(json!({
        "server": "junos-server",
        "captures_dir": dir.to_string_lossy(),
    }))
}
