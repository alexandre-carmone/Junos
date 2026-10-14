//! INDIGO properties in KStars' Ekos Live JSON shape.
//!
//! The browser parses device properties exactly as `kstars/indi/indistd.cpp`
//! emits them (`numberToJson`, `switchToJson`, `textToJson`, `lightToJson`):
//! `{device, name, label, group, state, perm, rule, numbers|switches|texts|lights}`,
//! with the INDI integer enums. The "compact" form (used for pushes and
//! `device_property_get {compact:true}`) drops the labels and hints.

use serde_json::{json, Value};

use super::protocol::{ItemValue, Kind, Property};

/// The full definition, as `device_get` / `device_property_add` carry it.
/// `None` for BLOB properties: the browser has no representation for them.
pub fn full(p: &Property) -> Option<Value> {
    let (key, elements) = elements(p, true)?;
    Some(json!({
        "device": p.device,
        "name": p.name,
        "label": p.label,
        "group": p.group,
        "state": p.state as u8,
        "perm": p.perm as u8,
        "rule": p.rule as u8,
        key: elements,
    }))
}

/// Values only: `{device, name, state, numbers:[{name, value}]…}`.
pub fn compact(p: &Property) -> Option<Value> {
    let (key, elements) = elements(p, false)?;
    Some(json!({
        "device": p.device,
        "name": p.name,
        "state": p.state as u8,
        key: elements,
    }))
}

fn elements(p: &Property, with_labels: bool) -> Option<(&'static str, Vec<Value>)> {
    let key = match p.kind {
        Kind::Number => "numbers",
        Kind::Switch => "switches",
        Kind::Text => "texts",
        Kind::Light => "lights",
        Kind::Blob => return None,
    };
    let elements = p
        .items
        .iter()
        .map(|i| {
            let mut e = json!({ "name": i.name });
            if with_labels {
                e["label"] = json!(i.label);
            }
            match &i.value {
                ItemValue::Number(v) => {
                    e["value"] = json!(v);
                    if with_labels {
                        e["min"] = json!(i.min);
                        e["max"] = json!(i.max);
                        e["step"] = json!(i.step);
                        e["format"] = json!(i.format);
                    }
                }
                ItemValue::Switch(on) => e["state"] = json!(u8::from(*on)),
                ItemValue::Text(s) => e["text"] = json!(s),
                ItemValue::Light(s) => e["state"] = json!(*s as u8),
                ItemValue::Blob(_) => {}
            }
            e
        })
        .collect();
    Some((key, elements))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::{parse_frame, Frame};

    fn prop(frame: &str) -> Property {
        match parse_frame(frame) {
            Some(Frame::Def(p)) => p,
            other => panic!("not a definition: {other:?}"),
        }
    }

    #[test]
    fn number_full_and_compact() {
        let p = prop(r#"{"defNumberVector":{"device":"Cam","name":"CCD_INFO","group":"Camera","label":"Info","perm":"ro","state":"Ok","items":[{"name":"WIDTH","label":"Width","min":0,"max":9000,"step":1,"format":"%g","value":1600}]}}"#);
        let f = full(&p).unwrap();
        assert_eq!(f["perm"], 0);
        assert_eq!(f["state"], 1);
        assert_eq!(f["rule"], 0);
        assert_eq!(f["group"], "Camera");
        assert_eq!(f["numbers"][0]["label"], "Width");
        assert_eq!(f["numbers"][0]["max"], 9000.0);
        assert_eq!(f["numbers"][0]["value"], 1600.0);
        let c = compact(&p).unwrap();
        assert!(c.get("label").is_none());
        assert!(c["numbers"][0].get("min").is_none());
        assert_eq!(c["numbers"][0]["value"], 1600.0);
    }

    #[test]
    fn switch_text_light_and_blob() {
        let p = prop(r#"{"defSwitchVector":{"device":"M","name":"MOUNT_PARK","perm":"rw","state":"Idle","rule":"OneOfMany","items":[{"name":"PARKED","label":"Parked","value":true},{"name":"UNPARKED","label":"Unparked","value":false}]}}"#);
        let f = full(&p).unwrap();
        assert_eq!(f["switches"][0]["state"], 1);
        assert_eq!(f["switches"][1]["state"], 0);
        assert_eq!(f["perm"], 2);

        let p = prop(r#"{"defTextVector":{"device":"M","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_NAME","label":"Name","value":"Mount Simulator"}]}}"#);
        assert_eq!(compact(&p).unwrap()["texts"][0]["text"], "Mount Simulator");

        let p = prop(r#"{"defLightVector":{"device":"M","name":"MOUNT_STATE","perm":"ro","state":"Ok","items":[{"name":"SLEW","label":"Slew","value":"Alert"}]}}"#);
        assert_eq!(full(&p).unwrap()["lights"][0]["state"], 3);

        let p = prop(r#"{"defBLOBVector":{"device":"Cam","name":"CCD_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE","label":"Image"}]}}"#);
        assert!(full(&p).is_none());
        assert!(compact(&p).is_none());
    }
}
