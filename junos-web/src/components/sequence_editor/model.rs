//! `SeqFrame` — one job row of the sequence editor — plus the validation and
//! duration helpers shared by the editor UI and its three callers.
//!
//! Fields are kept as raw `String`s, exactly as typed, so a half-typed value
//! ("1.", "") never gets reformatted under the user's cursor. The typed
//! accessors below are the single place that decides what is usable.

/// One row in the sequence builder.
#[derive(Clone)]
pub struct SeqFrame {
    pub frame_type: String,
    pub filter:     String,
    pub exposure:   String,
    pub count:      String,
    pub delay:      String,
    pub bin_x:      String,
    pub bin_y:      String,
    pub gain:       String,
    pub offset:     String,
    pub iso:        String,
    pub format:     String,
    pub encoding:   String,
    /// Flat rows only — KStars' calibration "Flat duration". `true` = ADU:
    /// KStars adapts the exposure until the frame mean reaches `flat_adu`
    /// ± `flat_tolerance`. `false` = Manual: the exposure is used as-is.
    pub flat_adu_mode:  bool,
    pub flat_adu:       String,
    pub flat_tolerance: String,
}

impl Default for SeqFrame {
    fn default() -> Self {
        Self {
            frame_type: "Light".into(),
            filter:     String::new(),
            exposure:   "120".into(),
            count:      "10".into(),
            delay:      "0".into(),
            bin_x:      "1".into(),
            bin_y:      "1".into(),
            gain:       "100".into(),
            offset:     String::new(),
            iso:        String::new(),
            format:     "FITS".into(),
            encoding:   "FITS".into(),
            flat_adu_mode:  false,
            flat_adu:       "20000".into(),
            // KStars' own default (kcfg `CalibrationADUValueTolerance`).
            flat_tolerance: "1000".into(),
        }
    }
}

impl SeqFrame {
    /// Exposure in seconds, if it is a finite number ≥ 0.
    pub fn exposure_secs(&self) -> Option<f64> {
        self.exposure.trim().parse::<f64>().ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    }

    /// Frame count, if it is a whole number ≥ 1.
    pub fn count_n(&self) -> Option<u32> {
        self.count.trim().parse::<u32>().ok().filter(|n| *n >= 1)
    }

    /// Total capture time of this row (exposure × count), ignoring delays.
    pub fn duration_secs(&self) -> Option<f64> {
        Some(self.exposure_secs()? * self.count_n()? as f64)
    }

    /// True when the row can be serialized into a job KStars will accept.
    pub fn is_valid(&self) -> bool {
        self.duration_secs().is_some()
            && (!self.is_adu_flat() || self.flat_adu_target().is_some())
    }

    /// Flat row with the ADU flat duration selected.
    pub fn is_adu_flat(&self) -> bool {
        self.frame_type == "Flat" && self.flat_adu_mode
    }

    /// `(target ADU, tolerance)` for an ADU flat whose values are usable, else
    /// `None`. KStars only runs the ADU calibration for a target > 0
    /// (`cameraprocess.cpp`), so a zero/unparsable target doesn't count.
    pub fn flat_adu_target(&self) -> Option<(f64, f64)> {
        if !self.is_adu_flat() { return None; }
        let adu = self.flat_adu.trim().parse::<f64>().ok()
            .filter(|v| *v > 0.0 && *v <= 65535.0)?;
        let tol = self.flat_tolerance.trim().parse::<f64>().ok()
            .filter(|v| *v >= 0.0)?;
        Some((adu, tol))
    }

    /// The editor offers square binning only, so one value sets both axes.
    pub fn set_bin(&mut self, n: u32) {
        self.bin_x = n.to_string();
        self.bin_y = n.to_string();
    }

    /// "2×2" — or "2×1" for asymmetric binning set outside the editor.
    pub fn bin_label(&self) -> String {
        let axis = |v: &str| if v.trim().is_empty() { "1".to_string() } else { v.trim().to_string() };
        format!("{}\u{00d7}{}", axis(&self.bin_x), axis(&self.bin_y))
    }
}

/// Human-readable duration: "45 s", "20 min", "1 h 12 min".
pub fn fmt_duration(secs: f64) -> String {
    if secs < 60.0 {
        // Sub-second exposures (bias, short flats) keep one decimal.
        if secs < 10.0 && secs.fract() != 0.0 {
            format!("{secs:.1} s")
        } else {
            format!("{secs:.0} s")
        }
    } else if secs < 3600.0 {
        format!("{:.0} min", (secs / 60.0).round())
    } else {
        let mins = (secs / 60.0).round() as u64;
        format!("{} h {:02} min", mins / 60, mins % 60)
    }
}
