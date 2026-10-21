//! Horizon mask — the part of the sky the setup can really see.
//!
//! Mirrors KStars' Artificial Horizon (`skycomponents/artificialhorizoncomponent.h`):
//! named regions of `(az, alt)` points. KStars is the store — read with
//! `artificial_horizon_get`, written with `artificial_horizon_import`
//! (its text format, below) and `artificial_horizon_toggle`
//! (`ekoslive/message.cpp:5910`) — and `HorizonCtx` in `main.rs` keeps a
//! localStorage copy so the sky and the planning tabs have the last known
//! mask before KStars attaches.
//!
//! Visibility copies `SchedulerJob::satisfiesAltitudeConstraint`: a point is
//! visible when it sits above the highest enabled non-ceiling line at its
//! azimuth. Ceilings are kept (and drawn) but never block — KStars' mixed
//! ceiling rule is not worth mirroring for a mask drawn by hand.

use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One named region: the line of `(az°, alt°)` points under (or, for a
/// ceiling, above) which the view is blocked.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HorizonRegion {
    pub name:    String,
    pub enabled: bool,
    pub ceiling: bool,
    pub points:  Vec<(f64, f64)>,
}

impl HorizonRegion {
    /// KStars' import text: a `Horizon <name>` / `Ceiling <name>` line then
    /// one `az alt` line per point (`dms::fromString` reads decimal degrees).
    pub fn to_import_text(&self) -> String {
        let mut s = format!("{} {}\n", if self.ceiling { "Ceiling" } else { "Horizon" }, self.name.trim());
        for (az, alt) in &self.points {
            s.push_str(&format!("{az:.2} {alt:.2}\n"));
        }
        s
    }

    /// The line's altitude at `az`, or `None` when no segment covers it.
    /// As KStars (`ArtificialHorizonEntity::altitudeConstraint`), a segment
    /// spans the short arc between its two azimuths, whichever way the
    /// points run, and the highest segment wins; the interpolation is linear
    /// where KStars follows a great circle.
    pub fn altitude_at(&self, az: f64) -> Option<f64> {
        let mut best: Option<f64> = None;
        for w in self.points.windows(2) {
            let (a1, h1) = w[0];
            let (a2, h2) = w[1];
            let total = delta_angle(a1, a2).abs();
            let d1 = delta_angle(a1, az).abs();
            let d2 = delta_angle(a2, az).abs();
            if d1 <= total && d2 <= total {
                let t = if total > 0.0 { d1 / total } else { 0.0 };
                let h = h1 + (h2 - h1) * t;
                best = Some(best.map_or(h, |b: f64| b.max(h)));
            }
        }
        best
    }

    /// The line resampled every `step` degrees of azimuth along each
    /// segment's short arc, the points themselves included.
    pub fn sampled(&self, step: f64) -> Vec<(f64, f64)> {
        sample_line(&self.points, step)
    }

    /// Enabled regions with fewer than two points never block anything.
    pub fn counts(&self) -> bool {
        self.enabled && !self.ceiling && self.points.len() >= 2
    }
}

/// Every region KStars knows, in its order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HorizonMask {
    pub regions: Vec<HorizonRegion>,
}

