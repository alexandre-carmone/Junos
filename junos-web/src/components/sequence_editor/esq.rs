//! ESQ (KStars capture sequence) XML serializer for `SeqFrame` rows.
//!
//! Callers feed the result into KStars via `capture_load_sequence_file`
//! (Imaging), `scheduler_save_sequence_file` (Scheduler) or
//! `scheduler_import_mosaic` (Mosaic).

use std::borrow::Cow;

use super::model::SeqFrame;

/// Escape text-node content, so a folder path like `/data/M81 & M82` or an
/// odd filter name can't break the document. Every value below lands in
/// element text (never an attribute), where only `&`, `<` and `>` matter —
/// quotes stay literal, exactly as before.
fn esc(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
}

/// `value`, or `fallback` when the field was left empty.
fn or<'a>(value: &'a str, fallback: &'a str) -> Cow<'a, str> {
    esc(if value.is_empty() { fallback } else { value })
}

/// Generate a minimal ESQ XML from a list of sequence frames.
///
/// `fits_dir` is written into every job's `<FITSDirectory>` (KStars parses it
/// into `SJ_LocalDirectory`, then combines it with the placeholder path). When
/// empty, the element is left empty so KStars keeps its own default location.
/// `target_folder` controls whether the capture path derives its per-target
/// subfolder from the `%t` (target name) placeholder. The scheduler flow bakes
/// the sanitized name straight into `fits_dir` instead, so it passes `false` to
/// drop the `%t` folder and avoid a doubled-up subfolder.
pub fn build_esq_xml(job_name: &str, fits_dir: &str, frames: &[SeqFrame], target_folder: bool) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<SequenceQueue version='2.1'>\n");
    xml.push_str("<GuideDeviation enabled='false'>0</GuideDeviation>\n");
    xml.push_str("<GuideStartDeviation enabled='false'>0</GuideStartDeviation>\n");
    xml.push_str("<HFRCheck enabled='false'><HFRDeviation>0.1</HFRDeviation>\
<HFRCheckAlgorithm>0</HFRCheckAlgorithm><HFRCheckThreshold>0</HFRCheckThreshold>\
<HFRCheckFrames>1</HFRCheckFrames></HFRCheck>\n");
    xml.push_str("<RefocusOnTemperatureDelta enabled='false'>1</RefocusOnTemperatureDelta>\n");
    xml.push_str("<RefocusEveryN enabled='false'>60</RefocusEveryN>\n");
    xml.push_str("<RefocusOnMeridianFlip enabled='false'/>\n");
    let job_name = esc(job_name);
    let fits_dir = esc(fits_dir);
    for f in frames {
        xml.push_str("<Job>\n");
        xml.push_str(&format!("<Exposure>{}</Exposure>\n", esc(&f.exposure)));
        xml.push_str(&format!("<Format>{}</Format>\n<Encoding>{}</Encoding>\n",
            or(&f.format, "FITS"), or(&f.encoding, "FITS")));
        xml.push_str(&format!("<Binning><X>{}</X><Y>{}</Y></Binning>\n",
            or(&f.bin_x, "1"), or(&f.bin_y, "1")));
        xml.push_str("<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>\n");
        if !f.filter.is_empty() {
            xml.push_str(&format!("<Filter>{}</Filter>\n", esc(&f.filter)));
        }
        xml.push_str(&format!("<Type>{}</Type>\n", esc(&f.frame_type)));
        xml.push_str(&format!("<Count>{}</Count>\n", esc(&f.count)));
        xml.push_str(&format!("<Delay>{}</Delay>\n", or(&f.delay, "0")));
        if !job_name.is_empty() {
            xml.push_str(&format!("<TargetName>{job_name}</TargetName>\n"));
        }
        xml.push_str("<GuideDitherPerJob>-1</GuideDitherPerJob>\n");
        xml.push_str(&format!("<FITSDirectory>{fits_dir}</FITSDirectory>\n"));
        // %t = target name (per-tile job name for mosaics), %F = Filter,
        // %T = frame Type, %e = exposure (adds "_secs"), %D = datetime.
        // The target folder must use %t, NOT %T — %T expands to the frame type
        // (e.g. "Light"), so it never carries the target/mosaic name. Filename
        // pattern: target_filter_type_exposure_date.
        // Without a target folder: Type/Filter/type_filter_exposure, KStars'
        // own ordering — flats land in `Flat/<filter>/`, darks/bias (no filter,
        // so `%F` collapses) in `Dark/` / `Bias/`. Don't add `_secs` (%e
        // already does) nor printf-style `%04d` (not a KStars tag, kept
        // literally); the frame counter comes from <PlaceholderSuffix>.
        let placeholder = if target_folder {
            "/%t/%t_%F_%T_%e_%D"
        } else {
            "/%T/%F/%T_%F_%e"
        };
        xml.push_str(&format!("<PlaceholderFormat>{placeholder}</PlaceholderFormat>\n"));
        // KStars auto-appends a `_%s<suffix>` frame counter to every filename;
        // the suffix value is its zero-padding width. Use 4 → `_0001`, `_0002`, …
        xml.push_str("<PlaceholderSuffix>4</PlaceholderSuffix>\n");
        xml.push_str("<UploadMode>0</UploadMode>\n");
        if !f.iso.is_empty() {
            xml.push_str(&format!("<ISOIndex>{}</ISOIndex>\n", esc(&f.iso)));
        }
        let has_gain = !f.gain.is_empty();
        let has_offset = !f.offset.is_empty();
        if has_gain || has_offset {
            xml.push_str("<Properties>\n");
            if has_gain {
                xml.push_str(&format!(
                    "<PropertyVector name='CCD_GAIN'><OneElement name='GAIN'>{}</OneElement></PropertyVector>\n",
                    esc(&f.gain)));
            }
            if has_offset {
                xml.push_str(&format!(
                    "<PropertyVector name='CCD_OFFSET'><OneElement name='OFFSET'>{}</OneElement></PropertyVector>\n",
                    esc(&f.offset)));
            }
            xml.push_str("</Properties>\n");
        } else {
            xml.push_str("<Properties/>\n");
        }
        // KStars switches a job to ADU flat duration as soon as a <Value>
        // element is present (sequencejob.cpp), so only ADU flats carry one;
        // every other row stays Manual.
        let flat_duration = match f.flat_adu_target() {
            Some((adu, tol)) => format!(
                "<FlatDuration><Type>ADU</Type><Value>{adu}</Value><Tolerance>{tol}</Tolerance></FlatDuration>"),
            None => "<FlatDuration><Type>Manual</Type></FlatDuration>".to_string(),
        };
        xml.push_str(&format!("<Calibration><FlatSource><Type>Manual</Type></FlatSource>\
{flat_duration}<PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>\n"));
        xml.push_str("</Job>\n");
    }
    xml.push_str("</SequenceQueue>\n");
    xml
}
