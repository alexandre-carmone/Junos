//! INDIGO's JSON wire format and the property mirror built from it.
//!
//! Reference: `indigo_docs/PROTOCOLS.md` and `indigo_libs/indigo_json.c`.
//! Every frame is one JSON object with a single key naming the message:
//! `defNumberVector`, `setSwitchVector`, `deleteProperty`, `message`, … and,
//! client → server, `getProperties` / `new*Vector`. Items of every type carry
//! their payload in `value` (number, bool, string, light state string, or a
//! BLOB URL); BLOBs are never inlined in JSON mode.

use indexmap::IndexMap;
use serde_json::{json, Value};

/// INDI/INDIGO property state. The discriminants are INDI's `IPState`, which
/// is what the Ekos Live JSON shape and the browser's `IndiProperty` use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle = 0,
    Ok = 1,
    Busy = 2,
    Alert = 3,
}

impl State {
    pub fn parse(s: &str) -> Self {
        match s {
            "Ok" => Self::Ok,
            "Busy" => Self::Busy,
            "Alert" => Self::Alert,
            _ => Self::Idle,
        }
    }
}

/// Property permission. Discriminants are INDI's `IPerm` (ro 0, wo 1, rw 2):
/// INDIGO numbers them differently (ro 1, rw 2, wo 3) but spells them the same
/// on the wire, and the browser reads the INDI ints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Perm {
    ReadOnly = 0,
    WriteOnly = 1,
    ReadWrite = 2,
}

impl Perm {
    pub fn parse(s: &str) -> Self {
        match s {
            "ro" => Self::ReadOnly,
            "wo" => Self::WriteOnly,
            _ => Self::ReadWrite,
        }
    }
}

/// Switch rule. Discriminants are INDI's `ISRule`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    OneOfMany = 0,
    AtMostOne = 1,
    AnyOfMany = 2,
}

