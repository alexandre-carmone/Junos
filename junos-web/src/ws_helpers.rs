//! Small helpers shared by the tab modules. Centralises three patterns that
//! were previously copy-pasted across `focus.rs`, `imaging.rs`,
//! `polar_align.rs`, `scheduler.rs` and `guide/mod.rs`:
//!
//!   * `send_cmd` — wrap `{type, payload}` JSON and push it to `SendCmd`.
//!   * `dispatch_setting` — single-key `*_set_all_settings` update, with or
//!     without a `{settings:{…}}` wrapper (align uses the wrapper, the others
//!     don't — see `message.cpp:673` vs the align handler).
//!   * `send_device_property_set` — INDI `device_property_set` with the
//!     `elements` array shaped by the caller.
//!
//! Plus `extract_indi_number` for reading a named element out of a
//! `device_property_get`/`set` payload's `numbers` array.

use crate::ws::SendCmd;
use serde_json::{json, Map, Value};

pub fn send_cmd(send: &SendCmd, ty: &str, payload: Value) {
    send(json!({ "type": ty, "payload": payload }).to_string());
}

pub fn dispatch_setting(
    send: &SendCmd,
    cmd: &str,
    wrapper_key: Option<&str>,
    key: &str,
    value: Value,
) {
    let mut map = Map::new();
    map.insert(key.to_string(), value);
    let payload = match wrapper_key {
        Some(w) => json!({ w: Value::Object(map) }),
        None    => Value::Object(map),
    };
    send_cmd(send, cmd, payload);
}

pub fn send_device_property_set(send: &SendCmd, device: &str, property: &str, elements: Value) {
    send_cmd(send, "device_property_set", json!({
        "device": device,
        "property": property,
        "elements": elements,
    }));
}

/// Push a new observer location into KStars' live `geo()` truth. KStars exposes
/// no direct "set location" command; the only path is via a connected INDI
/// device: point KStars' `locationSource` at the mount, then write
/// `GEOGRAPHIC_COORD` to it. The driver echoes it back `IPS_OK` and KStars'
/// `GenericDevice::processNumber` (indistd.cpp:361) calls `setLocation()`.
/// INDI `LONG` is 0..360° East, so a Western (negative) longitude is wrapped.
/// `ELEV` is intentionally omitted so KStars keeps its current elevation.
pub fn push_site_to_kstars(send: &SendCmd, device: &str, lat: f64, lon: f64) {
    // "Set & leave": make the mount KStars' location master (idempotent).
    send_cmd(send, "option_set", json!({
        "options": [ { "name": "locationSource", "value": device } ],
    }));
    let lon_east = if lon < 0.0 { lon + 360.0 } else { lon };
    send_device_property_set(send, device, "GEOGRAPHIC_COORD", json!([
        { "name": "LAT",  "value": lat },
        { "name": "LONG", "value": lon_east },
    ]));
}

pub fn extract_indi_number(payload: &Value, name: &str) -> Option<f64> {
    payload["numbers"].as_array()?
        .iter()
        .find(|el| el["name"].as_str() == Some(name))
        .and_then(|el| el["value"].as_f64())
}
