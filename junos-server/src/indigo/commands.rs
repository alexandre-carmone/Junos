//! Browser → INDIGO: the Ekos Live commands the backend answers itself.
//!
//! Each handler returns what to send where. Replies mimic the shapes KStars
//! would produce (`kstars/ekos/ekoslive/message.cpp`), because the WASM
//! client parses them in `ws/store.rs::apply_ekos_event` and must not know
//! which backend it talks to. Commands the INDIGO backend can't honour get a
//! `device_message` so the user sees why nothing happened — except the ones
//! the client polls every few seconds, which are swallowed.

use serde_json::{json, Value};
use tracing::debug;

use super::protocol::{interface, Kind, PropertyStore};
use super::scansexa;

/// What the backend knows besides the property mirror.
pub struct Ctx<'a> {
    pub store: &'a PropertyStore,
    /// Resolved captures dir — the browser's default save folder.
    pub captures_dir: &'a str,
    /// The INDIGO server, for the synthetic profile's host/port.
    pub indigo_host: &'a str,
    pub indigo_port: u16,
}

pub enum Out {
    ToBrowser(String),
    ToIndigo(String),
}

/// The synthetic Ekos profile that stands in for "which INDIGO server".
pub const PROFILE_NAME: &str = "INDIGO";

/// Commands the client re-sends on a timer (`ws/mod.rs::BOOTSTRAP_CMDS` and
/// `ws/retry.rs`). Unsupported ones are dropped silently rather than nagging.
const POLLED: &[&str] = &[
    "get_states",
    "train_get_profiles",
    "focus_get_all_settings",
    "capture_get_sequences",
    "align_get_all_settings",
    "guide_get_all_settings",
    "scheduler_get_all_settings",
    "scheduler_get_jobs",
    "livestacker_get_all_settings",
    "option_get",
    "get_drivers",
];

pub fn handle(cmd: &Value, ctx: &Ctx) -> Vec<Out> {
    let Some(ty) = cmd.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let payload = cmd.get("payload").cloned().unwrap_or(Value::Null);
    let reply = |ty: &str, payload: Value| vec![Out::ToBrowser(reply_msg(ty, payload))];

    match ty {
        // ── Devices tab ──────────────────────────────────────────────────
        "get_devices" => reply("get_devices", devices_payload(ctx.store)),
        "device_get" => {
            let Some(device) = payload.get("device").and_then(Value::as_str) else {
                return Vec::new();
            };
            let properties: Vec<Value> = ctx
                .store
                .properties(device)
                .filter_map(super::indi_shape::full)
                .collect();
            reply("device_get", json!({ "device": device, "properties": properties }))
        }
        "device_property_get" => {
            let (Some(device), Some(name)) = (
                payload.get("device").and_then(Value::as_str),
                payload.get("property").and_then(Value::as_str),
            ) else {
                return Vec::new();
            };
            let compact = payload.get("compact").and_then(Value::as_bool).unwrap_or(false);
            match ctx.store.get(device, name) {
                Some(p) => {
                    let shaped = if compact {
                        super::indi_shape::compact(p)
                    } else {
                        super::indi_shape::full(p)
                    };
                    shaped.map(|v| reply("device_property_get", v)).unwrap_or_default()
                }
                // Like KStars (`processDeviceCommands`): no such property, no
                // reply — the client's retry loop handles it.
                None => Vec::new(),
            }
        }
        // Every INDIGO property is pushed to every client; nothing to record.
        "device_property_subscribe" | "device_property_unsubscribe" => Vec::new(),
        "device_property_set" => property_set(&payload, ctx.store).into_iter().collect(),

        // ── Session / profiles ───────────────────────────────────────────
        "get_profiles" => reply("get_profiles", profiles_payload(ctx)),
        "profile_start" | "profile_stop" => {
            let connect = ty == "profile_start";
            ctx.store
                .device_names()
                .filter(|d| ctx.store.interface(d) & interface::AGENT == 0)
                .filter(|d| ctx.store.get(d, "CONNECTION").is_some())
                .filter(|d| ctx.store.connected(d) != connect)
                .map(|d| {
                    let items = [
                        ("CONNECTED".to_string(), json!(connect)),
                        ("DISCONNECTED".to_string(), json!(!connect)),
                    ];
                    Out::ToIndigo(super::protocol::new_vector_msg(Kind::Switch, d, "CONNECTION", &items))
                })
                .collect()
        }
        "file_default_path" => reply("file_default_path", json!(ctx.captures_dir)),
        "astro_get_location" => site_payload(ctx.store)
            .map(|p| reply("astro_get_location", p))
            .unwrap_or_default(),

        // ── Trains / scopes: nothing to offer until the mount phase ──────
        "get_scopes" => reply("get_scopes", json!([])),
        "train_get_all" => reply("train_get_all", json!([])),
        "train_set" => Vec::new(),
        "mount_get_all_settings" | "mount_set_all_settings" => reply(
            "mount_get_all_settings",
            json!({ "executeMeridianFlip": false, "meridianFlipOffsetDegrees": 0.0 }),
        ),
        "capture_get_all_settings" => reply("capture_get_all_settings", json!({})),

        other if POLLED.contains(&other) => Vec::new(),
        other => {
            debug!(command = other, "unsupported with the INDIGO backend");
            reply("device_message", unsupported_payload(other))
        }
    }
}