impl Rule {
    pub fn parse(s: &str) -> Self {
        match s {
            "AtMostOne" => Self::AtMostOne,
            "AnyOfMany" => Self::AnyOfMany,
            _ => Self::OneOfMany,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Number,
    Switch,
    Text,
    Light,
    Blob,
}

impl Kind {
    /// The `new*Vector` frame key a client uses to write this kind.
    pub fn new_vector_key(self) -> &'static str {
        match self {
            Self::Number => "newNumberVector",
            Self::Switch => "newSwitchVector",
            Self::Text => "newTextVector",
            Self::Light => "newLightVector",
            Self::Blob => "newBLOBVector",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemValue {
    Number(f64),
    Switch(bool),
    Text(String),
    Light(State),
    /// BLOB link, present only while the property is `Ok` and data exists.
    Blob(Option<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub name: String,
    pub label: String,
    pub value: ItemValue,
    // Number-only presentation hints; zero / empty for the other kinds.
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub format: String,
}

impl Item {
    pub fn number(&self) -> Option<f64> {
        match self.value {
            ItemValue::Number(v) => Some(v),
            _ => None,
        }
    }

    pub fn on(&self) -> bool {
        matches!(self.value, ItemValue::Switch(true))
    }

    pub fn text(&self) -> Option<&str> {
        match &self.value {
            ItemValue::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn blob_url(&self) -> Option<&str> {
        match &self.value {
            ItemValue::Blob(Some(u)) => Some(u),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Property {
    pub device: String,
    pub name: String,
    pub kind: Kind,
    pub group: String,
    pub label: String,
    pub perm: Perm,
    pub state: State,
    pub rule: Rule,
    pub items: Vec<Item>,
}

impl Property {
    pub fn item(&self, name: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.name == name)
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        self.item(name).and_then(Item::number)
    }

    pub fn on(&self, name: &str) -> bool {
        self.item(name).is_some_and(Item::on)
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        self.item(name).and_then(Item::text)
    }

    /// Name of the first switch that is on, for one-of-many switches.
    pub fn selected(&self) -> Option<&str> {
        self.items.iter().find(|i| i.on()).map(|i| i.name.as_str())
    }
}

/// One frame from the INDIGO server, decoded.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Def(Property),
    Set {
        device: String,
        name: String,
        state: Option<State>,
        items: Vec<(String, ItemValue)>,
        message: Option<String>,
    },
    Delete {
        device: String,
        /// `None` deletes every property of the device (the device itself).
        name: Option<String>,
    },
    Message {
        device: Option<String>,
        text: String,
    },
}

/// Decode one text frame. `None` for anything we don't understand — INDIGO
/// may add message kinds, and a relay must not die on them.
pub fn parse_frame(text: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;

    // `{"message": "text", "device": "…"}` — the text sits at top level.
    if let Some(text) = obj.get("message").and_then(Value::as_str) {
        return Some(Frame::Message {
            device: obj.get("device").and_then(Value::as_str).map(str::to_string),
            text: text.to_string(),
        });
    }

    let (key, body) = obj.iter().next()?;
    let body = body.as_object()?;
    let device = body.get("device").and_then(Value::as_str)?.to_string();
    let str_field = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_string);

    let kind = match key.as_str() {
        "defNumberVector" | "setNumberVector" => Kind::Number,
        "defSwitchVector" | "setSwitchVector" => Kind::Switch,
        "defTextVector" | "setTextVector" => Kind::Text,
        "defLightVector" | "setLightVector" => Kind::Light,
        "defBLOBVector" | "setBLOBVector" => Kind::Blob,
        "deleteProperty" => {
            return Some(Frame::Delete { device, name: str_field("name") });
        }
        "message" => {
            return Some(Frame::Message {
                device: Some(device),
                text: str_field("message").unwrap_or_default(),
            });
        }
        _ => return None,
    };

    let name = str_field("name")?;
    let items = body.get("items").and_then(Value::as_array);
    let message = str_field("message");

    if key.starts_with("def") {
        let items = items
            .map(|arr| arr.iter().filter_map(|i| parse_item(kind, i)).collect())
            .unwrap_or_default();
        return Some(Frame::Def(Property {
            device,
            name: name.clone(),
            kind,
            group: str_field("group").unwrap_or_else(|| "Main".into()),
            label: str_field("label").unwrap_or(name),
            perm: Perm::parse(body.get("perm").and_then(Value::as_str).unwrap_or("rw")),
            state: State::parse(body.get("state").and_then(Value::as_str).unwrap_or("Idle")),
            rule: Rule::parse(body.get("rule").and_then(Value::as_str).unwrap_or("OneOfMany")),
            items,
        }));
    }

    let items = items
        .map(|arr| {
            arr.iter()
                .filter_map(|i| {
                    let name = i.get("name").and_then(Value::as_str)?;
                    Some((name.to_string(), parse_value(kind, i)?))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Frame::Set {
        device,
        name,
        state: body.get("state").and_then(Value::as_str).map(State::parse),
        items,
        message,
    })
}

fn parse_value(kind: Kind, item: &Value) -> Option<ItemValue> {
    let v = item.get("value");
    Some(match kind {
        Kind::Number => ItemValue::Number(v?.as_f64()?),
        Kind::Switch => ItemValue::Switch(v?.as_bool()?),
        Kind::Text => ItemValue::Text(v.and_then(Value::as_str).unwrap_or("").to_string()),
        Kind::Light => ItemValue::Light(State::parse(v.and_then(Value::as_str).unwrap_or("Idle"))),
        Kind::Blob => ItemValue::Blob(
            v.and_then(Value::as_str)
                .or_else(|| item.get("url").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        ),
    })
}

fn parse_item(kind: Kind, item: &Value) -> Option<Item> {
    let name = item.get("name").and_then(Value::as_str)?.to_string();
    let num = |k: &str| item.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    // A definition always carries a value; a BLOB's may be absent (no data yet).
    let value = parse_value(kind, item).or(match kind {
        Kind::Blob => Some(ItemValue::Blob(None)),
        _ => None,
    })?;
    Some(Item {
        label: item.get("label").and_then(Value::as_str).unwrap_or(&name).to_string(),
        name,
        value,
        min: num("min"),
        max: num("max"),
        step: num("step"),
        format: item.get("format").and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

/// The handshake: ask for every property of every device.
pub fn get_properties_msg() -> String {
    json!({ "getProperties": { "version": 512, "client": "junos-server" } }).to_string()
}

/// A `new*Vector` write. `items` are `(name, value)` pairs already in INDIGO's
/// JSON types (number, bool or string).
pub fn new_vector_msg(kind: Kind, device: &str, name: &str, items: &[(String, Value)]) -> String {
    let items: Vec<Value> = items
        .iter()
        .map(|(n, v)| json!({ "name": n, "value": v }))
        .collect();
    json!({ kind.new_vector_key(): { "device": device, "name": name, "items": items } }).to_string()
}

/// What `PropertyStore::apply` tells the translator happened.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Defined { device: String, name: String },
    Updated { device: String, name: String },
    /// `name: None` — the whole device went away.
    Deleted { device: String, name: Option<String> },
    Message { device: Option<String>, text: String },
}

/// Every property of every device, in definition order (the browser renders
/// INDI groups in the order their first property arrived).
#[derive(Default, Debug)]
pub struct PropertyStore {
    devices: IndexMap<String, IndexMap<String, Property>>,
}

impl PropertyStore {
    pub fn apply(&mut self, frame: Frame) -> Option<Change> {
        match frame {
            Frame::Def(p) => {
                let device = p.device.clone();
                let name = p.name.clone();
                self.devices.entry(device.clone()).or_default().insert(name.clone(), p);
                Some(Change::Defined { device, name })
            }
            Frame::Set { device, name, state, items, message: _ } => {
                let p = self.devices.get_mut(&device)?.get_mut(&name)?;
                if let Some(s) = state {
                    p.state = s;
                }
                for (item_name, value) in items {
                    if let Some(i) = p.items.iter_mut().find(|i| i.name == item_name) {
                        i.value = value;
                    }
                }
                Some(Change::Updated { device, name })
            }
            Frame::Delete { device, name } => match name {
                Some(name) => {
                    self.devices.get_mut(&device)?.shift_remove(&name)?;
                    Some(Change::Deleted { device, name: Some(name) })
                }
                None => {
                    self.devices.shift_remove(&device)?;
                    Some(Change::Deleted { device, name: None })
                }
            },
            Frame::Message { device, text } => Some(Change::Message { device, text }),
        }
    }

    pub fn get(&self, device: &str, name: &str) -> Option<&Property> {
        self.devices.get(device)?.get(name)
    }

    pub fn device_names(&self) -> impl Iterator<Item = &str> {
        self.devices.keys().map(String::as_str)
    }

    pub fn properties(&self, device: &str) -> impl Iterator<Item = &Property> {
        self.devices.get(device).into_iter().flat_map(|m| m.values())
    }

    /// `INFO.DEVICE_INTERFACE`, the device-type bitmask (text item holding an
    /// integer). `0` when the device has no INFO property (yet).
    pub fn interface(&self, device: &str) -> u32 {
        self.get(device, "INFO")
            .and_then(|p| p.text("DEVICE_INTERFACE"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn connected(&self, device: &str) -> bool {
        self.get(device, "CONNECTION").is_some_and(|p| p.on("CONNECTED"))
    }
}

/// `INFO.DEVICE_INTERFACE` bits (`indigo_bus.h::indigo_device_interface`).
pub mod interface {
    pub const MOUNT: u32 = 1 << 0;
    pub const CCD: u32 = 1 << 1;
    pub const GUIDER: u32 = 1 << 2;
    pub const FOCUSER: u32 = 1 << 3;
    pub const WHEEL: u32 = 1 << 4;
    pub const AGENT: u32 = 1 << 14;
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEF_NUMBER: &str = r#"{"defNumberVector":{"version":512,"device":"CCD Imager Simulator","name":"CCD_EXPOSURE","group":"Camera","label":"Start exposure","perm":"rw","state":"Idle","items":[{"name":"EXPOSURE","label":"Start exposure","min":0,"max":10000,"step":1,"format":"%g","target":0,"value":0}]}}"#;
    const DEF_SWITCH: &str = r#"{"defSwitchVector":{"version":512,"device":"Server","name":"RESTART","group":"Main","label":"Restart","perm":"rw","state":"Idle","rule":"AnyOfMany","items":[{"name":"RESTART","label":"Restart server","value":false}]}}"#;

    #[test]
    fn def_number_round_trips() {
        let Some(Frame::Def(p)) = parse_frame(DEF_NUMBER) else { panic!("def") };
        assert_eq!(p.kind, Kind::Number);
        assert_eq!(p.group, "Camera");
        assert_eq!(p.perm, Perm::ReadWrite);
        assert_eq!(p.items[0].max, 10000.0);
        assert_eq!(p.items[0].format, "%g");
        assert_eq!(p.number("EXPOSURE"), Some(0.0));
    }

    #[test]
    fn set_updates_values_and_state_in_place() {
        let mut store = PropertyStore::default();
        store.apply(parse_frame(DEF_NUMBER).unwrap());
        let set = r#"{"setNumberVector":{"device":"CCD Imager Simulator","name":"CCD_EXPOSURE","state":"Busy","items":[{"name":"EXPOSURE","value":2.5}]}}"#;
        let change = store.apply(parse_frame(set).unwrap());
        assert_eq!(change, Some(Change::Updated { device: "CCD Imager Simulator".into(), name: "CCD_EXPOSURE".into() }));
        let p = store.get("CCD Imager Simulator", "CCD_EXPOSURE").unwrap();
        assert_eq!(p.state, State::Busy);
        assert_eq!(p.number("EXPOSURE"), Some(2.5));
        // The definition's presentation hints survive the update.
        assert_eq!(p.items[0].label, "Start exposure");
    }

    #[test]
    fn set_for_unknown_property_is_ignored() {
        let mut store = PropertyStore::default();
        let set = r#"{"setSwitchVector":{"device":"X","name":"Y","state":"Ok","items":[]}}"#;
        assert_eq!(store.apply(parse_frame(set).unwrap()), None);
    }

    #[test]
    fn switch_rule_and_delete() {
        let mut store = PropertyStore::default();
        store.apply(parse_frame(DEF_SWITCH).unwrap());
        assert_eq!(store.get("Server", "RESTART").unwrap().rule, Rule::AnyOfMany);
        let del = r#"{"deleteProperty":{"device":"Server","name":"RESTART"}}"#;
        assert_eq!(
            store.apply(parse_frame(del).unwrap()),
            Some(Change::Deleted { device: "Server".into(), name: Some("RESTART".into()) })
        );
        assert!(store.get("Server", "RESTART").is_none());
        // The device entry itself survives until a name-less delete.
        assert_eq!(store.device_names().count(), 1);
        store.apply(parse_frame(r#"{"deleteProperty":{"device":"Server"}}"#).unwrap());
        assert_eq!(store.device_names().count(), 0);
    }

    #[test]
    fn blob_and_light_values() {
        let blob = r#"{"defBLOBVector":{"device":"Cam","name":"CCD_IMAGE","group":"Camera","label":"Image","perm":"ro","state":"Ok","items":[{"name":"IMAGE","label":"Image","value":"/blob/0x1234.fits"}]}}"#;
        let Some(Frame::Def(p)) = parse_frame(blob) else { panic!("blob") };
        assert_eq!(p.items[0].blob_url(), Some("/blob/0x1234.fits"));
        let empty = r#"{"defBLOBVector":{"device":"Cam","name":"CCD_IMAGE","perm":"ro","state":"Idle","items":[{"name":"IMAGE","label":"Image"}]}}"#;
        let Some(Frame::Def(p)) = parse_frame(empty) else { panic!("blob") };
        assert_eq!(p.items[0].value, ItemValue::Blob(None));
        let light = r#"{"defLightVector":{"device":"M","name":"MOUNT_STATE","perm":"ro","state":"Ok","items":[{"name":"SLEW","label":"Slew","value":"Busy"}]}}"#;
        let Some(Frame::Def(p)) = parse_frame(light) else { panic!("light") };
        assert_eq!(p.items[0].value, ItemValue::Light(State::Busy));
    }

    #[test]
    fn messages_and_interface() {
        assert_eq!(
            parse_frame(r#"{"message":"Driver loaded","device":"Server"}"#),
            Some(Frame::Message { device: Some("Server".into()), text: "Driver loaded".into() })
        );
        let mut store = PropertyStore::default();
        let info = r#"{"defTextVector":{"device":"Mount","name":"INFO","perm":"ro","state":"Ok","items":[{"name":"DEVICE_INTERFACE","label":"Interface","value":"1"}]}}"#;
        store.apply(parse_frame(info).unwrap());
        assert_eq!(store.interface("Mount") & interface::MOUNT, interface::MOUNT);
        assert!(!store.connected("Mount"));
    }

    #[test]
    fn new_vector_shape() {
        let msg = new_vector_msg(Kind::Switch, "Mount", "MOUNT_PARK", &[("PARKED".into(), json!(true))]);
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["newSwitchVector"]["device"], "Mount");
        assert_eq!(v["newSwitchVector"]["items"][0]["value"], true);
        assert!(parse_frame("not json").is_none());
    }
}
