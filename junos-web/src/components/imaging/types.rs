//! Shared field-definition types and data tables for the Imaging tab.
//!
//! `Field` declarations map a KStars widget objectName (defined in
//! `kstars/ekos/capture/camera.ui`) to a human label and editor kind. The
//! keys are what comes back from `capture_get_all_settings` and what
//! `capture_set_all_settings` expects.

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::i18n::Translations;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Number,
    /// Dropdown whose options come from the active camera / filter wheel.
    /// The closure receives both snapshots and returns the option list — if
    /// it returns empty, the field renders as a free text input so the user
    /// can still type a value before the device pushes its property.
    ComboDynamic(fn(&CameraSnapshot, &FilterWheelSnapshot) -> Vec<String>),
    /// Filter dropdown — always rendered as `<select>`. When the option
    /// list is empty (no filter wheel attached / not yet reporting) it
    /// shows a single disabled placeholder option from i18n; never falls
    /// back to a free-text input.
    ComboFilter(fn(&CameraSnapshot, &FilterWheelSnapshot) -> Vec<String>),
}

#[derive(Clone, Copy)]
pub(super) struct Field {
    pub(super) key: &'static str,
    pub(super) label: fn(&Translations) -> &'static str,
    pub(super) kind: Kind,
}

// Exposure presets in seconds — covers fast focus frames (1 ms) up to long
// subs (5 min). The chip row below the input lets the user pick one with a
// single tap; matches by `(value - preset).abs() < 1e-6`.
pub(super) const EXPOSURE_PRESETS: &[f64] = &[0.001, 0.01, 0.1, 1.0, 5.0, 30.0, 60.0, 300.0];

pub(super) const ONE_SHOT_GAIN_FIELDS: &[Field] = &[
    Field {
        key: "captureGainN",
        label: |t| t.field_gain,
        kind: Kind::Number,
    },
    Field {
        key: "captureISOS",
        label: |t| t.field_iso,
        kind: Kind::ComboDynamic(|c, _| c.iso_options.clone()),
    },
];

pub(super) const FILTER_FIELDS: &[Field] = &[Field {
    key: "FilterPosCombo",
    label: |t| t.field_filter,
    kind: Kind::ComboFilter(|_, fw| fw.filter_names.clone()),
}];

#[derive(Clone)]
pub(super) struct SequenceRow {
    pub(super) index: usize,
    pub(super) completed: String,
    pub(super) total: String,
    pub(super) exp: String,
    pub(super) ftype: String,
    pub(super) filter: String,
    #[allow(dead_code)]
    pub(super) bin: String,
    pub(super) status: String,
}
