//! INDI ↔ INDIGO property names, for what the browser asks for *by INDI name*.
//!
//! The WASM client was written against KStars, so `ws/retry.rs` polls
//! `CCD_INFO`, `EQUATORIAL_EOD_COORD`, `FILTER_SLOT`, … and reads INDI's
//! element names out of the replies. INDIGO renamed most of them. This table
//! lets `device_property_get` answer under the INDI name and
//! `device_property_set` write under the INDIGO one. Only the properties the
//! client actually uses are here; everything else is served under its INDIGO
//! name, which is what the Devices tab shows.
//!
//! Aliased replies go out only in answer to an explicit `device_property_get`,
//! never as unsolicited pushes, so the Devices tab never sees an INDI-named
//! duplicate of an INDIGO property.

use super::protocol::{Property, PropertyStore};

/// One INDI property: its INDIGO counterpart and the element renames
/// (INDIGO name → INDI name). Elements missing from the list keep their name.
struct Alias {
    indi: &'static str,
    indigo: &'static str,
    elements: &'static [(&'static str, &'static str)],
}

const ALIASES: &[Alias] = &[
    Alias {
        indi: "CCD_INFO",
        indigo: "CCD_INFO",
        elements: &[
            ("WIDTH", "CCD_MAX_X"),
            ("HEIGHT", "CCD_MAX_Y"),
            ("PIXEL_SIZE", "CCD_PIXEL_SIZE"),
            ("PIXEL_WIDTH", "CCD_PIXEL_SIZE_X"),
            ("PIXEL_HEIGHT", "CCD_PIXEL_SIZE_Y"),
            ("BITS_PER_PIXEL", "CCD_BITSPERPIXEL"),
        ],
    },
    Alias { indi: "CCD_BINNING", indigo: "CCD_BIN", elements: &[("HORIZONTAL", "HOR_BIN"), ("VERTICAL", "VER_BIN")] },
    Alias { indi: "CCD_TEMPERATURE", indigo: "CCD_TEMPERATURE", elements: &[("TEMPERATURE", "CCD_TEMPERATURE_VALUE")] },
    Alias { indi: "CCD_COOLER", indigo: "CCD_COOLER", elements: &[("ON", "COOLER_ON"), ("OFF", "COOLER_OFF")] },
    Alias {
        indi: "CCD_FRAME_TYPE",
        indigo: "CCD_FRAME_TYPE",
        elements: &[("LIGHT", "FRAME_LIGHT"), ("BIAS", "FRAME_BIAS"), ("DARK", "FRAME_DARK"), ("FLAT", "FRAME_FLAT")],
    },
    // INDI splits "what the camera produces" from "what gets transferred";
    // INDIGO has one format list. Serve it under both names.
    Alias { indi: "CCD_CAPTURE_FORMAT", indigo: "CCD_IMAGE_FORMAT", elements: &[] },
    Alias { indi: "CCD_TRANSFER_FORMAT", indigo: "CCD_IMAGE_FORMAT", elements: &[] },
    Alias { indi: "FILTER_SLOT", indigo: "WHEEL_SLOT", elements: &[("SLOT", "FILTER_SLOT_VALUE")] },
    // `SLOT_NAME_n` → `FILTER_SLOT_NAME_n`: handled by prefix below.
    Alias { indi: "FILTER_NAME", indigo: "WHEEL_SLOT_NAME", elements: &[] },
    Alias { indi: "EQUATORIAL_EOD_COORD", indigo: "MOUNT_EQUATORIAL_COORDINATES", elements: &[] },
    Alias { indi: "ABS_FOCUS_POSITION", indigo: "FOCUSER_POSITION", elements: &[("POSITION", "FOCUS_ABSOLUTE_POSITION")] },
    Alias { indi: "FOCUS_TEMPERATURE", indigo: "FOCUSER_TEMPERATURE", elements: &[] },
];

fn alias_for(indi: &str) -> Option<&'static Alias> {
    ALIASES.iter().find(|a| a.indi == indi)
}

