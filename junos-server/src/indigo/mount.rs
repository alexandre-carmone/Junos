//! `new_mount_state` from the mount's INDIGO properties.
//!
//! KStars emits it from several places (`manager.cpp::updateMountCoords`,
//! `pierSideChanged`, status changes…) with partial payloads, the coordinate
//! one throttled to 1 s (`message.cpp:2552`); the browser merges them
//! (`ws/store.rs`). This emitter does the same: an immediate status-only push
//! whenever the status string changes, and the full coordinate payload at
//! most once a second, flushed by the session tick.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::astro;
use super::commands::reply_msg;
use super::protocol::{Change, Property, PropertyStore, State};

const COORD_THROTTLE: Duration = Duration::from_secs(1);

/// Properties whose change touches `new_mount_state`.
const MOUNT_PROPS: &[&str] = &[
    "MOUNT_EQUATORIAL_COORDINATES",
    "MOUNT_HORIZONTAL_COORDINATES",
    "MOUNT_PARK",
    "MOUNT_TRACKING",
    "MOUNT_SIDE_OF_PIER",
    "MOUNT_SLEW_RATE",
    "MOUNT_LST_TIME",
    "GEOGRAPHIC_COORDINATES",
    "CONNECTION",
];

#[derive(Default)]
pub struct MountEmitter {
    last_full: Option<Instant>,
    pending: bool,
    last_status: String,
}

impl MountEmitter {
    /// Messages for a property change on `device` (the mount).
    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, device: &str) -> Vec<String> {
        let touched = match change {
            Change::Defined { device: d, name } | Change::Updated { device: d, name } => {
                d == device && MOUNT_PROPS.contains(&name.as_str())
            }
            _ => false,
        };
        if !touched {
            return Vec::new();
        }
        let mut out = Vec::new();
        let status = status(store, device);
        if status != self.last_status {
            self.last_status = status.clone();
            out.push(reply_msg("new_mount_state", json!({ "device": device, "status": status })));
        }
        let due = self
            .last_full
            .is_none_or(|t| t.elapsed() >= COORD_THROTTLE);
        if due {
            out.extend(self.flush(store, device));
        } else {
            self.pending = true;
        }
        out
    }

    /// Called once a second by the session: sends the coordinate payload a
    /// throttled change left pending.
    pub fn tick(&mut self, store: &PropertyStore, device: &str) -> Vec<String> {
        if self.pending && self.last_full.is_none_or(|t| t.elapsed() >= COORD_THROTTLE) {
            self.flush(store, device)
        } else {
            Vec::new()
        }
    }

    fn flush(&mut self, store: &PropertyStore, device: &str) -> Vec<String> {
        self.pending = false;
        self.last_full = Some(Instant::now());
        full_payload(store, device)
            .map(|p| vec![reply_msg("new_mount_state", p)])
            .unwrap_or_default()
    }
}

/// KStars' canonical status string (`indimount.cpp:30`), which the browser
/// matches by substring (`store.rs`) and by name (`compat.rs`, `mount.rs`).
pub fn status(store: &PropertyStore, device: &str) -> String {
    let park = store.get(device, "MOUNT_PARK");
    if park.is_some_and(|p| p.state == State::Busy) {
        return "Parking".into();
    }
    if park.is_some_and(|p| p.on("PARKED")) {
        return "Parked".into();
    }
    let coords = store.get(device, "MOUNT_EQUATORIAL_COORDINATES");
    if coords.is_some_and(|p| p.state == State::Busy) {
        return "Slewing".into();
    }
    let moving = ["MOUNT_MOTION_RA", "MOUNT_MOTION_DEC"]
        .iter()
        .filter_map(|n| store.get(device, n))
        .any(|p| p.state == State::Busy || p.items.iter().any(|i| i.on()));
    if moving {
        return "Moving".into();
    }
    if store.get(device, "MOUNT_TRACKING").is_some_and(|p| p.on("ON")) {
        return "Tracking".into();
    }
    "Idle".into()
}

/// Ekos' slew-rate index (0 guide … 7 max) for the mount's current rate.
pub fn slew_rate_index(p: &Property) -> Option<i64> {
    Some(match p.selected()? {
        "GUIDE" => 0,
        "CENTERING" => 2,
        "FIND" => 4,
        "MAX" => 7,
        _ => return None,
    })
}

