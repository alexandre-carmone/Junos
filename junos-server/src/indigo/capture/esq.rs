//! KStars capture sequences (`.esq`) on the server: the files the browser's
//! sequence editor builds (`sequence_editor/esq.rs::build_esq_xml`) and the
//! ones KStars' own Capture module writes.
//!
//! Reader: `SequenceQueue::load` (`kstars/ekos/capture/sequencequeue.cpp`)
//! and `SequenceJob` (`sequencejob.cpp:721`) — the head elements are the
//! limits, **every other root child is a job**; legacy files keep their
//! `<Gain>` / `<Offset>` elements and a `<Prefix><RawPrefix>` target.
//! Writer: the browser's shape, so the Files › Planning viewer and KStars
//! both read it back.

use std::borrow::Cow;

use roxmltree::{Document, Node};

use super::{FrameType, Job, JobStatus, Limits, Queue};

/// KStars' `PlaceholderPath::defaultFormat(true, true, true)`.
pub const DEFAULT_PLACEHOLDER: &str = "/%t/%T/%F/%t_%T_%F_%e_%D";
pub const DEFAULT_SUFFIX: u32 = 3;

/// KStars writes paths unescaped, so a `&` in a folder name makes the XML
/// ill-formed; this repairs that before parsing (same as the browser's
/// `files/planning/parse.rs`).
pub fn escape_stray_amps(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let is_ref = |tail: &str| {
        let Some(end) = tail.find(';').filter(|&j| j > 0 && j <= 10) else { return false };
        let name = &tail[..end];
        match name.strip_prefix('#') {
            Some(n) => match n.strip_prefix(['x', 'X']) {
                Some(hex) => !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
                None => !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()),
            },
            None => matches!(name, "amp" | "lt" | "gt" | "quot" | "apos"),
        }
    };
    let mut out = String::with_capacity(s.len() + 16);
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        out.push_str(if is_ref(tail) { "&" } else { "&amp;" });
        rest = tail;
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Element text for `& < >` (quotes stay literal, as the browser writes).
pub fn esc(s: &str) -> Cow<'_, str> {
    if !s.contains(['&', '<', '>']) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"))
}

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(tag))
}

fn text(n: Node, tag: &str) -> String {
    child(n, tag).and_then(|c| c.text()).map(|t| t.trim().to_string()).unwrap_or_default()
}

fn num(n: Node, tag: &str) -> Option<f64> {
    text(n, tag).parse().ok()
}

fn enabled(n: Node, tag: &str) -> Option<bool> {
    child(n, tag).map(|c| c.attribute("enabled") == Some("true"))
}

/// `<Properties><PropertyVector name=…><OneElement name=…>v` lookup.
fn property(job: Node, vector: &str, element: &str) -> Option<f64> {
    child(job, "Properties")?
        .children()
        .filter(|v| v.has_tag_name("PropertyVector") && v.attribute("name") == Some(vector))
        .flat_map(|v| v.children())
        .find(|e| e.has_tag_name("OneElement") && e.attribute("name") == Some(element))
        .and_then(|e| e.text())
        .and_then(|t| t.trim().parse().ok())
}

const HEAD_TAGS: [&str; 11] = [
    "Observer", "GuideDeviation", "GuideStartDeviation", "HFRCheck", "RefocusOnTemperatureDelta",
    "RefocusEveryN", "RefocusOnMeridianFlip", "CCD", "FilterWheel", "Autofocus", "MeridianFlip",
];