pub fn reply_msg(ty: &str, payload: Value) -> String {
    json!({ "type": ty, "payload": payload }).to_string()
}

pub fn unsupported_payload(command: &str) -> Value {
    json!({
        "device": PROFILE_NAME,
        "message": format!("{command} is not available with the INDIGO backend"),
    })
}

/// `get_devices` reply: `[{name, connected, interface}]`. Agents are left out
/// — they are INDIGO's own and would confuse the train/device pickers; their
/// properties still arrive through `device_get` if asked by name.
pub fn devices_payload(store: &PropertyStore) -> Value {
    let devices: Vec<Value> = store
        .device_names()
        .filter(|d| store.interface(d) & interface::AGENT == 0)
        .map(|d| {
            json!({
                "name": d,
                "connected": store.connected(d),
                "interface": store.interface(d),
            })
        })
        .collect();
    Value::Array(devices)
}

fn profiles_payload(ctx: &Ctx) -> Value {
    json!({
        "selectedProfile": PROFILE_NAME,
        "profiles": [{
            "name": PROFILE_NAME,
            "auto_connect": true,
            "port_selector": false,
            "mode": "remote",
            "remote_host": ctx.indigo_host,
            "remote_port": ctx.indigo_port,
            "guiding": 0,
            "remote_guiding_host": "",
            "remote_guiding_port": 0,
            "use_web_manager": false,
            "mount": "", "ccd": "", "guider": "", "focuser": "", "filter": "",
            "ao": "", "dome": "", "weather": "",
            "aux1": "", "aux2": "", "aux3": "", "aux4": "",
            "remote": "", "driver_source": "",
        }],
    })
}

/// `astro_get_location` from the first device that knows where it is (the
/// mount first — it is what the sky needs to agree with).
pub fn site_payload(store: &PropertyStore) -> Option<Value> {
    let mut names: Vec<&str> = store.device_names().collect();
    names.sort_by_key(|d| store.interface(d) & interface::MOUNT == 0);
    let geo = names
        .iter()
        .find_map(|d| store.get(d, "GEOGRAPHIC_COORDINATES"))?;
    let lat = geo.number("LATITUDE")?;
    let mut lon = geo.number("LONGITUDE")?;
    // INDIGO longitudes are 0–360 east; the browser expects −180..180.
    if lon > 180.0 {
        lon -= 360.0;
    }
    Some(json!({
        "name": PROFILE_NAME,
        "latitude": lat,
        "longitude": lon,
        "elevation": geo.number("ELEVATION").unwrap_or(0.0),
    }))
}

