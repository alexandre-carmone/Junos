//! `SeqFrame` — one job row of the sequence editor — plus the validation and
//! duration helpers shared by the editor UI and its three callers.
//!
//! Fields are kept as raw `String`s, exactly as typed, so a half-typed value
//! ("1.", "") never gets reformatted under the user's cursor. The typed
//! accessors below are the single place that decides what is usable.

/// How a Light row dithers — `<GuideDitherPerJob>` (sequencejob.cpp). The
/// Guide module's "Enable dithering" stays the master switch either way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Dither {
    /// 0: the Guide module's "dither every N frames".
    #[default]
    Guide,
    /// N > 0: every `dither_every` frames for this row.
    Every,
    /// -1: never for this row.
    Off,
}

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
    /// ISO label as the camera lists it (`CCD_ISO`); serialized as its index.
    pub iso:        String,
    /// `CCD_CAPTURE_FORMAT` label ("Mono", "Raw 16 bit"…) — empty keeps the
    /// camera's current one.
    pub format:     String,
    /// `CCD_TRANSFER_FORMAT` label: "FITS", "XISF" or "Native".
    pub encoding:   String,
    /// Flat rows only — KStars' calibration "Flat duration". `true` = ADU:
    /// KStars adapts the exposure until the frame mean reaches `flat_adu`
    /// ± `flat_tolerance`. `false` = Manual: the exposure is used as-is.
    pub flat_adu_mode:  bool,
    pub flat_adu:       String,
    pub flat_tolerance: String,
    /// Light rows only — KStars dithers nothing else (`checkDithering`).
    pub dither:       Dither,
    pub dither_every: String,
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
            format:     String::new(),
            encoding:   "FITS".into(),
            flat_adu_mode:  false,
            flat_adu:       "20000".into(),
            // KStars' own default (kcfg `CalibrationADUValueTolerance`).
            flat_tolerance: "1000".into(),
            dither:       Dither::Guide,
            dither_every: "3".into(),
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

    /// Gain, `Ok(None)` when left empty (the camera keeps its own).
    pub fn gain_value(&self) -> Result<Option<f64>, ()> {
        optional_number(&self.gain)
    }

    /// Offset, `Ok(None)` when left empty (the camera keeps its own).
    pub fn offset_value(&self) -> Result<Option<f64>, ()> {
        optional_number(&self.offset)
    }

    /// Delay between frames in whole seconds — KStars reads `<Delay>` with
    /// `toInt`, so "2.5" would silently become 0. Empty means none.
    pub fn delay_secs(&self) -> Result<u32, ()> {
        Ok(optional_number(&self.delay)?.map_or(0, |v| v.round() as u32))
    }

    /// Binning factor of one axis; empty means 1.
    pub fn bin_n(axis: &str) -> Result<u32, ()> {
        match axis.trim() {
            "" => Ok(1),
            v => v.parse::<u32>().ok().filter(|n| *n >= 1).ok_or(()),
        }
    }

    /// `<GuideDitherPerJob>`: 0 (the Guide module's frequency), N or -1.
    /// Rows other than Light never dither, so they always follow the Guide
    /// module, whatever was picked while they were Light.
    pub fn dither_per_job(&self) -> Result<i32, ()> {
        if self.frame_type != "Light" {
            return Ok(0);
        }
        match self.dither {
            Dither::Guide => Ok(0),
            Dither::Off => Ok(-1),
            Dither::Every => self.dither_every.trim().parse::<i32>().ok().filter(|n| *n >= 1).ok_or(()),
        }
    }

    /// Gain, offset, delay, binning and dither are numbers KStars can read.
    /// Exposure and count are checked by `duration_secs`.
    pub fn values_ok(&self) -> bool {
        self.gain_value().is_ok() && self.offset_value().is_ok() && self.delay_secs().is_ok()
            && Self::bin_n(&self.bin_x).is_ok() && Self::bin_n(&self.bin_y).is_ok()
            && self.dither_per_job().is_ok()
    }

    /// True when the row can be serialized into a job KStars will accept.
    pub fn is_valid(&self) -> bool {
        self.duration_secs().is_some()
            && self.values_ok()
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

/// A number ≥ 0, `Ok(None)` for an empty field, `Err` for anything else —
/// KStars would read a stray string as 0 (`QVariant::toDouble`).
fn optional_number(s: &str) -> Result<Option<f64>, ()> {
    match s.trim() {
        "" => Ok(None),
        v => v.parse::<f64>().ok().filter(|n| n.is_finite() && *n >= 0.0).map(Some).ok_or(()),
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