/// Parse a sequence file. `target` overrides every job's `<TargetName>`
/// (KStars' `SequenceJob(…, targetName)` semantics).
pub fn parse(xml: &str) -> Result<Queue, String> {
    let xml = escape_stray_amps(xml);
    let doc = Document::parse(&xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    if !root.has_tag_name("SequenceQueue") {
        return Err(format!("not a sequence file (root is <{}>)", root.tag_name().name()));
    }
    if let Some(v) = root.attribute("version").and_then(|v| v.parse::<f64>().ok()) {
        if v < 2.0 {
            return Err(format!("deprecated sequence file format version {v}; create a new sequence file"));
        }
    }
    let mut limits = Limits::default();
    if let Some(on) = enabled(root, "GuideDeviation") {
        limits.guide_abort = on;
        limits.guide_abort_arcsec = num(root, "GuideDeviation").unwrap_or(limits.guide_abort_arcsec);
    }
    if let Some(on) = enabled(root, "GuideStartDeviation") {
        limits.guide_start = on;
        limits.guide_start_arcsec = num(root, "GuideStartDeviation").unwrap_or(limits.guide_start_arcsec);
    }
    if let Some(h) = child(root, "HFRCheck") {
        limits.hfr_check = h.attribute("enabled") == Some("true");
        limits.hfr_deviation = num(h, "HFRDeviation").filter(|v| *v >= 0.0).unwrap_or(0.0);
        limits.hfr_algorithm = num(h, "HFRCheckAlgorithm").map(|v| v as u8).filter(|v| *v <= 2).unwrap_or(0);
        limits.hfr_threshold = num(h, "HFRCheckThreshold").unwrap_or(10.0);
        limits.hfr_frames = num(h, "HFRCheckFrames").map(|v| v.max(1.0) as u32).unwrap_or(1);
    }
    if let Some(on) = enabled(root, "RefocusOnTemperatureDelta") {
        limits.refocus_temp = on;
        limits.refocus_temp_delta = num(root, "RefocusOnTemperatureDelta").unwrap_or(limits.refocus_temp_delta);
    }
    if let Some(on) = enabled(root, "RefocusEveryN") {
        limits.refocus_every = on;
        limits.refocus_every_min = num(root, "RefocusEveryN").map(|v| v.max(1.0) as u32).unwrap_or(limits.refocus_every_min);
    }
    if let Some(on) = enabled(root, "RefocusOnMeridianFlip") {
        limits.refocus_flip = on;
    }

    let mut jobs = Vec::new();
    for (i, j) in root.children().filter(|c| c.is_element() && !HEAD_TAGS.contains(&c.tag_name().name())).enumerate() {
        jobs.push(parse_job(j).map_err(|e| format!("job {}: {e}", i + 1))?);
    }
    Ok(Queue { jobs, limits, path: None })
}

fn parse_job(j: Node) -> Result<Job, String> {
    let exposure = num(j, "Exposure").ok_or("missing <Exposure>")?;
    if !(exposure > 0.0) {
        return Err(format!("exposure {exposure} is not positive"));
    }
    let count = num(j, "Count").map(|v| v.max(0.0) as u32).unwrap_or(1);
    let bin = child(j, "Binning")
        .map(|b| (num(b, "X").unwrap_or(1.0).max(1.0) as u32, num(b, "Y").unwrap_or(1.0).max(1.0) as u32))
        .unwrap_or((1, 1));
    // Standalone CCD_GAIN / CCD_OFFSET, else CCD_CONTROLS (ZWO and co.), else
    // the legacy elements.
    let gain = property(j, "CCD_GAIN", "GAIN")
        .or_else(|| property(j, "CCD_CONTROLS", "Gain"))
        .or_else(|| num(j, "Gain"));
    let offset = property(j, "CCD_OFFSET", "OFFSET")
        .or_else(|| property(j, "CCD_CONTROLS", "Offset"))
        .or_else(|| num(j, "Offset"));
    let temperature = child(j, "Temperature")
        .filter(|t| t.attribute("force") == Some("true"))
        .and_then(|t| t.text())
        .and_then(|t| t.trim().parse().ok());
    let mut target = text(j, "TargetName");
    let mut placeholder_format = text(j, "PlaceholderFormat");
    if let Some(prefix) = child(j, "Prefix") {
        // Outdated format: the target sat in the prefix, the file name
        // pattern was three flags.
        if target.is_empty() {
            target = text(prefix, "RawPrefix");
        }
        if placeholder_format.is_empty() {
            let flag = |tag: &str| text(prefix, tag) == "1";
            let mut f = String::from("/%t/%T/%F/%t_%T");
            if flag("FilterEnabled") { f.push_str("_%F"); }
            if flag("ExpEnabled") { f.push_str("_%e"); }
            if flag("TimeStampEnabled") { f.push_str("_%D"); }
            placeholder_format = f;
        }
    }
    if placeholder_format.is_empty() {
        placeholder_format = DEFAULT_PLACEHOLDER.to_string();
    }
    let filter = Some(text(j, "Filter")).filter(|f| !f.is_empty());
    let encoding = Some(text(j, "Encoding")).filter(|e| !e.is_empty()).unwrap_or_else(|| "FITS".into());
    Ok(Job {
        frame_type: FrameType::from_label(&text(j, "Type")),
        filter,
        exposure,
        count,
        delay_s: num(j, "Delay").unwrap_or(0.0).max(0.0),
        bin,
        gain,
        offset,
        iso_index: num(j, "ISOIndex").map(|v| v as u32),
        format: text(j, "Format"),
        encoding,
        temperature,
        target,
        fits_dir: text(j, "FITSDirectory"),
        placeholder_format,
        placeholder_suffix: num(j, "PlaceholderSuffix").map(|v| v as u32).unwrap_or(DEFAULT_SUFFIX),
        upload_mode: num(j, "UploadMode").map(|v| v as u8).unwrap_or(0),
        dither: num(j, "GuideDitherPerJob").map(|v| v as i32).unwrap_or(0),
        post_job_script: text(j, "PostJobScript"),
        completed: 0,
        status: JobStatus::Idle,
    })
}

/// Serialise a queue in the browser's shape (`build_esq_xml`).
pub fn write(q: &Queue) -> String {
    let flag = |on: bool| if on { "true" } else { "false" };
    let l = &q.limits;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<SequenceQueue version='2.1'>\n");
    xml.push_str(&format!(
        "<GuideDeviation enabled='{}'>{}</GuideDeviation>\n\
         <GuideStartDeviation enabled='{}'>{}</GuideStartDeviation>\n\
         <HFRCheck enabled='{}'><HFRDeviation>{}</HFRDeviation>\
         <HFRCheckAlgorithm>{}</HFRCheckAlgorithm><HFRCheckThreshold>{}</HFRCheckThreshold>\
         <HFRCheckFrames>{}</HFRCheckFrames></HFRCheck>\n\
         <RefocusOnTemperatureDelta enabled='{}'>{}</RefocusOnTemperatureDelta>\n\
         <RefocusEveryN enabled='{}'>{}</RefocusEveryN>\n\
         <RefocusOnMeridianFlip enabled='{}'/>\n",
        flag(l.guide_abort), l.guide_abort_arcsec,
        flag(l.guide_start), l.guide_start_arcsec,
        flag(l.hfr_check), l.hfr_deviation, l.hfr_algorithm, l.hfr_threshold, l.hfr_frames,
        flag(l.refocus_temp), l.refocus_temp_delta,
        flag(l.refocus_every), l.refocus_every_min,
        flag(l.refocus_flip),
    ));
    for job in &q.jobs {
        xml.push_str("<Job>\n");
        xml.push_str(&format!("<Exposure>{}</Exposure>\n", job.exposure));
        if !job.format.is_empty() {
            xml.push_str(&format!("<Format>{}</Format>\n", esc(&job.format)));
        }
        xml.push_str(&format!("<Encoding>{}</Encoding>\n", esc(&job.encoding)));
        xml.push_str(&format!("<Binning><X>{}</X><Y>{}</Y></Binning>\n", job.bin.0, job.bin.1));
        xml.push_str("<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>\n");
        if let Some(t) = job.temperature {
            xml.push_str(&format!("<Temperature force='true'>{t}</Temperature>\n"));
        }
        if let Some(f) = &job.filter {
            xml.push_str(&format!("<Filter>{}</Filter>\n", esc(f)));
        }
        xml.push_str(&format!("<Type>{}</Type>\n", job.frame_type.label()));
        xml.push_str(&format!("<Count>{}</Count>\n", job.count));
        xml.push_str(&format!("<Delay>{}</Delay>\n", job.delay_s));
        if !job.target.is_empty() {
            xml.push_str(&format!("<TargetName>{}</TargetName>\n", esc(&job.target)));
        }
        if !job.post_job_script.is_empty() {
            xml.push_str(&format!("<PostJobScript>{}</PostJobScript>\n", esc(&job.post_job_script)));
        }
        xml.push_str(&format!("<GuideDitherPerJob>{}</GuideDitherPerJob>\n", job.dither));
        xml.push_str(&format!("<FITSDirectory>{}</FITSDirectory>\n", esc(&job.fits_dir)));
        xml.push_str(&format!("<PlaceholderFormat>{}</PlaceholderFormat>\n", esc(&job.placeholder_format)));
        xml.push_str(&format!("<PlaceholderSuffix>{}</PlaceholderSuffix>\n", job.placeholder_suffix));
        xml.push_str(&format!("<UploadMode>{}</UploadMode>\n", job.upload_mode));
        if let Some(i) = job.iso_index {
            xml.push_str(&format!("<ISOIndex>{i}</ISOIndex>\n"));
        }
        if job.gain.is_some() || job.offset.is_some() {
            xml.push_str("<Properties>\n");
            if let Some(g) = job.gain {
                xml.push_str(&format!("<PropertyVector name='CCD_GAIN'><OneElement name='GAIN'>{g}</OneElement></PropertyVector>\n"));
            }
            if let Some(o) = job.offset {
                xml.push_str(&format!("<PropertyVector name='CCD_OFFSET'><OneElement name='OFFSET'>{o}</OneElement></PropertyVector>\n"));
            }
            xml.push_str("<PropertyVector name='CCD_CONTROLS'>");
            if let Some(g) = job.gain {
                xml.push_str(&format!("<OneElement name='Gain'>{g}</OneElement>"));
            }
            if let Some(o) = job.offset {
                xml.push_str(&format!("<OneElement name='Offset'>{o}</OneElement>"));
            }
            xml.push_str("</PropertyVector>\n</Properties>\n");
        } else {
            xml.push_str("<Properties/>\n");
        }
        xml.push_str(
            "<Calibration><FlatSource><Type>Manual</Type></FlatSource><FlatDuration><Type>Manual</Type></FlatDuration>\
             <PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>\n",
        );
        xml.push_str("</Job>\n");
    }
    xml.push_str("</SequenceQueue>\n");
    xml
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const BROWSER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<SequenceQueue version='2.1'>
<GuideDeviation enabled='false'>2</GuideDeviation>
<GuideStartDeviation enabled='false'>2</GuideStartDeviation>
<HFRCheck enabled='false'><HFRDeviation>0.5</HFRDeviation><HFRCheckAlgorithm>0</HFRCheckAlgorithm><HFRCheckThreshold>10</HFRCheckThreshold><HFRCheckFrames>1</HFRCheckFrames></HFRCheck>
<RefocusOnTemperatureDelta enabled='false'>1</RefocusOnTemperatureDelta>
<RefocusEveryN enabled='true'>45</RefocusEveryN>
<RefocusOnMeridianFlip enabled='false'/>
<Job>
<Exposure>120</Exposure>
<Format>FITS</Format>
<Encoding>FITS</Encoding>
<Binning><X>2</X><Y>2</Y></Binning>
<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>
<Filter>Ha</Filter>
<Type>Light</Type>
<Count>10</Count>
<Delay>5</Delay>
<TargetName>M 31</TargetName>
<GuideDitherPerJob>3</GuideDitherPerJob>
<FITSDirectory>/data/caps &amp; more</FITSDirectory>
<PlaceholderFormat>/%t/%t_%F_%T_%e_%D</PlaceholderFormat>
<PlaceholderSuffix>4</PlaceholderSuffix>
<UploadMode>0</UploadMode>
<Properties>
<PropertyVector name='CCD_GAIN'><OneElement name='GAIN'>120</OneElement></PropertyVector>
<PropertyVector name='CCD_CONTROLS'><OneElement name='Gain'>120</OneElement></PropertyVector>
</Properties>
<Calibration><FlatSource><Type>Manual</Type></FlatSource><FlatDuration><Type>Manual</Type></FlatDuration><PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>
</Job>
<Job>
<Exposure>1.5</Exposure>
<Encoding>FITS</Encoding>
<Binning><X>1</X><Y>1</Y></Binning>
<Frame><X>0</X><Y>0</Y><W>0</W><H>0</H></Frame>
<Type>Dark</Type>
<Count>2</Count>
<Delay>0</Delay>
<PostJobScript>/home/x/after.sh</PostJobScript>
<GuideDitherPerJob>-1</GuideDitherPerJob>
<FITSDirectory></FITSDirectory>
<PlaceholderFormat>/%T/%F/%T_%F_%e</PlaceholderFormat>
<PlaceholderSuffix>4</PlaceholderSuffix>
<UploadMode>0</UploadMode>
<Properties/>
<Calibration><FlatSource><Type>Manual</Type></FlatSource><FlatDuration><Type>Manual</Type></FlatDuration><PreMountPark>false</PreMountPark><PreDomePark>false</PreDomePark></Calibration>
</Job>
</SequenceQueue>
"#;

    #[test]
    fn parses_the_browsers_file() {
        let q = parse(BROWSER_XML).unwrap();
        assert_eq!(q.jobs.len(), 2);
        let j = &q.jobs[0];
        assert_eq!(j.frame_type, FrameType::Light);
        assert_eq!(j.filter.as_deref(), Some("Ha"));
        assert_eq!(j.exposure, 120.0);
        assert_eq!(j.count, 10);
        assert_eq!(j.delay_s, 5.0);
        assert_eq!(j.bin, (2, 2));
        assert_eq!(j.gain, Some(120.0));
        assert_eq!(j.offset, None);
        assert_eq!(j.target, "M 31");
        assert_eq!(j.fits_dir, "/data/caps & more");
        assert_eq!(j.dither, 3);
        assert_eq!(j.placeholder_suffix, 4);
        let d = &q.jobs[1];
        assert_eq!(d.frame_type, FrameType::Dark);
        assert_eq!(d.filter, None);
        assert_eq!(d.post_job_script, "/home/x/after.sh");
        assert_eq!(d.dither, -1);
        assert!(q.limits.refocus_every && q.limits.refocus_every_min == 45);
        assert!(!q.limits.hfr_check && q.limits.hfr_deviation == 0.5);
    }

    #[test]
    fn round_trip() {
        let q = parse(BROWSER_XML).unwrap();
        let again = parse(&write(&q)).unwrap();
        assert_eq!(again.jobs, q.jobs);
        assert_eq!(again.limits, q.limits);
    }

    #[test]
    fn legacy_kstars_file_and_version_check() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<SequenceQueue version='2.0'>
<Observer>Me</Observer>
<CCD>Sim</CCD>
<FilterWheel>Wheel</FilterWheel>
<GuideDeviation enabled='true'>1.5</GuideDeviation>
<Job>
<Exposure>30</Exposure>
<Gain>50</Gain>
<Offset>10</Offset>
<Temperature force='true'>-10</Temperature>
<Prefix><RawPrefix>NGC 7000</RawPrefix><FilterEnabled>1</FilterEnabled><ExpEnabled>0</ExpEnabled><TimeStampEnabled>1</TimeStampEnabled></Prefix>
<Type>Flat</Type>
<Count>5</Count>
<Delay>0</Delay>
<FITSDirectory>/x/a & b</FITSDirectory>
</Job>
</SequenceQueue>"#;
        let q = parse(xml).unwrap();
        assert_eq!(q.jobs.len(), 1);
        let j = &q.jobs[0];
        assert_eq!(j.gain, Some(50.0));
        assert_eq!(j.offset, Some(10.0));
        assert_eq!(j.temperature, Some(-10.0));
        assert_eq!(j.target, "NGC 7000");
        assert_eq!(j.placeholder_format, "/%t/%T/%F/%t_%T_%F_%D");
        assert_eq!(j.frame_type, FrameType::Flat);
        assert_eq!(j.fits_dir, "/x/a & b");
        assert_eq!(j.placeholder_suffix, DEFAULT_SUFFIX);
        assert!(q.limits.guide_abort && q.limits.guide_abort_arcsec == 1.5);
        // Everything else keeps the defaults.
        assert!(!q.limits.refocus_every && q.limits.refocus_every_min == 60);

        let old = xml.replace("version='2.0'", "version='1.9'");
        assert!(parse(&old).unwrap_err().contains("deprecated"));
        assert!(parse("<Nope/>").unwrap_err().contains("not a sequence"));
        let bad = xml.replace("<Exposure>30</Exposure>", "<Exposure>0</Exposure>");
        assert!(parse(&bad).unwrap_err().contains("job 1"));
    }
}