fn to_indi_element(alias: &Alias, indigo: &str) -> String {
    if let Some((_, indi)) = alias.elements.iter().find(|(i, _)| *i == indigo) {
        return (*indi).to_string();
    }
    if alias.indi == "FILTER_NAME" {
        if let Some(n) = indigo.strip_prefix("SLOT_NAME_") {
            return format!("FILTER_SLOT_NAME_{n}");
        }
    }
    indigo.to_string()
}

fn to_indigo_element(alias: &Alias, indi: &str) -> String {
    if let Some((indigo, _)) = alias.elements.iter().find(|(_, i)| *i == indi) {
        return (*indigo).to_string();
    }
    if alias.indi == "FILTER_NAME" {
        if let Some(n) = indi.strip_prefix("FILTER_SLOT_NAME_") {
            return format!("SLOT_NAME_{n}");
        }
    }
    indi.to_string()
}

/// The property the browser asked for under an INDI name, as a copy renamed
/// to INDI (property and elements) — ready for `indi_shape`. `None` when the
/// name isn't aliased or the device lacks the INDIGO property.
pub fn lookup(store: &PropertyStore, device: &str, indi_name: &str) -> Option<Property> {
    let alias = alias_for(indi_name)?;
    let mut p = store.get(device, alias.indigo)?.clone();
    p.name = alias.indi.to_string();
    for item in &mut p.items {
        item.name = to_indi_element(alias, &item.name);
    }
    Some(p)
}

/// Where a write to an INDI-named property should go: the INDIGO property
/// name and the element renamer. Non-aliased names pass through unchanged.
pub fn write_target(indi_name: &str) -> (String, impl Fn(&str) -> String) {
    let alias = alias_for(indi_name);
    let name = alias.map(|a| a.indigo).unwrap_or(indi_name).to_string();
    (name, move |element: &str| match alias {
        Some(a) => to_indigo_element(a, element),
        None => element.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    #[test]
    fn ccd_info_and_filter_names_read_as_indi() {
        let mut store = PropertyStore::default();
        store.apply(parse_frame(r#"{"defNumberVector":{"device":"Cam","name":"CCD_INFO","perm":"ro","state":"Ok","items":[{"name":"WIDTH","value":1600},{"name":"PIXEL_SIZE","value":5.2},{"name":"MAX_HORIZONTAL_BIN","value":4}]}}"#).unwrap());
        store.apply(parse_frame(r#"{"defTextVector":{"device":"Wheel","name":"WHEEL_SLOT_NAME","perm":"rw","state":"Ok","items":[{"name":"SLOT_NAME_1","value":"Ha"},{"name":"SLOT_NAME_2","value":"OIII"}]}}"#).unwrap());
        let p = lookup(&store, "Cam", "CCD_INFO").unwrap();
        assert_eq!(p.name, "CCD_INFO");
        assert_eq!(p.number("CCD_MAX_X"), Some(1600.0));
        assert_eq!(p.number("CCD_PIXEL_SIZE"), Some(5.2));
        // Unmapped elements keep their INDIGO name rather than vanishing.
        assert_eq!(p.number("MAX_HORIZONTAL_BIN"), Some(4.0));
        let p = lookup(&store, "Wheel", "FILTER_NAME").unwrap();
        assert_eq!(p.text("FILTER_SLOT_NAME_2"), Some("OIII"));
        assert!(lookup(&store, "Cam", "CCD_GAIN").is_none());
        assert!(lookup(&store, "Cam", "FILTER_SLOT").is_none());
    }

    #[test]
    fn writes_go_to_the_indigo_name() {
        let (name, rename) = write_target("CCD_COOLER");
        assert_eq!(name, "CCD_COOLER");
        assert_eq!(rename("COOLER_ON"), "ON");
        let (name, rename) = write_target("FILTER_SLOT");
        assert_eq!(name, "WHEEL_SLOT");
        assert_eq!(rename("FILTER_SLOT_VALUE"), "SLOT");
        let (name, rename) = write_target("FILTER_NAME");
        assert_eq!(name, "WHEEL_SLOT_NAME");
        assert_eq!(rename("FILTER_SLOT_NAME_3"), "SLOT_NAME_3");
        let (name, rename) = write_target("MOUNT_PARK");
        assert_eq!(name, "MOUNT_PARK");
        assert_eq!(rename("PARKED"), "PARKED");
    }
}
