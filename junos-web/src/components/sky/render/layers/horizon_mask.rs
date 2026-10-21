//! Horizon mask: the sky hidden by walls and trees (`crate::horizon`),
//! shaded from each region's line down to the horizon (up to the zenith for
//! a ceiling), plus the region being drawn with its handles.
//!
//! Canvas2D in both modes, like the mosaic: the overlay canvas sits above
//! the WebGPU one and the GPU line pipeline has no filled primitive. The
//! fill is translucent so the stars under it stay readable.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{project_with, Frame, SkyLayer};
use crate::horizon::HorizonRegion;

/// Azimuth step of the shaded outline, degrees.
const STEP_DEG: f64 = 1.0;
/// Handle radius, CSS px — `mod.rs` hit-tests the handles with the same value.
pub const HANDLE_R: f64 = 6.0;

pub struct HorizonMaskLayer;

impl SkyLayer for HorizonMaskLayer {
    fn enabled(&self, f: &Frame) -> bool {
        let drawn = f.state.horizon.regions.iter().any(|r| r.enabled && r.points.len() >= 2);
        (f.toggles.horizon_mask_on && drawn) || f.state.horizon_draft.is_some()
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let view = *f.view;
        let proj = |alt: f64, az: f64| project_with(view, alt, az);
        let draft = f.state.horizon_draft.as_ref();

        if f.toggles.horizon_mask_on {
            for r in &f.state.horizon.regions {
                if !r.enabled || r.points.len() < 2 || draft.is_some_and(|d| d.name == r.name) {
                    continue;
                }
                let (fill, stroke, dash) = if r.ceiling {
                    ("rgba(200, 90, 40, 0.18)", "#d08050", true)
                } else {
                    ("rgba(200, 90, 40, 0.28)", "#d08050", false)
                };
                shade(ctx, &proj, &r.sampled(STEP_DEG), r.ceiling, fill, stroke, 2.0, dash);
            }
        }

        if let Some(d) = draft {
            let region = HorizonRegion { points: d.points.clone(), ..Default::default() };
            shade(ctx, &proj, &region.sampled(STEP_DEG), false,
                  "rgba(0, 200, 220, 0.18)", "#22d3ee", 2.0, false);
            for (i, (az, alt)) in d.points.iter().enumerate() {
                let Some((sx, sy)) = proj(*alt, *az) else { continue };
                let r = if d.drag == Some(i) { HANDLE_R * 1.6 } else { HANDLE_R };
                ctx.begin_path();
                let _ = ctx.arc(sx, sy, r, 0.0, std::f64::consts::TAU);
                ctx.set_fill_style_str("#22d3ee");
                ctx.fill();
                ctx.set_stroke_style_str("#ffffff");
                ctx.set_line_width(1.5);
                ctx.stroke();
            }
        }
    }
}

/// Fill between the line and the horizon (or the zenith for a ceiling), one
/// polygon per run of samples that project — `astro::project` gives up past
/// 1.5 × the field radius, and a polygon spanning that gap would snap across
/// the view when zoomed in — then stroke the line itself.
#[allow(clippy::too_many_arguments)]
fn shade(
    ctx: &CanvasRenderingContext2d,
    proj: &dyn Fn(f64, f64) -> Option<(f64, f64)>,
    line: &[(f64, f64)],
    ceiling: bool,
    fill: &str,
    stroke: &str,
    width: f64,
    dashed: bool,
) {
    // (top, bottom) per sample, both on screen.
    let columns: Vec<Option<((f64, f64), (f64, f64))>> = line
        .iter()
        .map(|&(az, alt)| {
            let (hi, lo) = if ceiling { (90.0, alt) } else { (alt.max(0.0), 0.0) };
            if hi <= lo {
                return None;
            }
            Some((proj(hi, az)?, proj(lo, az)?))
        })
        .collect();

    ctx.set_fill_style_str(fill);
    ctx.set_stroke_style_str(stroke);
    ctx.set_line_width(width);
    if dashed {
        let arr = js_sys::Array::new();
        arr.push(&6.0.into());
        arr.push(&4.0.into());
        let _ = ctx.set_line_dash(&arr);
    }

    let mut i = 0;
    while i < columns.len() {
        if columns[i].is_none() {
            i += 1;
            continue;
        }
        let start = i;
        while i < columns.len() && columns[i].is_some() {
            i += 1;
        }
        let run: Vec<_> = columns[start..i].iter().flatten().collect();
        if run.len() < 2 {
            continue;
        }
        ctx.begin_path();
        for (k, (top, _)) in run.iter().enumerate() {
            if k == 0 { ctx.move_to(top.0, top.1) } else { ctx.line_to(top.0, top.1) }
        }
        for (_, bottom) in run.iter().rev() {
            ctx.line_to(bottom.0, bottom.1);
        }
        ctx.close_path();
        ctx.fill();

        ctx.begin_path();
        for (k, (top, _)) in run.iter().enumerate() {
            if k == 0 { ctx.move_to(top.0, top.1) } else { ctx.line_to(top.0, top.1) }
        }
        ctx.stroke();
    }

    if dashed {
        let _ = ctx.set_line_dash(&js_sys::Array::new());
    }
}
