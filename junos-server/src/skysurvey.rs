//! Sky-survey image proxy for the browser's Framing Assistant.
//!
//! `GET /api/skysurvey?ra=<deg>&dec=<deg>&fov=<deg>&w=<px>&h=<px>` fetches a
//! tangent-plane (TAN) cutout from CDS's `hips2fits` service server-side and
//! streams the JPEG back same-origin — hips2fits does not send permissive
//! CORS headers, so the browser cannot call it directly.

use axum::body::Bytes;
use axum::extract::Query;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tracing::warn;

const HIPS2FITS_URL: &str = "https://alaskybis.u-strasbg.fr/hips-image-services/hips2fits";
const DEFAULT_HIPS: &str = "CDS/P/DSS2/color";
const MAX_SIDE_PX: u32 = 2048;
const MIN_FOV_DEG: f64 = 0.01;
const MAX_FOV_DEG: f64 = 10.0;

#[derive(Debug, Deserialize)]
pub struct SkySurveyQ {
    pub ra: f64,
    pub dec: f64,
    pub fov: f64,
    #[serde(default = "default_side")]
    pub w: u32,
    #[serde(default = "default_side")]
    pub h: u32,
}

fn default_side() -> u32 {
    512
}

pub async fn skysurvey(Query(q): Query<SkySurveyQ>) -> Result<Response, StatusCode> {
    if !q.fov.is_finite() || q.fov < MIN_FOV_DEG || q.fov > MAX_FOV_DEG {
        return Err(StatusCode::BAD_REQUEST);
    }
    if !q.ra.is_finite() || !q.dec.is_finite() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let w = q.w.clamp(1, MAX_SIDE_PX);
    let h = q.h.clamp(1, MAX_SIDE_PX);

    let url = format!(
        "{HIPS2FITS_URL}?hips={hips}&width={w}&height={h}&fov={fov}&projection=TAN&coordsys=icrs&ra={ra}&dec={dec}&format=jpg",
        hips = DEFAULT_HIPS,
        fov = q.fov,
        ra = q.ra,
        dec = q.dec,
    );

    let resp = reqwest::get(&url).await.map_err(|e| {
        warn!("hips2fits fetch failed: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    if !resp.status().is_success() {
        warn!("hips2fits returned {}", resp.status());
        return Err(StatusCode::BAD_GATEWAY);
    }

    let bytes = resp.bytes().await.map_err(|e| {
        warn!("hips2fits body read failed: {e}");
        StatusCode::BAD_GATEWAY
    })?;

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "private, max-age=300".parse().unwrap());
    Ok((StatusCode::OK, headers, Bytes::from(bytes)).into_response())
}
