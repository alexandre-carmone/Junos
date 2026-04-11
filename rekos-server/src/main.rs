mod auth;
mod config;
mod hub;
mod kstars_ws;
mod proxy;

use std::sync::Arc;

use axum::{
    response::Json,
    routing::{get, post},
    Router,
};
use clap::Parser;
use serde_json::json;
use tower_http::services::ServeDir;
use tracing::info;
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

    info!("Serving frontend from: {}", config.dist_dir);
    info!("Listening on:          {}", config.bind_addr);
    info!("Point KStars Ekos Live 'offline' or 'online' server at: http://<this-host>:<port>");

    let state = AppState {
        hub:    Hub::new(),
        config: config.clone(),
    };

    let dist_dir = config.dist_dir.clone();

    let app = Router::new()
        // ── Browser ──────────────────────────────────────────────────
        .route("/ws", get(proxy::ws_handler))
        .route("/api/config", get(api_config))
        // ── KStars (inbound Ekos Live client) ────────────────────────
        .route("/api/authenticate", post(auth::authenticate))
        .route("/message/ekos", get(kstars_ws::message_handler))
        .route("/media/ekos", get(kstars_ws::media_handler))
        // ── Static WASM frontend ─────────────────────────────────────
        .fallback_service(ServeDir::new(&dist_dir).append_index_html_on_directories(true))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&config.bind_addr)
        .await
        .expect("Failed to bind address");

    info!("rekos-server ready — open http://{}", config.bind_addr);

    axum::serve(listener, app)
        .await
        .expect("Server error");
}

async fn api_config() -> Json<serde_json::Value> {
    Json(json!({ "server": "rekos-server" }))
}
