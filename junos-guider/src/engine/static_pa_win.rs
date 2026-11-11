//! The Static Polar Alignment window as the engine drives it: open,
//! Rotate / the positions, Clear, the window's choices, close, and
//! [`StaticPa::update_state`] on each SELECTED frame through the gear it
//! reaches ([`StaticPaGear`]: the pointing source to slew, the guider to
//! read and lock on the star).
//!
//! Ported from PHD2 src/staticpa_toolwin.cpp (the window's calls into the frame), src/guider.cpp (UpdateGuideState's SELECTED branch) — BSD-3-Clause, see THIRD_PARTY.md

use serde_json::{Value, json};

use super::command::{CmdError, ManualPointing, Reply};
use super::guider::{Guider, PaFailure, PaTools};
use super::mount::PointingSource;
use super::tools::static_pa::{self, Adjustment, MapStar, StaticPa, StaticPaGear, StaticPaOpen};
use super::tools;
use super::traits::{PierSide, Point};
use super::{Engine, ScopeDev};

/// The window: the tool, and what it last drew (the adjustment and the
/// star map, worked out after each frame and each change).
pub(super) struct StaticPaWin {
    tool: StaticPa,
    adjustment: Option<Adjustment>,
    stars: Vec<MapStar>,
}

/// The window's choices (`OnManual`, `OnHemi`, `OnHa`, `OnFlip`,
/// `OnRefStar`); `None` leaves one as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StaticPaSet {
    pub manual: Option<bool>,
    pub north: Option<bool>,
    /// Hours.
    pub hour_angle: Option<f64>,
    pub flip: Option<bool>,
    pub ref_star: Option<usize>,
}

/// `pPointingSource`, to slew: the choice of [`super::pointing_source`].
pub(super) fn pointing_mut<'a>(
    scope: &'a mut Option<ScopeDev>,
    aux: &'a mut Option<(String, Box<dyn PointingSource + Send>)>,
    manual: &'a mut Option<ManualPointing>,
) -> Option<&'a mut dyn PointingSource> {
    let scope_reports = scope.as_ref().and_then(|s| s.pointing.as_ref()).is_some_and(|p| p.can_report_position());
    let aux_reports = aux.as_ref().is_some_and(|a| a.1.can_report_position());
    let sp = scope.as_mut().and_then(|s| s.pointing.as_deref_mut()).map(|p| p as &mut dyn PointingSource);
    let ap = aux.as_mut().map(|a| &mut *a.1 as &mut dyn PointingSource);
    if scope_reports {
        sp
    } else if aux_reports {
        ap
    } else if let Some(m) = manual.as_mut() {
        Some(m)
    } else {
        sp.or(ap)
    }
}

/// What the tool reaches while it rotates. Its lock move is made once it
/// returns, on the frame the slew started from.
struct PaGear<'a> {
    pointing: Option<&'a mut dyn PointingSource>,
    guider: &'a Guider,
    lock: bool,
}

impl PointingSource for PaGear<'_> {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        self.pointing.as_ref()?.coordinates()
    }
    fn pier_side(&self) -> PierSide {
        self.pointing.as_ref().map_or(PierSide::Unknown, |p| p.pier_side())
    }
    fn guide_rates(&self) -> Option<(f64, f64)> {
        self.pointing.as_ref()?.guide_rates()
    }
    fn is_slewing(&self) -> bool {
        self.pointing.as_ref().is_some_and(|p| p.is_slewing())
    }
    fn can_report_position(&self) -> bool {
        self.pointing.as_ref().is_some_and(|p| p.can_report_position())
    }
    fn can_slew_async(&self) -> bool {
        self.pointing.as_ref().is_some_and(|p| p.can_slew_async())
    }
    fn slew_to_async(&mut self, ra_hours: f64, dec_deg: f64) -> bool {
        self.pointing.as_mut().is_some_and(|p| p.slew_to_async(ra_hours, dec_deg))
    }
    fn abort_slew(&mut self) {
        if let Some(p) = self.pointing.as_mut() {
            p.abort_slew();
        }
    }
}

