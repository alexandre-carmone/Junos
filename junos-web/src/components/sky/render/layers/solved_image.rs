//! The last solve's Align frame, drawn at its solved centre, PA and scale so
//! its stars can be checked against the catalog's.
//!
//! Canvas2D in both pipeline modes. It is registered before the stars, so in
//! the fallback the stars and boxes paint over it; in GPU mode the overlay
//! sits above the GPU canvas, hence the user-set opacity.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{project_with, Frame, SkyLayer};

pub struct SolvedImageLayer;

impl SkyLayer for SolvedImageLayer {
    fn enabled(&self, f: &Frame) -> bool {
        f.toggles.solved_image_on
    }

    fn draw_canvas2d(&self, f: &mut Frame, ctx: &CanvasRenderingContext2d) {
        let Some(img) = f.state.solved_image.as_ref() else { return };
        let (iw, ih) = (img.el.natural_width() as f64, img.el.natural_height() as f64);
        let (hw, hh) = (img.fov_w_deg / 2.0, img.fov_h_deg / 2.0);
        if iw <= 0.0 || ih <= 0.0 || hw <= 0.0 || hh <= 0.0 {
            return;
        }
        let view = *f.view;
        let (lst, lat) = (f.scene.lst, f.scene.latitude);
        // Screen position of a point given as (right, up) image offsets in deg.
        let at = |dx: f64, dy: f64| {
            let (ra, dec) =
                crate::astro::frame_point_eq(img.ra_deg, img.dec_deg, img.pa_deg, dx, dy);
            let (alt, az) = crate::astro::eq_to_altaz(ra, dec, lst, lat);
            project_with(view, alt, az)
        };

        // One affine fit, anchored at the image point nearest the view centre:
        // exact there, and the projection's curvature over a camera field is
        // sub-pixel wherever the image is small enough to be seen whole.
        let (vra, vdec) = crate::astro::altaz_to_eq(view.c_alt, view.c_az, lst, lat);
        let (ax, ay) =
            crate::astro::frame_offset_of(img.ra_deg, img.dec_deg, img.pa_deg, vra, vdec)
                .map_or((0.0, 0.0), |(x, y)| (x.clamp(-hw, hw), y.clamp(-hh, hh)));
        let step = (view.fov * 0.25).min(hw).min(hh);
        let (Some(p0), Some(px), Some(py)) = (at(ax, ay), at(ax + step, ay), at(ax, ay + step))
        else {
            return;
        };
        // Screen px per image px, along the image's x (right) and y (down).
        let (kx, ky) = (2.0 * hw / iw / step, 2.0 * hh / ih / step);
        let (ux, uy) = ((px.0 - p0.0) * kx, (px.1 - p0.1) * kx);
        let (vx, vy) = ((p0.0 - py.0) * ky, (p0.1 - py.1) * ky);
        if (ux * vy - uy * vx).abs() < 1e-12 {
            return;
        }
        // Image centre on screen: back from the anchor by its offsets.
        let (sx, sy) = (ax / step, ay / step);
        let ex = p0.0 - sx * (px.0 - p0.0) - sy * (py.0 - p0.0);
        let ey = p0.1 - sx * (px.1 - p0.1) - sy * (py.1 - p0.1);

        ctx.save();
        ctx.set_global_alpha(f.state.solved_image_opacity.clamp(0.0, 1.0));
        let _ = ctx.transform(ux, uy, vx, vy, ex, ey);
        let _ = ctx.draw_image_with_html_image_element_and_dw_and_dh(
            &img.el,
            -iw / 2.0,
            -ih / 2.0,
            iw,
            ih,
        );
        ctx.restore();
    }
}