/// `device_property_set {device, property, elements:[…]}` → one `new*Vector`.
/// Elements come in the Ekos Live shape (`{name, value}` numbers — possibly a
/// sexagesimal string —, `{name, text}`, `{name, state: 0|1}`).
fn property_set(payload: &Value, store: &PropertyStore) -> Option<Out> {
    let device = payload.get("device").and_then(Value::as_str)?;
    let name = payload.get("property").and_then(Value::as_str)?;
    let elements = payload.get("elements").and_then(Value::as_array)?;
    let kind = store.get(device, name).map(|p| p.kind).or_else(|| {
        // Unknown property: infer the kind from the element keys.
        let first = elements.first()?;
        Some(if first.get("text").is_some() {
            Kind::Text
        } else if first.get("state").is_some() {
            Kind::Switch
        } else {
            Kind::Number
        })
    })?;
    let items: Vec<(String, Value)> = elements
        .iter()
        .filter_map(|e| {
            let n = e.get("name").and_then(Value::as_str)?.to_string();
            let v = match kind {
                Kind::Number => match e.get("value")? {
                    Value::Number(x) => json!(x),
                    Value::String(s) => json!(scansexa::parse(s)?),
                    _ => return None,
                },
                Kind::Switch => {
                    let on = match e.get("state")? {
                        Value::Number(x) => x.as_f64()? != 0.0,
                        Value::Bool(b) => *b,
                        Value::String(s) => s == "On" || s == "1",
                        _ => return None,
                    };
                    json!(on)
                }
                Kind::Text => json!(e.get("text").and_then(Value::as_str).unwrap_or("")),
                Kind::Light | Kind::Blob => return None,
            };
            Some((n, v))
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    Some(Out::ToIndigo(super::protocol::new_vector_msg(kind, device, name, &items)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn store() -> PropertyStore {
        let mut s = PropertyStore::default();
        for f in [
            r#"{"defTextVector":{"device":"Mount","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"1"}]}}"#,
            r#"{"defSwitchVector":{"device":"Mount","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":true},{"name":"DISCONNECTED","value":false}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"GEOGRAPHIC_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"LATITUDE","value":48.5},{"name":"LONGITUDE","value":357.7},{"name":"ELEVATION","value":120}]}}"#,
            r#"{"defTextVector":{"device":"Imager Agent","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","value":"16384"}]}}"#,
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":true}]}}"#,
            r#"{"defNumberVector":{"device":"Mount","name":"MOUNT_EQUATORIAL_COORDINATES","perm":"rw","state":"Ok","items":[{"name":"RA","value":1},{"name":"DEC","value":2}]}}"#,
        ] {
            s.apply(parse_frame(f).unwrap());
        }
        s
    }

    fn ctx(store: &PropertyStore) -> Ctx<'_> {
        Ctx { store, captures_dir: "/tmp/caps", indigo_host: "127.0.0.1", indigo_port: 7624 }
    }

    fn browser(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::ToBrowser(s) => serde_json::from_str(s).ok(),
                _ => None,
            })
            .collect()
    }

    fn indigo(outs: &[Out]) -> Vec<Value> {
        outs.iter()
            .filter_map(|o| match o {
                Out::ToIndigo(s) => serde_json::from_str(s).ok(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn get_devices_skips_agents_and_reports_interface() {
        let s = store();
        let out = handle(&json!({"type":"get_devices","payload":{}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["type"], "get_devices");
        let list = b[0]["payload"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "Mount");
        assert_eq!(list[0]["connected"], true);
        assert_eq!(list[0]["interface"], 1);
    }

    #[test]
    fn device_get_and_property_get() {
        let s = store();
        let out = handle(&json!({"type":"device_get","payload":{"device":"Mount","compact":false}}), &ctx(&s));
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["device"], "Mount");
        assert_eq!(b[0]["payload"]["properties"].as_array().unwrap().len(), 4);
        let out = handle(
            &json!({"type":"device_property_get","payload":{"device":"Mount","property":"GEOGRAPHIC_COORDINATES","compact":true}}),
            &ctx(&s),
        );
        let b = browser(&out);
        assert_eq!(b[0]["payload"]["numbers"][0]["value"], 48.5);
        assert!(b[0]["payload"].get("label").is_none());
        let out = handle(&json!({"type":"device_property_get","payload":{"device":"Mount","property":"NOPE"}}), &ctx(&s));
        assert!(out.is_empty());
    }

    #[test]
    fn property_set_maps_each_kind() {
        let s = store();
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"CONNECTION","elements":[{"name":"DISCONNECTED","state":1}]}}),
            &ctx(&s),
        );
        let i = indigo(&out);
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], true);
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"MOUNT_EQUATORIAL_COORDINATES","elements":[{"name":"RA","value":"05 35 17.3"},{"name":"DEC","value":-5.39}]}}),
            &ctx(&s),
        );
        let i = indigo(&out);
        let items = i[0]["newNumberVector"]["items"].as_array().unwrap();
        assert!((items[0]["value"].as_f64().unwrap() - 5.5881).abs() < 1e-3);
        assert_eq!(items[1]["value"], -5.39);
        // Unknown property, text elements: kind inferred.
        let out = handle(
            &json!({"type":"device_property_set","payload":{"device":"Mount","property":"DEVICE_PORT","elements":[{"name":"PORT","text":"/dev/ttyUSB0"}]}}),
            &ctx(&s),
        );
        assert_eq!(indigo(&out)[0]["newTextVector"]["items"][0]["value"], "/dev/ttyUSB0");
    }

    #[test]
    fn profile_stop_disconnects_only_connected_non_agents() {
        let s = store();
        let out = handle(&json!({"type":"profile_stop","payload":{}}), &ctx(&s));
        let i = indigo(&out);
        assert_eq!(i.len(), 1);
        assert_eq!(i[0]["newSwitchVector"]["device"], "Mount");
        assert_eq!(i[0]["newSwitchVector"]["items"][0]["value"], false);
        // Already connected: profile_start has nothing to do.
        assert!(handle(&json!({"type":"profile_start","payload":{}}), &ctx(&s)).is_empty());
    }

    #[test]
    fn site_wraps_longitude_and_profiles_name_the_server() {
        let s = store();
        let site = site_payload(&s).unwrap();
        assert!((site["longitude"].as_f64().unwrap() - (-2.3)).abs() < 1e-9);
        assert_eq!(site["elevation"], 120.0);
        let b = browser(&handle(&json!({"type":"get_profiles","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["payload"]["selectedProfile"], PROFILE_NAME);
        assert_eq!(b[0]["payload"]["profiles"][0]["remote_port"], 7624);
    }

    #[test]
    fn polled_commands_are_silent_and_others_explain() {
        let s = store();
        assert!(handle(&json!({"type":"scheduler_get_jobs","payload":{}}), &ctx(&s)).is_empty());
        let b = browser(&handle(&json!({"type":"scheduler_start_job","payload":{}}), &ctx(&s)));
        assert_eq!(b[0]["type"], "device_message");
        assert!(b[0]["payload"]["message"].as_str().unwrap().contains("scheduler_start_job"));
    }
}
