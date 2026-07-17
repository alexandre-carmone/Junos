//! Shared DSO symbol geometry — angular size → screen half-axes + rotation.
//!
//! Single source of truth for `render::render_dso` (Canvas2D), `dso_render`
//! (GPU instances) and `picking` (hit radius). Symbols are drawn at the
//! object's true angular extent, so a wide object like M31 grows to fill the
//! view when zoomed in rather than staying a fixed-size glyph.

use crate::astro;
use crate::dso_catalog::Dso;

/// Screen-space geometry of one DSO symbol.
///
/// `half_w` is the major semi-axis, `half_h` the minor, both in CSS px.
/// `(cos_rot, sin_rot)` is the unit screen vector the **major axis** points
/// along (screen y grows downward).
#[derive(Copy, Clone)]
pub struct DsoShape {
    pub half_w: f64,
    pub half_h: f64,
    pub cos_rot: f64,
    pub sin_rot: f64,
}

/// Smallest symbol we draw — below this an outline stops being readable.
const MIN_HALF_PX: f64 = 3.0;
/// Below this the axis ratio is invisible, so skip the rotation math.
const ROT_MIN_HALF_PX: f64 = 4.0;
/// Offset used to probe the local north/east directions on screen.
const PROBE_DEG: f64 = 0.05;

/// Angular size (arcmin, full axis) → screen semi-axis in px.
fn arcmin_to_half_px(size_arcmin: f64, fov: f64, scale: f64) -> f64 {
    size_arcmin / 60.0 / (fov * 2.0) * scale
}

/// Compute the on-screen geometry of `dso`'s symbol.
///
/// `sx`/`sy` and `ra_jnow_deg`/`dec_jnow_deg` are the object's already-projected
/// screen position and its precessed coordinates — both are computed by every
/// caller anyway, so they're passed in rather than recomputed here. `project`
/// maps (alt, az) to screen px.
pub fn dso_shape(
    dso: &Dso,
    sx: f64,
    sy: f64,
    ra_jnow_deg: f64,
    dec_jnow_deg: f64,
    lst: f64,
    latitude: f64,
    fov: f64,
    scale: f64,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) -> DsoShape {
    let major = dso.size_arcmin as f64;
    let minor = dso.size_minor_arcmin as f64;

    // A missing minor axis (0.0 in the catalog) means "unknown" — draw round.
    let elongated = minor > 0.0 && minor < major;

    let raw_w = arcmin_to_half_px(major, fov, scale);
    let raw_h = if elongated {
        arcmin_to_half_px(minor, fov, scale)
    } else {
        raw_w
    };

    // Enforce the readability floor on the *major* axis and scale the minor by
    // the same factor: flooring each axis independently would stretch a small
    // round object into an ellipse.
    let (half_w, half_h) = if raw_w >= MIN_HALF_PX {
        (raw_w, raw_h)
    } else if raw_w > 0.0 {
        let k = MIN_HALF_PX / raw_w;
        (MIN_HALF_PX, raw_h * k)
    } else {
        // Size unknown (0 arcmin in the catalog) — a bare glyph is all we have.
        (MIN_HALF_PX, MIN_HALF_PX)
    };

    // Round symbols have no meaningful orientation, and at a few px the tilt
    // is invisible — skip the two extra projections in both cases.
    if !elongated || half_w < ROT_MIN_HALF_PX {
        return DsoShape {
            half_w,
            half_h,
            cos_rot: 0.0,
            sin_rot: -1.0,
        };
    }

    // Position angle is measured from north through east, so resolve both
    // directions on screen and combine. Deriving them from the projection
    // itself keeps the symbol correct under field rotation — this is an
    // alt/az view, where sky north only points up near the meridian.
    let north = probe(ra_jnow_deg, dec_jnow_deg + PROBE_DEG, sx, sy, lst, latitude, project);
    let cos_dec = dec_jnow_deg.to_radians().cos().abs().max(0.01);
    let east = probe(
        ra_jnow_deg + PROBE_DEG / cos_dec,
        dec_jnow_deg,
        sx,
        sy,
        lst,
        latitude,
        project,
    );

    let (Some((nx, ny)), Some((ex, ey))) = (north, east) else {
        return DsoShape {
            half_w,
            half_h,
            cos_rot: 0.0,
            sin_rot: -1.0,
        };
    };

    let pa = (dso.pa_deg as f64).to_radians();
    let dx = pa.cos() * nx + pa.sin() * ex;
    let dy = pa.cos() * ny + pa.sin() * ey;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-9 {
        return DsoShape {
            half_w,
            half_h,
            cos_rot: 0.0,
            sin_rot: -1.0,
        };
    }

    DsoShape {
        half_w,
        half_h,
        cos_rot: dx / len,
        sin_rot: dy / len,
    }
}

/// Project a point slightly offset from the object and return the normalized
/// screen direction from the object toward it. `None` when the probe falls
/// outside the projection (object right at the cull edge).
fn probe(
    ra_deg: f64,
    dec_deg: f64,
    sx: f64,
    sy: f64,
    lst: f64,
    latitude: f64,
    project: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
) -> Option<(f64, f64)> {
    let (alt, az) = astro::eq_to_altaz(ra_deg, dec_deg, lst, latitude);
    let (px, py) = project(alt, az)?;
    let dx = px - sx;
    let dy = py - sy;
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < 1e-9 {
        return None;
    }
    Some((dx / len, dy / len))
}
