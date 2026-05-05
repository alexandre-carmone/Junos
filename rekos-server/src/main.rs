mod auth;
mod config;
mod files;
mod hub;
mod kstars_ws;
mod proxy;
mod tls;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    response::Json,
    routing::{delete, get, post},
    Router,
};
use clap::Parser;
use serde_json::json;
use tower_http::services::ServeDir;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;
use crate::hub::Hub;

#[derive(Clone)]
pub struct AppState {
    pub hub:    Hub,
    pub config: Arc<Config>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "rekos_server=info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Arc::new(Config::parse());

    if let Ok(home) = std::env::var("HOME") {
        let seq_dir = std::path::Path::new(&home).join(".rekos-sequences");
        let _ = std::fs::create_dir_all(&seq_dir);
    }

    info!("Serving frontend from: {}", config.dist_dir);

    let state = AppState {
        hub:    Hub::new(),
        config: config.clone(),
    };

    let dist_dir = config.dist_dir.clone();

    let app = Router::new()
        .route("/ws", get(proxy::ws_handler))
        .route("/api/config", get(api_config))
        .route("/api/authenticate", post(auth::authenticate))
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
        .fallback_service(ServeDir::new(&dist_dir).append_index_html_on_directories(true))
        .with_state(state);

    let http_addr: SocketAddr = config
        .http_addr
        .parse()
        .expect("--http-addr must parse as host:port");
    info!("HTTP  (KStars)  → http://{}", http_addr);

    let http_app = app.clone();
    let http_task = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(http_addr)
            .await
            .expect("Failed to bind HTTP address");
        axum::serve(listener, http_app)
            .await
            .map_err(|e| anyhow::anyhow!("HTTP serve error: {e}"))
    });

    let https_task = if config.no_https {
        info!("HTTPS disabled (--no-https). Browsers will lose WebGPU support.");
        None
    } else {
        let tls_cfg = tls::ensure_cert(config.tls_cert.as_deref(), config.tls_key.as_deref())
            .await
            .expect("Failed to prepare TLS material");
        let https_addr: SocketAddr = config
            .https_addr
            .parse()
            .expect("--https-addr must parse as host:port");
        info!("HTTPS (browser) → https://{}", https_addr);
        let https_app = app;
        Some(tokio::spawn(async move {
            axum_server::bind_rustls(https_addr, tls_cfg)
                .serve(https_app.into_make_service())
                .await
                .map_err(|e| anyhow::anyhow!("HTTPS serve error: {e}"))
        }))
    };

    // If either listener exits (cleanly or otherwise), tear down the process.
    // We don't try to recover — both are critical.
    match https_task {
        Some(https) => tokio::select! {
            r = http_task  => log_exit("HTTP",  r),
            r = https      => log_exit("HTTPS", r),
        },
        None => {
            let r = http_task.await;
            log_exit("HTTP", r);
        }
    }
}

fn log_exit(
    name: &str,
    r: Result<Result<(), anyhow::Error>, tokio::task::JoinError>,
) {
    match r {
        Ok(Ok(())) => info!("{name} listener exited cleanly"),
        Ok(Err(e)) => error!("{name} listener error: {e}"),
        Err(e)     => error!("{name} task panicked: {e}"),
    }
}

async fn api_config() -> Json<serde_json::Value> {
    Json(json!({ "server": "rekos-server" }))
}