impl StaticPaGear for PaGear<'_> {
    fn current_position(&self) -> Option<Point> {
        self.guider.current_position()
    }
    /// Refused without a star; else made once the tool returns.
    fn lock_on_current_star(&mut self) -> bool {
        self.lock = self.guider.current_position().is_some();
        self.lock
    }
    fn search_region(&self) -> i32 {
        self.guider.max_move_pixels()
    }
    fn frame_size(&self) -> (i32, i32) {
        let img = self.guider.current_image();
        (img.w as i32, img.h as i32)
    }
}

impl Engine {
    /// `StaticPaToolWin()` after `CreateStaticPaToolWindow`'s checks (the
    /// pixel-scale prompt is the UI's): open, and loop when the camera is
    /// idle.
    pub(super) fn static_pa_open(&mut self) -> Reply {
        tools::open_check(self.camera_connected(), self.guider.is_calibrating_or_guiding())
            .map_err(|e| CmdError::new(1, e))?;
        if self.static_pa.is_none() {
            let pointing = self.pointing();
            let mount = self.primary_mount();
            let calibrated = mount.filter(|m| m.is_calibrated()).map(|m| (m.x_angle(), m.class()));
            let open = StaticPaOpen {
                pixel_scale: self.pixel_scale(),
                mount: calibrated,
                pier_side: match calibrated {
                    Some(_) => pointing.map_or(PierSide::Unknown, |p| p.pier_side()),
                    None => PierSide::Unknown,
                },
                site_lat: pointing.and_then(|p| p.site_lat_long()).map(|(lat, _)| lat),
                can_slew: pointing.is_some_and(|p| p.can_slew_async()),
                days: self.pa_days(),
                cam_width: match self.camera.as_ref().map_or(0, |c| c.frame_size().0) {
                    0 => self.guider.current_image().w as f64,
                    w => w as f64,
                },
            };
            let tool = StaticPa::new(&self.store.profile(), &open);
            self.static_pa = Some(StaticPaWin { tool, adjustment: None, stars: Vec::new() });
            self.static_pa_redraw();
        }
        if !self.ctrl.capture_active() {
            self.call(|c, cx, ev| c.start_looping(cx, ev));
        }
        Ok(json!(0))
    }

    fn static_pa_tool(&mut self) -> Result<&mut StaticPa, CmdError> {
        self.static_pa.as_mut().map(|w| &mut w.tool).ok_or_else(|| CmdError::new(1, "Static PA tool not open"))
    }

    /// Rotate: in auto mode starts, or stops while aligning; in manual
    /// mode "Get first position". A refusal is in the status.
    pub(super) fn static_pa_rotate(&mut self) -> Reply {
        let busy = self.guider.is_calibrating_or_guiding();
        let locked = self.guider.multistar().is_locked();
        self.static_pa_tool()?;
        let Engine { static_pa, guider, scope, aux, manual_pointing, .. } = self;
        let mut gear = PaGear { pointing: pointing_mut(scope, aux, manual_pointing), guider, lock: false };
        static_pa.as_mut().expect("open").tool.rotate(busy, locked, &mut gear);
        self.static_pa_redraw();
        Ok(json!(0))
    }

    /// "Get second / third position" (manual mode).
    pub(super) fn static_pa_mark(&mut self, position: i32) -> Reply {
        let t = self.static_pa_tool()?;
        match position {
            2 => t.second_position(),
            3 => t.third_position(),
            _ => return Err(CmdError::new(1, "invalid position")),
        }
        Ok(json!(0))
    }

    pub(super) fn static_pa_clear(&mut self) -> Reply {
        self.static_pa_tool()?.clear();
        self.static_pa_redraw();
        Ok(json!(0))
    }

    pub(super) fn static_pa_set(&mut self, set: StaticPaSet) -> Reply {
        self.static_pa_tool()?;
        let Engine { static_pa, store, .. } = self;
        let t = &mut static_pa.as_mut().expect("open").tool;
        let mut prof = store.profile();
        if let Some(m) = set.manual {
            t.set_manual(m);
        }
        if let Some(north) = set.north {
            t.set_hemisphere(&mut prof, north);
        }
        if let Some(h) = set.hour_angle {
            t.set_hour_angle(h);
        }
        if let Some(f) = set.flip {
            t.set_flip(f);
        }
        if let Some(i) = set.ref_star {
            t.set_ref_star(&mut prof, i);
        }
        drop(prof);
        self.static_pa_redraw();
        Ok(json!(0))
    }

