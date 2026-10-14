//! INDIGO → browser: the Ekos Live events a property change turns into.
//!
//! Generic INDI traffic only (what the Devices tab consumes): the module
//! emitters (`new_mount_state`, `new_capture_state`, …) are layered on top by
//! the session, which knows which device plays which role.

use serde_json::json;

use super::commands::{devices_payload, reply_msg};
use super::indi_shape;
use super::protocol::{Change, PropertyStore};

/// Messages to broadcast for one `PropertyStore` change. The `Deleted` and
/// `Defined` arms also refresh the device list, which the client otherwise
/// only asks for on its bootstrap timer.
pub fn for_change(change: &Change, store: &PropertyStore) -> Vec<String> {
    let mut out = Vec::new();
    match change {
        Change::Defined { device, name } => {
            if let Some(full) = store.get(device, name).and_then(indi_shape::full) {
                out.push(reply_msg("device_property_add", full));
            }
            if name == "INFO" || name == "CONNECTION" {
                out.push(reply_msg("get_devices", devices_payload(store)));
            }
        }
        Change::Updated { device, name } => {
            if let Some(compact) = store.get(device, name).and_then(indi_shape::compact) {
                out.push(reply_msg("device_property_get", compact));
            }
            if name == "CONNECTION" {
                out.push(reply_msg("get_devices", devices_payload(store)));
            }
        }
        Change::Deleted { device, name } => {
            match name {
                Some(name) => out.push(reply_msg(
                    "device_property_remove",
                    json!({ "device": device, "name": name }),
                )),
                None => out.push(reply_msg("get_devices", devices_payload(store))),
            }
        }
        Change::Message { device, text } => {
            out.push(reply_msg(
                "device_message",
                json!({ "device": device.as_deref().unwrap_or("Server"), "message": text }),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;
    use serde_json::Value;

    fn types(msgs: &[String]) -> Vec<String> {
        msgs.iter()
            .map(|m| serde_json::from_str::<Value>(m).unwrap()["type"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn define_update_delete() {
        let mut store = PropertyStore::default();
        let def = parse_frame(r#"{"defSwitchVector":{"device":"M","name":"CONNECTION","perm":"rw","state":"Ok","items":[{"name":"CONNECTED","value":false},{"name":"DISCONNECTED","value":true}]}}"#).unwrap();
        let c = store.apply(def).unwrap();
        assert_eq!(types(&for_change(&c, &store)), ["device_property_add", "get_devices"]);

        let set = parse_frame(r#"{"setSwitchVector":{"device":"M","name":"CONNECTION","state":"Ok","items":[{"name":"CONNECTED","value":true}]}}"#).unwrap();
        let c = store.apply(set).unwrap();
        let msgs = for_change(&c, &store);
        assert_eq!(types(&msgs), ["device_property_get", "get_devices"]);
        let v: Value = serde_json::from_str(&msgs[1]).unwrap();
        assert_eq!(v["payload"][0]["connected"], true);

        let del = parse_frame(r#"{"deleteProperty":{"device":"M","name":"CONNECTION"}}"#).unwrap();
        let c = store.apply(del).unwrap();
        assert_eq!(types(&for_change(&c, &store)), ["device_property_remove"]);

        let msg = Change::Message { device: None, text: "hi".into() };
        let v: Value = serde_json::from_str(&for_change(&msg, &store)[0]).unwrap();
        assert_eq!(v["payload"]["device"], "Server");
    }
}
