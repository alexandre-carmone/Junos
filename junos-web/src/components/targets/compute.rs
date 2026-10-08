//! Tonight's candidates: each catalog object's altitude across the observing
//! window, sampled every 10 min next to the Moon, and the Best score that
//! ranks them.
//!
//! An object is precessed once (mid-window: the drift over a night is
//! arcseconds) and its `sin φ·sin δ` / `cos φ·cos δ` cached, so a sample costs
//! one cosine — the whole catalog over a long night is about a million of
//! them, a few milliseconds.

use crate::astro;
use crate::compat::SiteSnapshot;
use crate::components::scheduler::altitude::samples;
use crate::components::sky::clock;
use crate::coords::J2000;
use crate::dso_catalog::Dso;
use crate::ephemeris;

/// Sampling step, as the Scheduler's altitude chart.
pub const STEP_MS: f64 = 600_000.0;

/// Sidereal time and the Moon at each sample of the window.
#[derive(PartialEq)]
pub struct NightSky {
    pub start: f64,
    pub end: f64,
    /// Sample instants, Unix ms, `start` and `end` included.
    pub times: Vec<f64>,
    /// Local sidereal time, radians.
    lst: Vec<f64>,
    /// The Moon, JNow: (RA, sin Dec, cos Dec), radians.
    moon: Vec<(f64, f64, f64)>,
    /// The Moon's altitude, degrees.
    pub moon_alt: Vec<f64>,
    /// Illuminated fraction at mid-window, 0–1.
    pub illum: f64,
    jd_mid: f64,
    lat: f64,
}

impl NightSky {
    pub fn new(start: f64, end: f64, site: &SiteSnapshot) -> Self {
        let times: Vec<f64> = samples(start, end, STEP_MS).collect();
        let mut lst = Vec::with_capacity(times.len());
        let mut moon = Vec::with_capacity(times.len());
        let mut moon_alt = Vec::with_capacity(times.len());
        for &t in &times {
            let jd = clock::ms_to_jd(t);
            let l = astro::lst_deg(astro::gmst_deg(jd), site.longitude);
            let m = ephemeris::moon(jd).jnow;
            lst.push(l.to_radians());
            let (sd, cd) = m.dec_deg.to_radians().sin_cos();
            moon.push((m.ra_deg.to_radians(), sd, cd));
            moon_alt.push(astro::eq_to_altaz(m.ra_deg, m.dec_deg, l, site.latitude).0);
        }
        let jd_mid = clock::ms_to_jd(0.5 * (start + end));
        Self {
            start,
            end,
            times,
            lst,
            moon,
            moon_alt,
            illum: ephemeris::moon(jd_mid).phase.unwrap_or(0.0),
            jd_mid,
            lat: site.latitude,
        }
    }

    /// When the Moon is above the horizon in the window: the first and last
    /// such sample, `None` if it never is.
    pub fn moon_up(&self) -> Option<(f64, f64)> {
        let up = |i: &usize| self.moon_alt[*i] > 0.0;
        let first = (0..self.times.len()).find(up)?;
        let last = (0..self.times.len()).rev().find(up)?;
        Some((self.times[first], self.times[last]))
    }
}

/// One object that clears the minimum altitude during the window.
#[derive(Clone, Copy, PartialEq)]
pub struct Candidate {
    /// Index into the catalog's `dsos`.
    pub idx: usize,
    /// Altitude at the start of the window, degrees.
    pub alt_start: f64,
    /// Highest altitude in the window and when.
    pub peak_alt: f64,
    pub peak_ms: f64,
    /// First and last sample above the minimum altitude.
    pub up_from: f64,
    pub up_to: f64,
    /// Time above the minimum altitude, minutes.
    pub up_minutes: f64,
    /// Distance from the Moon at the peak, degrees, and whether the Moon is
    /// above the horizon then.
    pub moon_sep: f64,
    pub moon_up: bool,
}

