//! A job's observing constraints: where the target, the Sun and the Moon
//! stand at a given time (`snapshot`), whether a job's rules hold there
//! (`violated`), and the minute-by-minute window search KStars' greedy
//! scheduler does (`next_start` / `next_end`).

use crate::indigo::{astro, ephem};

/// The observer: degrees, east longitude positive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Site {
    pub lat: f64,
    pub lon_east: f64,
}

/// Everything the rules look at, at one instant. Degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub alt: f64,
    pub az: f64,
    /// Lower one minute later.
    pub setting: bool,
    pub moon_alt: f64,
    /// Target ↔ Moon separation.
    pub moon_sep: f64,
    pub sun_alt: f64,
}

/// A job's rules; a `None` rule isn't checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    pub min_alt: Option<f64>,
    pub min_moon_sep: Option<f64>,
    pub max_moon_alt: Option<f64>,
    pub twilight: bool,
}

/// Sun altitude the astronomical night ends at.
pub const TWILIGHT_SUN_ALT: f64 = -18.0;

const MINUTE: f64 = 1.0 / 1440.0;

/// The target's (alt, az, ra_now, dec_now) from J2000 coordinates.
fn target(ra0_h: f64, dec0_deg: f64, jd: f64, site: &Site) -> (f64, f64, f64, f64) {
    let (ra, dec) = astro::j2000_to_jnow(ra0_h * 15.0, dec0_deg, jd);
    let (alt, az) = astro::eq_to_altaz(ra, dec, astro::lst_deg(jd, site.lon_east), site.lat);
    (alt, az, ra, dec)
}

pub fn snapshot(ra0_h: f64, dec0_deg: f64, jd: f64, site: &Site) -> Snapshot {
    let (alt, az, ra, dec) = target(ra0_h, dec0_deg, jd, site);
    let (alt_next, ..) = target(ra0_h, dec0_deg, jd + MINUTE, site);
    let lst = astro::lst_deg(jd, site.lon_east);
    let (mra, mdec, _) = ephem::moon(jd);
    let (moon_alt, _) = astro::eq_to_altaz(mra, mdec, lst, site.lat);
    let (sra, sdec) = ephem::sun(jd);
    let (sun_alt, _) = astro::eq_to_altaz(sra, sdec, lst, site.lat);
    Snapshot {
        alt,
        az,
        setting: alt_next < alt,
        moon_alt,
        moon_sep: astro::angular_sep_deg(ra, dec, mra, mdec),
        sun_alt,
    }
}

/// The first rule `s` breaks, as the scheduler's log line, `None` when all hold.
pub fn violated(rules: &Rules, s: &Snapshot) -> Option<String> {
    if let Some(min) = rules.min_alt {
        if s.alt < min {
            return Some(format!("altitude {:.1}° below {:.1}°", s.alt, min));
        }
    }
    if rules.twilight && s.sun_alt > TWILIGHT_SUN_ALT {
        return Some(format!("astronomical twilight (sun at {:.1}°)", s.sun_alt));
    }
    if let Some(min) = rules.min_moon_sep {
        if s.moon_sep < min {
            return Some(format!("moon separation {:.1}° below {:.1}°", s.moon_sep, min));
        }
    }
    if let Some(max) = rules.max_moon_alt {
        if s.moon_alt > max {
            return Some(format!("moon altitude {:.1}° above {:.1}°", s.moon_alt, max));
        }
    }
    None
}

/// The first minute at or after `max(from, not_before)`, within
/// `horizon_days`, where every rule holds — `from_jd` itself when it already
/// does.
pub fn next_start(
    rules: &Rules,
    ra0_h: f64,
    dec0_deg: f64,
    from_jd: f64,
    not_before_jd: Option<f64>,
    site: &Site,
    horizon_days: f64,
) -> Option<f64> {
    let start = not_before_jd.map_or(from_jd, |t| t.max(from_jd));
    let end = from_jd + horizon_days;
    let mut jd = start;
    while jd <= end {
        if violated(rules, &snapshot(ra0_h, dec0_deg, jd, site)).is_none() {
            return Some(jd);
        }
        jd += MINUTE;
    }
    None
}

