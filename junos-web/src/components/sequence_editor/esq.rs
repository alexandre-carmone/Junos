//! ESQ (KStars capture sequence) XML serializer for `SeqFrame` rows.
//!
//! Callers feed the result into KStars via `capture_load_sequence_file`
//! (Imaging) or `scheduler_save_sequence_file` (Scheduler, Mosaic).

use std::borrow::Cow;

use crate::compat::CameraSnapshot;

use super::limits::SeqLimits;
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

/// One `<PropertyVector>` with a single number element.
fn vector(xml: &mut String, name: &str, element: &str, value: f64) {
    xml.push_str(&format!(
        "<PropertyVector name='{name}'><OneElement name='{element}'>{value}</OneElement></PropertyVector>\n"));
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
///
/// `limits` is the sequence-wide head (refocus, HFR check, guide drift),
/// which KStars applies to its global options on load. `post_script` is run
/// by KStars after the last frame (`SeqEnd::post_job_script`).
///
/// `camera` resolves what the rows leave to the camera (an unset format) and
/// what KStars wants as an index (ISO). Numbers are written as parsed, so a
/// stray space or decimal can't turn into 0 on KStars' side.
pub fn build_esq_xml(
    job_name: &str,
    fits_dir: &str,
    frames: &[SeqFrame],
    limits: &SeqLimits,
    post_script: Option<&str>,
    target_folder: bool,
    camera: &CameraSnapshot,
) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<SequenceQueue version='2.1'>\n");
    xml.push_str(&limits.esq_xml());
    let job_name = esc(job_name);
    let fits_dir = esc(fits_dir);
    for (i, f) in frames.iter().enumerate() {
        xml.push_str("<Job>\n");
        match f.exposure_secs() {
            Some(secs) => xml.push_str(&format!("<Exposure>{secs}</Exposure>\n")),
            None => xml.push_str(&format!("<Exposure>{}</Exposure>\n", esc(&f.exposure))),
        }
        // <Format> is a CCD_CAPTURE_FORMAT label, which KStars sets on every
        // frame (`setCaptureFormat` turns all switches off first, so a label
        // the camera doesn't list leaves none on). Unset → the camera's
        // current one; unknown too → no element, KStars leaves it alone.
        let format = Some(f.format.trim()).filter(|v| !v.is_empty())
            .or(camera.capture_format.as_deref());
        if let Some(format) = format {
            xml.push_str(&format!("<Format>{}</Format>\n", esc(format)));
        }
        xml.push_str(&format!("<Encoding>{}</Encoding>\n", or(f.encoding.trim(), "FITS")));
        let bin = |axis: &str| SeqFrame::bin_n(axis).unwrap_or(1);
        xml.push_str(&format!("<Binning><X>{}</X><Y>{}</Y></Binning>\n", bin(&f.bin_x), bin(&f.bin_y)));
        xml.push_str("<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>\n");
        // KStars looks the name up in the filter wheel's labels (`<Filter>`
        // absent: the wheel stays where it is).
        if !f.filter.is_empty() {
            xml.push_str(&format!("<Filter>{}</Filter>\n", esc(&f.filter)));
        }
        xml.push_str(&format!("<Type>{}</Type>\n", esc(&f.frame_type)));
        match f.count_n() {
            Some(n) => xml.push_str(&format!("<Count>{n}</Count>\n")),
            None => xml.push_str(&format!("<Count>{}</Count>\n", esc(&f.count))),
        }
        xml.push_str(&format!("<Delay>{}</Delay>\n", f.delay_secs().unwrap_or(0)));
        if !job_name.is_empty() {
            xml.push_str(&format!("<TargetName>{job_name}</TargetName>\n"));
        }
        if let Some(script) = post_script.filter(|_| i + 1 == frames.len()) {
            xml.push_str(&format!("<PostJobScript>{}</PostJobScript>\n", esc(script)));
        }
        // 0 follows the Guide module's DitherFrames, N dithers every N frames,
        // -1 never (camerastate.cpp checkDithering / resetDitherCounter).
        xml.push_str(&format!("<GuideDitherPerJob>{}</GuideDitherPerJob>\n", f.dither_per_job().unwrap_or(0)));
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
        // <ISOIndex> is the position in the camera's CCD_ISO list
        // (`CameraChip::setISOIndex`), not the ISO itself.
        if let Some(i) = camera.iso_options.iter().position(|o| *o == f.iso) {
            xml.push_str(&format!("<ISOIndex>{i}</ISOIndex>\n"));
        }
        // Gain and offset live in a standalone CCD_GAIN / CCD_OFFSET on some
        // drivers, in CCD_CONTROLS ("Gain" / "Offset") on others (ZWO…), and
        // KStars only reads the one the camera has (`cameraGain(propertyMap)`).
        // Write both: the vector the camera lacks is skipped when the job
        // applies its properties.
        let gain = f.gain_value().ok().flatten();
        let offset = f.offset_value().ok().flatten();
        if gain.is_some() || offset.is_some() {
            xml.push_str("<Properties>\n");
            if let Some(g) = gain {
                vector(&mut xml, "CCD_GAIN", "GAIN", g);
            }
            if let Some(o) = offset {
                vector(&mut xml, "CCD_OFFSET", "OFFSET", o);
            }
            xml.push_str("<PropertyVector name='CCD_CONTROLS'>");
            if let Some(g) = gain {
                xml.push_str(&format!("<OneElement name='Gain'>{g}</OneElement>"));
            }
            if let Some(o) = offset {
                xml.push_str(&format!("<OneElement name='Offset'>{o}</OneElement>"));
            }
            xml.push_str("</PropertyVector>\n</Properties>\n");
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