    /// `OnClose`: a slew under way is left to run.
    pub(super) fn static_pa_close(&mut self) -> Reply {
        self.static_pa_tool()?.close();
        self.static_pa_redraw();
        self.static_pa = None;
        Ok(json!(0))
    }

    /// [`static_pa::days_since_j2000`] now.
    fn pa_days(&self) -> f64 {
        static_pa::days_since_j2000(self.wall().timestamp(), static_pa::j2000_local())
    }

    /// Its log lines out, then the overlay and the star map as PHD2 paints
    /// them after a change.
    fn static_pa_redraw(&mut self) {
        let coords = self.pointing().and_then(|p| p.coordinates());
        let days = self.pa_days();
        let Some(w) = self.static_pa.as_mut() else { return };
        let lines = w.tool.take_log();
        w.adjustment = w.tool.calc_adjustments(coords, days);
        w.stars = w.tool.star_map(coords, days);
        for l in lines {
            self.debug(&l);
        }
    }

    /// The tool's part of a SELECTED frame (`StaticPaTool::UpdateState`,
    /// then `NotifyStarLost` on a lost star). A failed rotation drops the
    /// star; false then, and the polar drift tool is not run.
    pub(super) fn static_pa_frame(&mut self, t: PaTools) -> bool {
        if self.static_pa.is_none() || t == PaTools::None {
            return true;
        }
        let Engine { static_pa, guider, scope, aux, manual_pointing, .. } = self;
        let mut gear = PaGear { pointing: pointing_mut(scope, aux, manual_pointing), guider, lock: false };
        let tool = &mut static_pa.as_mut().expect("open").tool;
        let ok = match t {
            PaTools::UpdateState => tool.update_state(&mut gear),
            _ => {
                tool.notify_star_lost(&mut gear);
                true
            }
        };
        if gear.lock
            && let Some(pos) = self.guider.current_position()
        {
            self.call(|c, cx, ev| {
                let mut gev = Vec::new();
                cx.guider.set_lock_pos_to_star_at_position(pos, &mut cx.gcx, &mut gev);
                c.forward(gev, cx, ev);
            });
        }
        self.static_pa_redraw();
        if !ok {
            self.call(|c, cx, ev| {
                let mut gev = Vec::new();
                cx.guider.pa_tool_failed(PaFailure::StaticPa, &mut cx.gcx, &mut gev);
                c.forward(gev, cx, ev);
            });
        }
        ok
    }

    /// The window, for `guider_pa`'s `static`.
    pub(super) fn static_pa_view(&self) -> Value {
        let Some(w) = self.static_pa.as_ref() else { return Value::Null };
        let t = &w.tool;
        let pt = |p: Point| json!([p.x, p.y]);
        let (n_step, req_step, tot_rot, req_rot) = t.rotation();
        json!({
            "auto": t.is_auto(),
            "can_slew": t.can_slew(),
            "aligning": t.is_aligning(),
            "calced": t.is_calced(),
            "num_pos": t.num_pos(),
            "hemisphere": t.hemisphere(),
            "ref_star": t.ref_star(),
            "hour_angle": t.hour_angle(),
            "flip": t.flip(),
            "cam_angle": t.cam_angle(),
            "pixel_scale": t.pixel_scale(),
            "cam_width": t.cam_width(),
            "status": t.status(),
            "positions": t.positions().map(|p| p.map(pt)),
            "centre": t.centre().map(|(c, r)| json!({"x": c.x, "y": c.y, "radius": r})),
            "frame": [t.frame_size().0, t.frame_size().1],
            "rotation": {"steps": n_step, "req_steps": req_step, "total_deg": tot_rot, "req_deg": req_rot},
            "adjustment": w.adjustment.map(|a| json!({
                "measured": pt(a.measured), "target": pt(a.target),
                "alt_corr": pt(a.alt_corr), "az_corr": pt(a.az_corr),
                "alt_px": a.alt_px, "az_px": a.az_px, "total_px": a.total_px,
                "alt_arcmin": a.alt_arcmin, "az_arcmin": a.az_arcmin, "total_arcmin": a.total_arcmin,
            })),
            "stars": w.stars.iter().map(|s| json!({
                "name": s.name, "ra": s.ra, "dec": s.dec, "mag": s.mag, "px": pt(s.px), "radius_px": s.radius_px,
            })).collect::<Vec<_>>(),
        })
    }
}