/// The first minute after `start_jd`, within `horizon_days`, where a rule
/// breaks — `None` when none does within the horizon.
pub fn next_end(rules: &Rules, ra0_h: f64, dec0_deg: f64, start_jd: f64, site: &Site, horizon_days: f64) -> Option<f64> {
    let end = start_jd + horizon_days;
    let mut jd = start_jd + MINUTE;
    while jd <= end {
        if violated(rules, &snapshot(ra0_h, dec0_deg, jd, site)).is_some() {
            return Some(jd);
        }
        jd += MINUTE;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: Site = Site { lat: 48.5, lon_east: -2.3 };
    const VEGA: (f64, f64) = (18.6157, 38.78);
    const JUL_15_10H: f64 = 2_460_871.916_67; // 2025-07-15 10:00 UT, Vega down
    const JUL_15_12H: f64 = 2_460_872.0; // 2025-07-15 12:00 UT, noon
    const JUL_15_22H: f64 = 2_460_872.416_67; // 2025-07-15 22:00 UT, Vega near transit
    const OCT_15_0H: f64 = 2_460_963.5; // 2025-10-15 00:00 UT, deep night

    fn vega(jd: f64) -> Snapshot {
        snapshot(VEGA.0, VEGA.1, jd, &SITE)
    }

    #[test]
    fn vega_up_and_down() {
        let up = vega(JUL_15_22H);
        assert!(up.alt > 60.0, "{}", up.alt);
        let down = vega(JUL_15_10H);
        assert!(down.alt < 10.0, "{}", down.alt);
        let rules = Rules { min_alt: Some(30.0), ..Rules::default() };
        assert_eq!(violated(&rules, &up), None);
        let why = violated(&rules, &down).unwrap();
        assert!(why.starts_with("altitude ") && why.ends_with(" below 30.0°"), "{why}");
    }

    #[test]
    fn setting_flag_follows_the_meridian() {
        // Vega transits around 23:10 UT at this longitude: rising before, setting after.
        assert!(!vega(JUL_15_22H).setting);
        assert!(vega(JUL_15_22H + 2.0 / 24.0).setting);
    }

    #[test]
    fn twilight() {
        let rules = Rules { twilight: true, ..Rules::default() };
        let noon = vega(JUL_15_12H);
        assert!(noon.sun_alt > 50.0, "{}", noon.sun_alt);
        let why = violated(&rules, &noon).unwrap();
        assert!(why.starts_with("astronomical twilight (sun at "), "{why}");
        let night = vega(OCT_15_0H);
        assert!(night.sun_alt < -18.0, "{}", night.sun_alt);
        assert_eq!(violated(&rules, &night), None);
    }

    #[test]
    fn moon_rules() {
        let s = vega(OCT_15_0H);
        assert!((-90.0..=90.0).contains(&s.moon_alt));
        assert!((0.0..=180.0).contains(&s.moon_sep));
        let sep = Rules { min_moon_sep: Some(s.moon_sep + 1.0), ..Rules::default() };
        assert!(violated(&sep, &s).unwrap().starts_with("moon separation "));
        let alt = Rules { max_moon_alt: Some(s.moon_alt - 1.0), ..Rules::default() };
        assert!(violated(&alt, &s).unwrap().starts_with("moon altitude "));
        let fine = Rules {
            min_moon_sep: Some(s.moon_sep - 1.0),
            max_moon_alt: Some(s.moon_alt + 1.0),
            ..Rules::default()
        };
        assert_eq!(violated(&fine, &s), None);
    }

    #[test]
    fn window_search() {
        let rules = Rules { min_alt: Some(30.0), twilight: true, ..Rules::default() };
        // From a July morning: the (short) astronomical night around 00:30 UT.
        let start = next_start(&rules, VEGA.0, VEGA.1, JUL_15_10H, None, &SITE, 1.0).unwrap();
        assert!(start > JUL_15_10H && start - JUL_15_10H <= 1.0, "{start}");
        assert_eq!(violated(&rules, &vega(start)), None);
        assert_eq!(violated(&rules, &vega(start - MINUTE)).is_some(), true);
        let end = next_end(&rules, VEGA.0, VEGA.1, start, &SITE, 1.0).unwrap();
        assert!(end > start && end - start <= 1.0, "{end}");
        assert!(violated(&rules, &vega(end)).is_some());
        // Already fine: the start is the start.
        assert_eq!(next_start(&rules, VEGA.0, VEGA.1, start, None, &SITE, 1.0), Some(start));
        // `not_before` pushes the search past the window's first minute.
        let later = next_start(&rules, VEGA.0, VEGA.1, start, Some(start + 10.0 * MINUTE), &SITE, 1.0).unwrap();
        assert!((later - (start + 10.0 * MINUTE)).abs() < 1e-9);
        // Nothing within an hour of noon.
        assert_eq!(next_start(&rules, VEGA.0, VEGA.1, JUL_15_12H, None, &SITE, 1.0 / 24.0), None);
        // No rules: always fine, never ends.
        assert_eq!(next_start(&Rules::default(), VEGA.0, VEGA.1, JUL_15_12H, None, &SITE, 1.0), Some(JUL_15_12H));
        assert_eq!(next_end(&Rules::default(), VEGA.0, VEGA.1, JUL_15_12H, &SITE, 1.0), None);
    }
}
