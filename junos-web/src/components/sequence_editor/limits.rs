//! `SeqLimits` — the sequence-wide head of an `.esq`: refocus triggers, the
//! in-sequence HFR check and the guide-drift limits (KStars' Capture
//! "Limits" dialog).
//!
//! KStars copies them into its global options whenever Capture loads the
//! file (`SequenceQueue::setOptions`, sequencequeue.cpp) — the Scheduler at
//! each job start, Imaging on `capture_load_sequence_file` — and they stay in
//! force until the next load. They're always written out: a missing element
//! doesn't keep Ekos' value, it re-applies the last loaded file's (the
//! `m_*Set` flags are never reset).
//!
//! Like `SeqFrame`, edited values are kept as typed; `is_valid` decides.

use serde_json::Value;

use crate::i18n::Translations;

#[derive(Clone, Debug, PartialEq)]
pub struct SeqLimits {
    /// `<RefocusEveryN>`, in whole minutes.
    pub refocus_every:       bool,
    pub refocus_every_min:   String,
    /// `<RefocusOnTemperatureDelta>`, in °C.
    pub refocus_temp:        bool,
    pub refocus_temp_delta:  String,
    /// `<RefocusOnMeridianFlip>`.
    pub refocus_flip:        bool,
    /// `<HFRCheck>` — only switched here; its algorithm, threshold, frame
    /// count and fixed HFR are carried from Ekos so a sequence doesn't reset
    /// them.
    pub hfr_check:           bool,
    pub hfr_algorithm:       u8,
    pub hfr_threshold:       f64,
    pub hfr_frames:          u32,
    pub hfr_deviation:       f64,
    /// `<GuideDeviation>`: abort the exposure past this drift, in arcsec.
    pub guide_abort:         bool,
    pub guide_abort_arcsec:  String,
    /// `<GuideStartDeviation>`: wait for the drift to drop below this.
    pub guide_start:         bool,
    pub guide_start_arcsec:  String,
    /// The user changed a value here: stop following Ekos' settings.
    pub edited:              bool,
}

/// KStars' own defaults (kstars.kcfg): everything off.
impl Default for SeqLimits {
    fn default() -> Self {
        Self {
            refocus_every:      false,
            refocus_every_min:  "60".into(),
            refocus_temp:       false,
            refocus_temp_delta: "1".into(),
            refocus_flip:       false,
            hfr_check:          false,
            hfr_algorithm:      0,
            hfr_threshold:      10.0,
            hfr_frames:         1,
            hfr_deviation:      0.5,
            guide_abort:        false,
            guide_abort_arcsec: "2".into(),
            guide_start:        false,
            guide_start_arcsec: "2".into(),
            edited:             false,
        }
    }
}

/// A number > 0.
fn positive(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0)
}

impl SeqLimits {
    /// Minutes between refocus runs — KStars reads them with `toInt`.
    pub fn refocus_every_n(&self) -> Option<u32> {
        self.refocus_every_min.trim().parse::<u32>().ok().filter(|n| *n >= 1)
    }

    pub fn temp_delta(&self) -> Option<f64> { positive(&self.refocus_temp_delta) }
    pub fn guide_abort_value(&self) -> Option<f64> { positive(&self.guide_abort_arcsec) }
    pub fn guide_start_value(&self) -> Option<f64> { positive(&self.guide_start_arcsec) }

    /// Every enabled limit has a usable value (a disabled one is written
    /// with its default instead).
    pub fn is_valid(&self) -> bool {
        (!self.refocus_every || self.refocus_every_n().is_some())
            && (!self.refocus_temp || self.temp_delta().is_some())
            && (!self.guide_abort || self.guide_abort_value().is_some())
            && (!self.guide_start || self.guide_start_value().is_some())
    }