/// The INDIGO rate for an Ekos index (the Mount tab's eight pills).
pub fn slew_rate_item(index: i64) -> &'static str {
    match index {
        i64::MIN..=0 => "GUIDE",
        1..=2 => "CENTERING",
        3..=5 => "FIND",
        _ => "MAX",
    }
}

/// `kstars/indi/indimount.h:39`: −1 unknown, 0 west, 1 east.
fn pier_side(store: &PropertyStore, device: &str) -> i64 {
    match store.get(device, "MOUNT_SIDE_OF_PIER").and_then(Property::selected) {
        Some("EAST") => 1,
        Some("WEST") => 0,
        _ => -1,
    }
}

/// Observer latitude and east longitude (degrees) from the first device that
/// has them, the mount first.
pub fn site_latlon(store: &PropertyStore, mount: Option<&str>) -> Option<(f64, f64)> {
    let geo = mount
        .and_then(|m| store.get(m, "GEOGRAPHIC_COORDINATES"))
        .or_else(|| store.device_names().find_map(|d| store.get(d, "GEOGRAPHIC_COORDINATES")))?;
    let lat = geo.number("LATITUDE")?;
    let mut lon = geo.number("LONGITUDE")?;
    if lon > 180.0 {
        lon -= 360.0;
    }
    Some((lat, lon))
}