/// Every object above `min_alt` (degrees) at some sample of the window.
pub fn evaluate(dsos: &[Dso], sky: &NightSky, min_alt: f64) -> Vec<Candidate> {
    let (sl, cl) = sky.lat.to_radians().sin_cos();
    let sin_min = min_alt.to_radians().sin();
    let span = sky.end - sky.start;
    let mut out = Vec::new();
    for (idx, d) in dsos.iter().enumerate() {
        let p = J2000::new(d.ra_deg as f64, d.dec_deg as f64).to_jnow(sky.jd_mid);
        let ra = p.ra_deg.to_radians();
        let (sd, cd) = p.dec_deg.to_radians().sin_cos();
        let (a, b) = (sl * sd, cl * cd);
        // sin(alt) = a + b·cos(H); at transit a + b. Never high enough: skip.
        if a + b < sin_min {
            continue;
        }
        let (mut peak, mut peak_i) = (f64::MIN, 0);
        let (mut first, mut last, mut count) = (None, 0, 0usize);
        for (i, lst) in sky.lst.iter().enumerate() {
            let s = a + b * (lst - ra).cos();
            if s > peak {
                peak = s;
                peak_i = i;
            }
            if s >= sin_min {
                first.get_or_insert(i);
                last = i;
                count += 1;
            }
        }
        let Some(first) = first else { continue };
        let (mra, msd, mcd) = sky.moon[peak_i];
        let cos_sep = sd * msd + cd * mcd * (ra - mra).cos();
        let alt = |s: f64| s.clamp(-1.0, 1.0).asin().to_degrees();
        out.push(Candidate {
            idx,
            alt_start: alt(a + b * (sky.lst[0] - ra).cos()),
            peak_alt: alt(peak),
            peak_ms: sky.times[peak_i],
            up_from: sky.times[first],
            up_to: sky.times[last],
            up_minutes: (count as f64 * STEP_MS).min(span) / 60_000.0,
            moon_sep: cos_sep.clamp(-1.0, 1.0).acos().to_degrees(),
            moon_up: sky.moon_alt[peak_i] > 0.0,
        });
    }
    out
}

/// Exponents of the Best score's terms: 0 drops a term, 3 lets it dominate.
#[derive(Clone, Copy, PartialEq)]
pub struct Weights {
    pub alt: f64,
    pub time: f64,
    pub moon: f64,
    pub bright: f64,
    pub size: f64,
}

/// Best score, 0–1: a weighted geometric mean of
/// - altitude: the peak, from the minimum altitude (0) to the zenith (1), in
///   sin(alt) — roughly the airmass gained;
/// - time: hours above the minimum altitude, full marks from 4 h;
/// - Moon: its glare when it is up at the peak, growing with illumination and
///   closeness (from 90° in to 20°), and with how much of the object is
///   broadband — a narrowband Hα target barely cares, OIII somewhat;
/// - brightness: magnitude 15 → 5 (the size stand-in when there is none);
/// - size: log scale, 0.3′ → 100′.
///
/// Each term is floored at 0.05 so a zero doesn't flatten the ranking.
pub fn score(c: &Candidate, d: &Dso, illum: f64, min_alt: f64, w: &Weights) -> f64 {
    let term = |x: f64| x.clamp(0.05, 1.0);
    let sin_min = min_alt.to_radians().sin();
    let alt = term((c.peak_alt.to_radians().sin() - sin_min) / (1.0 - sin_min).max(1e-6));
    let time = term(c.up_minutes / 240.0);
    let moon = if c.moon_up {
        let closeness = ((90.0 - c.moon_sep) / 70.0).clamp(0.0, 1.0);
        let l = d.lines;
        let sensitivity = (l.broad() as f64 / 3.0).max(0.5 * l.oiii() as f64 / 3.0).max(0.25);
        term(1.0 - 0.9 * illum * closeness * sensitivity)
    } else {
        1.0
    };
    let bright = term((15.0 - d.vis_mag as f64) / 10.0);
    let size = term(((d.size_arcmin.max(0.1) as f64).log10() + 0.5) / 2.5);
    alt.powf(w.alt) * time.powf(w.time) * moon.powf(w.moon) * bright.powf(w.bright) * size.powf(w.size)
}
