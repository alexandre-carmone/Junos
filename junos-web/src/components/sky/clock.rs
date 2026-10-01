//! Simulated planetarium time — the single time source of the Sky tab.
//!
//! `sim = base_sim + (real_now − base_real) × rate`: `rate` is 1 for normal
//! flow and the time-lapse speed (sim seconds per real second) in playback.
//! Local-time helpers use `js_sys::Date`, so the browser handles DST.

use crate::{astro, ephemeris};

/// Sun altitude that bounds astronomical night.
const TWILIGHT_ALT_DEG: f64 = -18.0;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SkyClock {
    base_sim_ms: f64,
    base_real_ms: f64,
    pub rate: f64,
}

impl SkyClock {
    pub fn live() -> Self {
        let t = js_sys::Date::now();
        Self { base_sim_ms: t, base_real_ms: t, rate: 1.0 }
    }

    /// Simulated instant, Unix ms.
    pub fn now_ms(&self) -> f64 {
        self.base_sim_ms + (js_sys::Date::now() - self.base_real_ms) * self.rate
    }

    pub fn jd(&self) -> f64 {
        ms_to_jd(self.now_ms())
    }

    /// Simulated minus real time, seconds.
    pub fn offset_s(&self) -> f64 {
        (self.now_ms() - js_sys::Date::now()) / 1000.0
    }

    pub fn is_live(&self) -> bool {
        self.rate == 1.0 && self.offset_s().abs() < 1.0
    }

    /// Jump to `sim_ms`, keeping the rate.
    pub fn at(self, sim_ms: f64) -> Self {
        Self { base_sim_ms: sim_ms, base_real_ms: js_sys::Date::now(), rate: self.rate }
    }

    /// Change the rate from the current simulated instant.
    pub fn with_rate(self, rate: f64) -> Self {
        Self { rate, ..self.at(self.now_ms()) }
    }
}

pub fn ms_to_jd(ms: f64) -> f64 {
    ms / 86_400_000.0 + 2_440_587.5
}

fn date(ms: f64) -> js_sys::Date {
    js_sys::Date::new(&ms.into())
}

/// Local noon that starts the observing night (noon → noon) containing `ms`.
pub fn night_start(ms: f64) -> f64 {
    let d = date(ms);
    if d.get_hours() < 12 {
        d.set_date(d.get_date() - 1); // day 0 rolls back into the previous month
    }
    d.set_hours(12);
    d.set_minutes(0);
    d.set_seconds(0);
    d.set_milliseconds(0);
    d.get_time()
}

/// Local `h:m` within the observing night containing `ms`: at 22:41,
/// 02:00 is tomorrow morning and 23:00 is tonight.
pub fn night_hour(ms: f64, h: u32, m: u32) -> f64 {
    let d = date(night_start(ms));
    if h < 12 {
        d.set_date(d.get_date() + 1);
    }
    d.set_hours(h);
    d.set_minutes(m);
    d.get_time()
}

/// `ms` moved to local calendar date `y-mo-d`, keeping the time of day.
pub fn on_date(ms: f64, y: u32, mo: u32, d: u32) -> f64 {
    let t = date(ms);
    t.set_full_year_with_month_date(y, mo as i32 - 1, d as i32);
    t.get_time()
}

fn sun_alt(ms: f64, lat: f64, lon: f64) -> f64 {
    let jd = ms_to_jd(ms);
    let sun = ephemeris::sun(jd).jnow;
    let lst = astro::lst_deg(astro::gmst_deg(jd), lon);
    astro::eq_to_altaz(sun.ra_deg, sun.dec_deg, lst, lat).0
}

/// Astronomical dusk and dawn (Unix ms) of the night starting at local noon
/// `noon_ms`. `None` when the sun never crosses −18° that night.
pub fn night_twilights(noon_ms: f64, lat: f64, lon: f64) -> (Option<f64>, Option<f64>) {
    const STEP_MS: f64 = 600_000.0; // 10 min scan, then bisection
    let bright = |ms: f64| sun_alt(ms, lat, lon) > TWILIGHT_ALT_DEG;
    let refine = |mut lo: f64, mut hi: f64| {
        let lo_bright = bright(lo);
        for _ in 0..12 {
            let mid = 0.5 * (lo + hi);
            if bright(mid) == lo_bright { lo = mid } else { hi = mid }
        }
        hi
    };
    let (mut dusk, mut dawn) = (None, None);
    let mut a = noon_ms;
    let mut a_bright = bright(a);
    for _ in 0..144 {
        let b = a + STEP_MS;
        let b_bright = bright(b);
        if dusk.is_none() && a_bright && !b_bright {
            dusk = Some(refine(a, b));
        } else if dusk.is_some() && !a_bright && b_bright {
            dawn = Some(refine(a, b));
            break;
        }
        a = b;
        a_bright = b_bright;
    }
    (dusk, dawn)
}