/// The coordinate-bearing payload: RA/Dec (JNow, degrees), their J2000
/// counterparts, Az/Alt, HA (degrees), pier side, slew rate and status.
pub fn full_payload(store: &PropertyStore, device: &str) -> Option<Value> {
    let eq = store.get(device, "MOUNT_EQUATORIAL_COORDINATES")?;
    let ra_h = eq.number("RA")?;
    let dec = eq.number("DEC")?;
    let ra = ra_h * 15.0;
    let jd = astro::now_jd();
    let (ra0, de0) = astro::jnow_to_j2000(ra, dec, jd);
    let site = site_latlon(store, Some(device));

    // Hour angle, from the mount's own sidereal clock when it has one.
    let lst_h = store
        .get(device, "MOUNT_LST_TIME")
        .and_then(|p| p.number("TIME"))
        .or_else(|| site.map(|(_, lon)| astro::lst_deg(jd, lon) / 15.0));
    let ha = lst_h.map(|lst| {
        let mut ha = lst - ra_h;
        if ha > 12.0 {
            ha -= 24.0;
        } else if ha < -12.0 {
            ha += 24.0;
        }
        ha * 15.0
    });

    let horizontal = store.get(device, "MOUNT_HORIZONTAL_COORDINATES");
    let (az, alt) = match horizontal.and_then(|p| Some((p.number("AZ")?, p.number("ALT")?))) {
        Some(v) => (Some(v.0), Some(v.1)),
        None => match (site, lst_h) {
            (Some((lat, _)), Some(lst)) => {
                let (alt, az) = astro::eq_to_altaz(ra, dec, lst * 15.0, lat);
                (Some(az), Some(alt))
            }
            _ => (None, None),
        },
    };

    let mut payload = json!({
        "device": device,
        "status": status(store, device),
        "ra": ra,
        "de": dec,
        "ra0": ra0,
        "de0": de0,
        "pierSide": pier_side(store, device),
        "target": "",
    });
    if let Some(ha) = ha {
        payload["ha"] = json!(ha);
    }
    if let (Some(az), Some(alt)) = (az, alt) {
        payload["az"] = json!(az);
        payload["at"] = json!(alt);
    }
    if let Some(rate) = store.get(device, "MOUNT_SLEW_RATE").and_then(slew_rate_index) {
        payload["slewRate"] = json!(rate);
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defNumberVector":{"device":"M","name":"MOUNT_EQUATORIAL_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"RA","value":6.0},{"name":"DEC","value":20.0}]}}"#,
            r#"{"defSwitchVector":{"device":"M","name":"MOUNT_PARK","perm":"rw","state":"Ok","items":[{"name":"PARKED","value":false},{"name":"UNPARKED","value":true}]}}"#,
            r#"{"defSwitchVector":{"device":"M","name":"MOUNT_TRACKING","perm":"rw","state":"Ok","items":[{"name":"ON","value":true},{"name":"OFF","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"M","name":"MOUNT_SLEW_RATE","perm":"rw","state":"Ok","items":[{"name":"GUIDE","value":false},{"name":"CENTERING","value":false},{"name":"FIND","value":true},{"name":"MAX","value":false}]}}"#,
            r#"{"defSwitchVector":{"device":"M","name":"MOUNT_SIDE_OF_PIER","perm":"ro","state":"Ok","items":[{"name":"EAST","value":true},{"name":"WEST","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"M","name":"GEOGRAPHIC_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"LATITUDE","value":45.0},{"name":"LONGITUDE","value":10.0},{"name":"ELEVATION","value":0}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    #[test]
    fn status_precedence() {
        let mut s = store();
        assert_eq!(status(&s, "M"), "Tracking");
        s.apply(parse_frame(r#"{"setNumberVector":{"device":"M","name":"MOUNT_EQUATORIAL_COORDINATES","state":"Busy","items":[]}}"#).unwrap());
        assert_eq!(status(&s, "M"), "Slewing");
        s.apply(parse_frame(r#"{"setSwitchVector":{"device":"M","name":"MOUNT_PARK","state":"Busy","items":[]}}"#).unwrap());
        assert_eq!(status(&s, "M"), "Parking");
        s.apply(parse_frame(r#"{"setSwitchVector":{"device":"M","name":"MOUNT_PARK","state":"Ok","items":[{"name":"PARKED","value":true}]}}"#).unwrap());
        assert_eq!(status(&s, "M"), "Parked");
    }

    #[test]
    fn full_payload_units() {
        let s = store();
        let p = full_payload(&s, "M").unwrap();
        assert_eq!(p["ra"], 90.0);
        assert_eq!(p["de"], 20.0);
        assert_eq!(p["pierSide"], 1);
        assert_eq!(p["slewRate"], 4);
        assert_eq!(p["status"], "Tracking");
        // J2000 differs from JNow by a fraction of a degree, no more.
        let ra0 = p["ra0"].as_f64().unwrap();
        assert!((ra0 - 90.0).abs() < 1.0 && (ra0 - 90.0).abs() > 1e-4);
        // Computed az/alt and HA (no MOUNT_HORIZONTAL_COORDINATES / LST here).
        let ha = p["ha"].as_f64().unwrap();
        assert!((-180.0..=180.0).contains(&ha));
        assert!(p["at"].as_f64().unwrap() <= 90.0);
        assert_eq!(slew_rate_item(0), "GUIDE");
        assert_eq!(slew_rate_item(7), "MAX");
        assert_eq!(slew_rate_item(3), "FIND");
    }

    #[test]
    fn emitter_pushes_status_at_once_and_throttles_coords() {
        let mut s = store();
        let mut e = MountEmitter::default();
        let c = Change::Defined { device: "M".into(), name: "MOUNT_EQUATORIAL_COORDINATES".into() };
        let first = e.on_change(&c, &s, "M");
        // Status (new) + full payload (first ever).
        assert_eq!(first.len(), 2);
        assert!(first[0].contains("\"status\":\"Tracking\"") && !first[0].contains("\"ra\""));
        assert!(first[1].contains("\"ra\":90.0"));
        // Another coordinate update within the second: held back.
        let c = Change::Updated { device: "M".into(), name: "MOUNT_EQUATORIAL_COORDINATES".into() };
        assert!(e.on_change(&c, &s, "M").is_empty());
        assert!(e.tick(&s, "M").is_empty());
        // A status change goes out immediately regardless.
        s.apply(parse_frame(r#"{"setSwitchVector":{"device":"M","name":"MOUNT_TRACKING","state":"Ok","items":[{"name":"ON","value":false}]}}"#).unwrap());
        let c = Change::Updated { device: "M".into(), name: "MOUNT_TRACKING".into() };
        let out = e.on_change(&c, &s, "M");
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("\"status\":\"Idle\""));
        // Other devices and unrelated properties are ignored.
        let c = Change::Updated { device: "X".into(), name: "MOUNT_PARK".into() };
        assert!(e.on_change(&c, &s, "M").is_empty());
    }
}