    /// The limits Ekos currently applies, from `capture_get_all_settings`
    /// (`Camera::getAllSettings` walks the Limits dialog's widgets too).
    /// `None` until KStars has reported them.
    pub fn from_capture_settings(s: &Value) -> Option<Self> {
        s.get("enforceRefocusEveryN")?;
        let d = Self::default();
        let on = |k: &str| s[k].as_bool().unwrap_or(false);
        let num = |k: &str| s[k].as_f64().filter(|v| v.is_finite());
        let text = |k: &str, fallback: &str| num(k).map_or_else(|| fallback.to_string(), |v| v.to_string());
        Some(Self {
            refocus_every:      on("enforceRefocusEveryN"),
            refocus_every_min:  text("refocusEveryN", &d.refocus_every_min),
            refocus_temp:       on("enforceAutofocusOnTemperature"),
            refocus_temp_delta: text("maxFocusTemperatureDelta", &d.refocus_temp_delta),
            refocus_flip:       on("refocusAfterMeridianFlip"),
            hfr_check:          on("enforceAutofocusHFR"),
            // A combo box reports its text, translated with the KStars UI;
            // an unknown one falls back to "Last Autofocus".
            hfr_algorithm:      match s["hFRCheckAlgorithm"].as_str() {
                Some("Fixed") => 1,
                Some("Median Measure") => 2,
                _ => 0,
            },
            hfr_threshold:      num("hFRThresholdPercentage").unwrap_or(d.hfr_threshold),
            hfr_frames:         num("inSequenceCheckFrames").map_or(d.hfr_frames, |v| v.max(1.0) as u32),
            hfr_deviation:      num("hFRDeviation").unwrap_or(d.hfr_deviation),
            guide_abort:        on("enforceGuideDeviation"),
            guide_abort_arcsec: text("guideDeviation", &d.guide_abort_arcsec),
            guide_start:        on("enforceStartGuiderDrift"),
            guide_start_arcsec: text("startGuideDeviation", &d.guide_start_arcsec),
            edited:             false,
        })
    }

    /// The `.esq` head, one element per line. A disabled limit with an
    /// unusable value is written with KStars' default.
    pub fn esq_xml(&self) -> String {
        let flag = |on: bool| if on { "true" } else { "false" };
        let every = self.refocus_every_n().unwrap_or(60);
        let delta = self.temp_delta().unwrap_or(1.0);
        let abort = self.guide_abort_value().unwrap_or(2.0);
        let start = self.guide_start_value().unwrap_or(2.0);
        format!(
            "<GuideDeviation enabled='{}'>{abort}</GuideDeviation>\n\
             <GuideStartDeviation enabled='{}'>{start}</GuideStartDeviation>\n\
             <HFRCheck enabled='{}'><HFRDeviation>{}</HFRDeviation>\
             <HFRCheckAlgorithm>{}</HFRCheckAlgorithm><HFRCheckThreshold>{}</HFRCheckThreshold>\
             <HFRCheckFrames>{}</HFRCheckFrames></HFRCheck>\n\
             <RefocusOnTemperatureDelta enabled='{}'>{delta}</RefocusOnTemperatureDelta>\n\
             <RefocusEveryN enabled='{}'>{every}</RefocusEveryN>\n\
             <RefocusOnMeridianFlip enabled='{}'/>\n",
            flag(self.guide_abort), flag(self.guide_start),
            flag(self.hfr_check), self.hfr_deviation, self.hfr_algorithm, self.hfr_threshold, self.hfr_frames,
            flag(self.refocus_temp), flag(self.refocus_every), flag(self.refocus_flip),
        )
    }

    /// The enabled limits, short: "Every 60 min", "ΔT 1 °C", "Guide < 2″"…
    pub fn summary(&self, tr: &'static Translations) -> Vec<String> {
        let mut parts = Vec::new();
        if self.refocus_every {
            parts.push(format!("{} {} min", tr.seq_sum_every, self.refocus_every_min.trim()));
        }
        if self.refocus_temp {
            parts.push(format!("\u{0394}T {} \u{00b0}C", self.refocus_temp_delta.trim()));
        }
        if self.refocus_flip {
            parts.push(tr.seq_sum_flip.to_string());
        }
        if self.hfr_check {
            parts.push("HFR".to_string());
        }
        if self.guide_abort {
            parts.push(format!("{} < {}\u{2033}", tr.seq_sum_guide, self.guide_abort_arcsec.trim()));
        }
        if self.guide_start {
            parts.push(format!("{} < {}\u{2033}", tr.seq_sum_start, self.guide_start_arcsec.trim()));
        }
        parts
    }
}