impl HorizonMask {
    /// The `artificial_horizon_get` reply: an array of
    /// `{name, enabled, ceiling, points: [{az, alt}]}`.
    pub fn from_reply(payload: &Value) -> Option<Self> {
        let arr = payload.as_array()?;
        let regions = arr
            .iter()
            .filter_map(|r| {
                let name = r["name"].as_str()?.to_string();
                let points = r["points"]
                    .as_array()
                    .map(|pts| {
                        pts.iter()
                            .filter_map(|p| Some((p["az"].as_f64()?, p["alt"].as_f64()?)))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(HorizonRegion {
                    name,
                    enabled: r["enabled"].as_bool().unwrap_or(true),
                    ceiling: r["ceiling"].as_bool().unwrap_or(false),
                    points,
                })
            })
            .collect();
        Some(Self { regions })
    }

    /// At least one region blocks something.
    pub fn is_active(&self) -> bool {
        self.regions.iter().any(HorizonRegion::counts)
    }

    /// The highest blocking altitude at `az`; −90 when nothing covers it.
    pub fn altitude_at(&self, az: f64) -> f64 {
        self.regions
            .iter()
            .filter(|r| r.counts())
            .filter_map(|r| r.altitude_at(az))
            .fold(-90.0, f64::max)
    }

    pub fn is_visible(&self, az: f64, alt: f64) -> bool {
        alt >= self.altitude_at(az)
    }

    /// The Goto warning: a mask exists and the point sits behind it.
    pub fn blocks(&self, az: f64, alt: f64) -> bool {
        self.is_active() && !self.is_visible(az, alt)
    }

    /// Replace (or add) a region by name — the optimistic local update after
    /// an `artificial_horizon_import`.
    pub fn upsert(&mut self, region: HorizonRegion) {
        match self.regions.iter_mut().find(|r| r.name == region.name) {
            Some(r) => *r = region,
            None => self.regions.push(region),
        }
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        if let Some(r) = self.regions.iter_mut().find(|r| r.name == name) {
            r.enabled = enabled;
        }
    }

    /// `Junos`, `Junos 2`, … — a name no region holds yet.
    pub fn fresh_name(&self) -> String {
        let taken = |n: &str| self.regions.iter().any(|r| r.name == n);
        if !taken("Junos") {
            return "Junos".to_string();
        }
        (2..).map(|i| format!("Junos {i}")).find(|n| !taken(n)).unwrap()
    }
}

/// Signed short-way angle from `a` to `b`, degrees, in (−180, 180].
pub fn delta_angle(a: f64, b: f64) -> f64 {
    let d = (b - a).rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}

/// `points` resampled every `step` degrees of azimuth along each segment's
/// short arc, the points themselves included; empty below two points.
pub fn sample_line(points: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    if points.len() < 2 {
        return out;
    }
    out.push(points[0]);
    for w in points.windows(2) {
        let (a1, h1) = w[0];
        let (a2, h2) = w[1];
        let d = delta_angle(a1, a2);
        let n = (d.abs() / step).ceil().max(1.0) as usize;
        for i in 1..=n {
            let t = i as f64 / n as f64;
            out.push(((a1 + d * t).rem_euclid(360.0), h1 + (h2 - h1) * t));
        }
    }
    out
}

/// The region being drawn on the sky: its points, the handle being dragged
/// and an undo history. Lives in a signal of the Sky tab; the render layer
/// draws it in place of the stored region of the same name.
#[derive(Clone, Debug, PartialEq)]
pub struct HorizonDraft {
    pub name:    String,
    pub points:  Vec<(f64, f64)>,
    pub drag:    Option<usize>,
    pub history: Vec<Vec<(f64, f64)>>,
}

impl HorizonDraft {
    pub fn new(name: String) -> Self {
        Self { name, points: Vec::new(), drag: None, history: Vec::new() }
    }

    pub fn from_region(r: &HorizonRegion) -> Self {
        Self { name: r.name.clone(), points: r.points.clone(), drag: None, history: Vec::new() }
    }

    fn snapshot(&mut self) {
        if self.history.len() >= 50 {
            self.history.remove(0);
        }
        self.history.push(self.points.clone());
    }

    /// Where a point at `az` goes: after the last point not past it, so a
    /// line drawn left to right stays sorted and one drawn the other way
    /// isn't scrambled.
    fn slot(&self, az: f64) -> usize {
        self.points.iter().rposition(|(a, _)| *a <= az).map_or(0, |i| i + 1)
    }

    /// Add a point and return its index.
    pub fn add(&mut self, az: f64, alt: f64) -> usize {
        self.snapshot();
        let i = self.slot(az);
        self.points.insert(i, (az.rem_euclid(360.0), alt.clamp(0.0, 89.0)));
        i
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.points.len() {
            self.snapshot();
            self.points.remove(i);
            self.drag = None;
        }
    }

    pub fn begin_move(&mut self, i: usize) {
        if i < self.points.len() {
            self.snapshot();
            self.drag = Some(i);
        }
    }

    /// Move the dragged point, keeping it in azimuth order.
    pub fn move_to(&mut self, az: f64, alt: f64) {
        let Some(i) = self.drag.filter(|i| *i < self.points.len()) else { return };
        self.points.remove(i);
        let az = az.rem_euclid(360.0);
        let j = self.slot(az);
        self.points.insert(j, (az, alt.clamp(0.0, 89.0)));
        self.drag = Some(j);
    }

    /// A press that moved nothing leaves no undo step behind.
    pub fn end_move(&mut self) {
        if self.drag.take().is_some() && self.history.last() == Some(&self.points) {
            self.history.pop();
        }
    }

    pub fn undo(&mut self) {
        if let Some(p) = self.history.pop() {
            self.points = p;
            self.drag = None;
        }
    }

    pub fn clear(&mut self) {
        if !self.points.is_empty() {
            self.snapshot();
            self.points.clear();
            self.drag = None;
        }
    }

    /// What Save sends: the draft as an enabled horizon region.
    pub fn region(&self) -> HorizonRegion {
        HorizonRegion { name: self.name.trim().to_string(), enabled: true, ceiling: false, points: self.points.clone() }
    }

    pub fn can_save(&self) -> bool {
        self.points.len() >= 2 && !self.name.trim().is_empty()
    }
}

/// The wire commands, built here so the sky, the layers panel and the
/// bootstrap all spell them the same way.
pub fn cmd_get() -> String {
    r#"{"type":"artificial_horizon_get","payload":{}}"#.to_string()
}

pub fn cmd_import(region: &HorizonRegion) -> String {
    serde_json::json!({
        "type": "artificial_horizon_import",
        "payload": { "data": region.to_import_text() },
    })
    .to_string()
}

pub fn cmd_toggle(name: &str, enabled: bool) -> String {
    serde_json::json!({
        "type": "artificial_horizon_toggle",
        "payload": { "region": name, "enabled": enabled },
    })
    .to_string()
}

const STORAGE_KEY: &str = "junos_horizon";

/// The last mask KStars reported, from localStorage.
pub fn load_cached() -> HorizonMask {
    crate::components::sky::local_storage()
        .and_then(|s| s.get_item(STORAGE_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

pub fn save_cached(mask: &HorizonMask) {
    if let (Some(s), Ok(v)) = (crate::components::sky::local_storage(), serde_json::to_string(mask)) {
        let _ = s.set_item(STORAGE_KEY, &v);
    }
}

/// Whether the mask hides a JNow position right now, from `site`.
pub fn blocks_now(mask: &HorizonMask, ra_jnow_deg: f64, dec_jnow_deg: f64, site: &crate::compat::SiteSnapshot) -> bool {
    let lst = crate::astro::lst_deg(crate::astro::gmst_deg(crate::astro::now_jd()), site.longitude);
    let (alt, az) = crate::astro::eq_to_altaz(ra_jnow_deg, dec_jnow_deg, lst, site.latitude);
    mask.blocks(az, alt)
}

/// The two-tap Goto confirm: the first tap on a masked target arms `sig`
/// (the button then reads "Goto anyway?"), a second one within 5 s sends.
pub fn arm_confirm(sig: RwSignal<bool>) {
    sig.set(true);
    gloo_timers::callback::Timeout::new(5_000, move || {
        if let Some(true) = sig.try_get_untracked() {
            sig.set(false);
        }
    })
    .forget();
}
