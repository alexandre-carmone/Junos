//! Fake authentication endpoint that satisfies the Ekos Live client handshake.
//!
//! KStars POSTs `{"username","password","machine_id"}` to `/api/authenticate`
//! and expects a response with a JWT-ish token that it then passes as a query
//! parameter to the WebSocket URLs. We don't actually verify anything — this
//! is a local-network bridge — but we need to return a response shape that
//! KStars considers valid.
//!
//! See `kstars/kstars/ekos/ekoslive/nodemanager.cpp::onResult()` for the
//! fields KStars reads from the response.

use axum::response::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct AuthRequest {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub password: String,
    #[serde(default)]
    pub machine_id: String,
}

/// Stub JWT with `{"alg":"none"}.{"exp":9999999999}.` payload (expires ~2286).
/// KStars parses the middle segment to determine token expiry; a valid JWT-ish
/// shape avoids the "failed to parse token expiry" warning in KStars logs.
const STUB_JWT: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjk5OTk5OTk5OTl9.";

pub async fn authenticate(Json(req): Json<AuthRequest>) -> Json<Value> {
    info!(
        username = %req.username,
        machine_id = %req.machine_id,
        "KStars Ekos Live authentication"
    );

    let username = if req.username.is_empty() {
        "rekos".to_string()
    } else {
        req.username
    };

    Json(json!({
        "success":    true,
        "token":      STUB_JWT,
        "username":   username,
        "email":      "local@rekos.local",
        "from_date":  "2020-01-01",
        "to_date":    "2286-11-20",
        "plan_id":    "local",
        "type":       "offline",
        "machine_id": req.machine_id,
    }))
}
