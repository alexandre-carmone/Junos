//! The `.esl` schedule file: written byte for byte like
//! `SchedulerProcess::saveScheduler` (`schedulerprocess.cpp:2977`), one
//! element per line, and read back after the frontend's
//! `components/files/planning/parse.rs` — KStars' own files included, stray
//! `&` and all.

use std::borrow::Cow;
use std::fmt::Write as _;

use roxmltree::{Document, Node};

use super::job::{iso_local, parse_local_time, Completion, SchedulerJob, Settings, StartCond, Steps};

/// What a file says about the module; `None` where it says nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileSettings {
    pub reschedule_errors: Option<bool>,
    pub error_delay_secs: Option<u32>,
    pub startup_enabled: Option<bool>,
    pub shutdown_enabled: Option<bool>,
    pub pre_startup: Option<String>,
    pub post_startup: Option<String>,
    pub pre_shutdown: Option<String>,
    pub post_shutdown: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub jobs: Vec<SchedulerJob>,
    pub settings: FileSettings,
    /// What was skipped, for the log.
    pub warnings: Vec<String>,
}

// ── write ────────────────────────────────────────────────────────────────────

/// KStars' `entityXML`: the five XML entities, in text and attributes alike.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

pub fn write(jobs: &[SchedulerJob], settings: &Settings, profile: &str) -> String {
    let mut out = String::new();
    // Every `outstream << … << Qt::endl`; `{}` of an f64 is `QLocale::c().toString`.
    macro_rules! line {
        ($($arg:tt)*) => {{ let _ = writeln!(out, $($arg)*); }};
    }
    line!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
    line!("<SchedulerList version='2.2'>");
    line!("<Profile>{}</Profile>", esc(profile));
    for job in jobs {
        line!("<Job>");
        line!("<JobType lead='true'/>");
        line!("<Name>{}</Name>", esc(&job.name));
        line!("<Group>{}</Group>", esc(&job.group));
        line!("<Coordinates>");
        line!("<J2000RA>{}</J2000RA>", job.ra0_h);
        line!("<J2000DE>{}</J2000DE>", job.dec0_deg);
        line!("</Coordinates>");
        line!("<PositionAngle>{}</PositionAngle>", job.pa);
        line!("<Sequence>{}</Sequence>", esc(&job.sequence.to_string_lossy()));
        line!("<StartupCondition>");
        match &job.start {
            StartCond::Asap => line!("<Condition>ASAP</Condition>"),
            StartCond::At(t) => line!("<Condition value='{}'>At</Condition>", iso_local(t)),
        }
        line!("</StartupCondition>");
        line!("<Constraints>");
        if let Some(v) = job.min_alt {
            line!("<Constraint value='{v}'>MinimumAltitude</Constraint>");
        }
        if let Some(v) = job.min_moon_sep {
            line!("<Constraint value='{v}'>MoonSeparation</Constraint>");
        }
        if let Some(v) = job.max_moon_alt {
            line!("<Constraint value='{v}'>MoonMaxAltitude</Constraint>");
        }
        if job.twilight {
            line!("<Constraint>EnforceTwilight</Constraint>");
        }
        if job.horizon {
            line!("<Constraint>EnforceArtificialHorizon</Constraint>");
        }
        line!("</Constraints>");
        line!("<CompletionCondition>");
        match &job.completion {
            Completion::Sequence => line!("<Condition>Sequence</Condition>"),
            Completion::Repeat(n) => line!("<Condition value='{n}'>Repeat</Condition>"),
            Completion::Loop => line!("<Condition>Loop</Condition>"),
            Completion::At(t) => line!("<Condition value='{}'>At</Condition>", iso_local(t)),
        }
        line!("</CompletionCondition>");
        line!("<Steps>");
        for (on, name) in [
            (job.steps.track, "Track"),
            (job.steps.focus, "Focus"),
            (job.steps.align, "Align"),
            (job.steps.guide, "Guide"),
        ] {
            if on {
                line!("<Step>{name}</Step>");
            }
        }
        line!("</Steps>");
        line!("</Job>");
    }
    line!("<SchedulerAlgorithm value='1'/>");
    line!("<ErrorHandlingStrategy value='0'>");
    if settings.reschedule_errors {
        line!("<RescheduleErrors />");
    }
    line!("<delay>{}</delay>", settings.error_delay_secs);
    line!("</ErrorHandlingStrategy>");
    line!("<StartupProcedure enabled='{}'>", settings.startup_enabled);
    if !settings.pre_startup.is_empty() {
        line!("<PreStartupQueue>{}</PreStartupQueue>", esc(&settings.pre_startup));
    }
    if !settings.post_startup.is_empty() {
        line!("<PostStartupQueue>{}</PostStartupQueue>", esc(&settings.post_startup));
    }
    line!("</StartupProcedure>");
    line!("<ShutdownProcedure enabled='{}'>", settings.shutdown_enabled);
    if !settings.pre_shutdown.is_empty() {
        line!("<PreShutdownQueue>{}</PreShutdownQueue>", esc(&settings.pre_shutdown));
    }
    if !settings.post_shutdown.is_empty() {
        line!("<PostShutdownQueue>{}</PostShutdownQueue>", esc(&settings.post_shutdown));
    }
    line!("</ShutdownProcedure>");
    line!("</SchedulerList>");
    out
}

// ── read ─────────────────────────────────────────────────────────────────────

/// `&` that doesn't start one of XML's five entities or a character
/// reference becomes `&amp;` — KStars writes `<Sequence>` paths unescaped.
fn escape_stray_amps(s: &str) -> Cow<'_, str> {
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

fn child<'a, 'i>(n: Node<'a, 'i>, tag: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| c.has_tag_name(tag))
}

fn children<'a, 'i: 'a>(n: Node<'a, 'i>, tag: &'a str) -> impl Iterator<Item = Node<'a, 'i>> + 'a {
    n.children().filter(move |c| c.has_tag_name(tag))
}

/// The trimmed text of `n`'s first `<tag>`, "" if absent.
fn text(n: Node, tag: &str) -> String {
    child(n, tag).map(own_text).unwrap_or_default()
}

fn own_text(n: Node) -> String {
    n.text().map(str::trim).unwrap_or_default().to_string()
}

fn num(n: Node, tag: &str) -> Option<f64> {
    text(n, tag).parse().ok()
}

/// `(name, value)` of the `<Condition value='…'>name</Condition>` under `n`.
fn condition(n: Option<Node>) -> Option<(String, String)> {
    let c = n.and_then(|n| child(n, "Condition"))?;
    Some((own_text(c), c.attribute("value").unwrap_or_default().to_string()))
}

fn root_named<'a, 'i>(doc: &'a Document<'i>, tag: &str) -> Result<Node<'a, 'i>, String> {
    let root = doc.root_element();
    if root.has_tag_name(tag) {
        Ok(root)
    } else {
        Err(format!("<{}> where <{tag}> was expected", root.tag_name().name()))
    }
}

/// One `<Job>`; `Err` is the reason it's skipped.
fn job(j: Node, warnings: &mut Vec<String>) -> Result<SchedulerJob, String> {
    let name = text(j, "Name");
    if child(j, "JobType").and_then(|t| t.attribute("lead")) == Some("false") {
        return Err(format!("follower job \"{name}\" skipped"));
    }
    let coords = child(j, "Coordinates");
    let (Some(ra0_h), Some(dec0_deg)) = (coords.and_then(|c| num(c, "J2000RA")), coords.and_then(|c| num(c, "J2000DE")))
    else {
        return Err(if child(j, "FITS").is_some() {
            format!("job \"{name}\" targets a FITS file, skipped")
        } else {
            format!("job \"{name}\" has no coordinates, skipped")
        });
    };
    let mut job = SchedulerJob::new(name, ra0_h, dec0_deg, text(j, "Sequence"));
    job.group = text(j, "Group");
    job.pa = num(j, "PositionAngle").unwrap_or(0.0);

    let at = |value: &str, what: &str, warnings: &mut Vec<String>| {
        let t = parse_local_time(value);
        if t.is_none() {
            warnings.push(format!("job \"{}\": invalid {what} time \"{value}\"", job.name));
        }
        t
    };
    job.start = match condition(child(j, "StartupCondition")) {
        Some((c, v)) if c == "At" => at(&v, "startup", warnings).map_or(StartCond::Asap, StartCond::At),
        _ => StartCond::Asap,
    };
    let completion = match condition(child(j, "CompletionCondition")) {
        Some((c, v)) if c == "Repeat" => Completion::Repeat(v.parse::<f64>().map_or(1, |n| n.max(1.0) as u32)),
        Some((c, _)) if c == "Loop" => Completion::Loop,
        Some((c, v)) if c == "At" => at(&v, "completion", warnings).map_or(Completion::Sequence, Completion::At),
        _ => Completion::Sequence,
    };
    job.set_completion(completion);

    if let Some(cs) = child(j, "Constraints") {
        for c in children(cs, "Constraint") {
            let value = c.attribute("value").and_then(|v| v.parse().ok());
            match own_text(c).as_str() {
                "MinimumAltitude" => job.min_alt = value,
                "MoonSeparation" => job.min_moon_sep = value,
                "MoonMaxAltitude" => job.max_moon_alt = value,
                "EnforceTwilight" => job.twilight = true,
                "EnforceArtificialHorizon" => job.horizon = true,
                _ => {}
            }
        }
    }
    if let Some(steps) = child(j, "Steps") {
        let mut s = Steps::default();
        for step in children(steps, "Step") {
            match own_text(step).as_str() {
                "Track" => s.track = true,
                "Focus" => s.focus = true,
                "Align" => s.align = true,
                "Guide" => s.guide = true,
                _ => {}
            }
        }
        job.steps = s;
    }
    Ok(job)
}

pub fn read(xml: &str) -> Result<Loaded, String> {
    let xml = escape_stray_amps(xml);
    let doc = Document::parse(&xml).map_err(|e| e.to_string())?;
    let root = root_named(&doc, "SchedulerList")?;

    let mut warnings = Vec::new();
    let mut jobs = Vec::new();
    for j in children(root, "Job") {
        match job(j, &mut warnings) {
            Ok(job) => jobs.push(job),
            Err(why) => warnings.push(why),
        }
    }

    let mut settings = FileSettings::default();
    if let Some(e) = child(root, "ErrorHandlingStrategy") {
        settings.reschedule_errors = Some(child(e, "RescheduleErrors").is_some());
        settings.error_delay_secs = num(e, "delay").map(|d| d.max(0.0) as u32);
    }
    // KStars 3.8+ names a task queue per slot; older files a startup /
    // shutdown script.
    let path = |p: Node, tags: &[&str]| tags.iter().map(|t| text(p, t)).find(|v| !v.is_empty());
    if let Some(p) = child(root, "StartupProcedure") {
        settings.startup_enabled = Some(p.attribute("enabled") == Some("true"));
        settings.pre_startup = path(p, &["PreStartupQueue", "StartupScript"]);
        settings.post_startup = path(p, &["PostStartupQueue"]);
    }
    if let Some(p) = child(root, "ShutdownProcedure") {
        settings.shutdown_enabled = Some(p.attribute("enabled") == Some("true"));
        settings.pre_shutdown = path(p, &["PreShutdownQueue"]);
        settings.post_shutdown = path(p, &["PostShutdownQueue", "ShutdownScript"]);
    }
    Ok(Loaded { jobs, settings, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs() -> Vec<SchedulerJob> {
        let mut a = SchedulerJob::new("M 31", 0.7123, 41.269, "/home/x/.junos-sequences/M_31.esq");
        a.min_alt = Some(30.0);
        a.twilight = true;
        a.steps.track = true;
        a.set_completion(Completion::Repeat(3));
        let mut b = SchedulerJob::new("NGC 7000 & co", 20.98, 44.33, "/home/x/.junos-sequences/NGC_7000.esq");
        b.group = "nebulae".into();
        b.pa = 90.5;
        b.start = StartCond::At(parse_local_time("2025-07-15T22:30:00").unwrap());
        b.set_completion(Completion::At(parse_local_time("2025-07-16T03:00:00").unwrap()));
        b.min_moon_sep = Some(45.0);
        b.max_moon_alt = Some(20.0);
        b.horizon = true;
        b.steps = Steps { track: true, focus: true, align: true, guide: true };
        vec![a, b]
    }

    #[test]
    fn layout_matches_kstars() {
        let settings = Settings { reschedule_errors: true, startup_enabled: true, pre_startup: "/q/pre.json".into(), ..Settings::default() };
        let xml = write(&jobs(), &settings, "Simulators");
        let expected = "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<SchedulerList version='2.2'>
<Profile>Simulators</Profile>
<Job>
<JobType lead='true'/>
<Name>M 31</Name>
<Group></Group>
<Coordinates>
<J2000RA>0.7123</J2000RA>
<J2000DE>41.269</J2000DE>
</Coordinates>
<PositionAngle>0</PositionAngle>
<Sequence>/home/x/.junos-sequences/M_31.esq</Sequence>
<StartupCondition>
<Condition>ASAP</Condition>
</StartupCondition>
<Constraints>
<Constraint value='30'>MinimumAltitude</Constraint>
<Constraint>EnforceTwilight</Constraint>
</Constraints>
<CompletionCondition>
<Condition value='3'>Repeat</Condition>
</CompletionCondition>
<Steps>
<Step>Track</Step>
</Steps>
</Job>
<Job>
<JobType lead='true'/>
<Name>NGC 7000 &amp; co</Name>
<Group>nebulae</Group>
<Coordinates>
<J2000RA>20.98</J2000RA>
<J2000DE>44.33</J2000DE>
</Coordinates>
<PositionAngle>90.5</PositionAngle>
<Sequence>/home/x/.junos-sequences/NGC_7000.esq</Sequence>
<StartupCondition>
<Condition value='2025-07-15T22:30:00'>At</Condition>
</StartupCondition>
<Constraints>
<Constraint value='45'>MoonSeparation</Constraint>
<Constraint value='20'>MoonMaxAltitude</Constraint>
<Constraint>EnforceArtificialHorizon</Constraint>
</Constraints>
<CompletionCondition>
<Condition value='2025-07-16T03:00:00'>At</Condition>
</CompletionCondition>
<Steps>
<Step>Track</Step>
<Step>Focus</Step>
<Step>Align</Step>
<Step>Guide</Step>
</Steps>
</Job>
<SchedulerAlgorithm value='1'/>
<ErrorHandlingStrategy value='0'>
<RescheduleErrors />
<delay>600</delay>
</ErrorHandlingStrategy>
<StartupProcedure enabled='true'>
<PreStartupQueue>/q/pre.json</PreStartupQueue>
</StartupProcedure>
<ShutdownProcedure enabled='false'>
</ShutdownProcedure>
</SchedulerList>
";
        assert_eq!(xml, expected);
    }

    #[test]
    fn round_trip() {
        let settings = Settings {
            reschedule_errors: true,
            error_delay_secs: 120,
            startup_enabled: true,
            shutdown_enabled: true,
            pre_startup: "/q/pre.json".into(),
            post_startup: "/q/post.json".into(),
            pre_shutdown: "/q/pre_sd.json".into(),
            post_shutdown: "/q/post_sd.json".into(),
            ..Settings::default()
        };
        let written = jobs();
        let loaded = read(&write(&written, &settings, "Simulators")).unwrap();
        assert_eq!(loaded.jobs, written);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(
            loaded.settings,
            FileSettings {
                reschedule_errors: Some(true),
                error_delay_secs: Some(120),
                startup_enabled: Some(true),
                shutdown_enabled: Some(true),
                pre_startup: Some("/q/pre.json".into()),
                post_startup: Some("/q/post.json".into()),
                pre_shutdown: Some("/q/pre_sd.json".into()),
                post_shutdown: Some("/q/post_sd.json".into()),
            }
        );
        // An empty file says nothing about the settings.
        let bare = read(&write(&[], &Settings::default(), "")).unwrap();
        assert!(bare.jobs.is_empty());
        assert_eq!(bare.settings.pre_startup, None);
        assert_eq!(bare.settings.reschedule_errors, Some(false));
        assert_eq!(bare.settings.startup_enabled, Some(false));
    }

    #[test]
    fn kstars_sample_with_a_stray_ampersand_and_a_follower() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<SchedulerList version='2.2'>
<Profile>Simulators</Profile>
<Job>
<JobType lead='true'/>
<Name>M 42</Name>
<Group></Group>
<Coordinates>
<J2000RA>5.5881</J2000RA>
<J2000DE>-5.3911</J2000DE>
</Coordinates>
<PositionAngle>0</PositionAngle>
<Sequence>/home/x/Pictures/Orion & co/M42.esq</Sequence>
<StartupCondition>
<Condition>ASAP</Condition>
</StartupCondition>
<Constraints>
<Constraint value='15'>MinimumAltitude</Constraint>
<Constraint value='30'>MoonSeparation</Constraint>
<Constraint>EnforceTwilight</Constraint>
</Constraints>
<CompletionCondition>
<Condition>Loop</Condition>
</CompletionCondition>
<Steps>
<Step>Track</Step>
<Step>Align</Step>
<Step>Guide</Step>
</Steps>
</Job>
<Job>
<JobType lead='false'/>
<OpticalTrain>Guide train</OpticalTrain>
<PositionAngle>0</PositionAngle>
<Sequence>/home/x/follow.esq</Sequence>
<CompletionCondition>
<Condition>Sequence</Condition>
</CompletionCondition>
</Job>
<Job>
<JobType lead='true'/>
<Name>plate</Name>
<Group></Group>
<FITS>/home/x/plate.fits</FITS>
<Sequence>/home/x/plate.esq</Sequence>
<StartupCondition>
<Condition>ASAP</Condition>
</StartupCondition>
<Constraints>
</Constraints>
<CompletionCondition>
<Condition>Sequence</Condition>
</CompletionCondition>
<Steps>
</Steps>
</Job>
<SchedulerAlgorithm value='1'/>
<ErrorHandlingStrategy value='0'>
<delay>300</delay>
</ErrorHandlingStrategy>
<StartupProcedure enabled='true'>
<StartupScript>/home/x/startup.sh</StartupScript>
</StartupProcedure>
<ShutdownProcedure enabled='true'>
<ShutdownScript>/home/x/shutdown.sh</ShutdownScript>
</ShutdownProcedure>
</SchedulerList>
"#;
        let loaded = read(xml).unwrap();
        assert_eq!(loaded.jobs.len(), 1);
        let job = &loaded.jobs[0];
        assert_eq!(job.name, "M 42");
        assert_eq!(job.sequence.to_string_lossy(), "/home/x/Pictures/Orion & co/M42.esq");
        assert_eq!(job.min_alt, Some(15.0));
        assert_eq!(job.min_moon_sep, Some(30.0));
        assert_eq!(job.max_moon_alt, None);
        assert!(job.twilight && !job.horizon);
        assert_eq!(job.completion, Completion::Loop);
        assert_eq!(job.steps, Steps { track: true, focus: false, align: true, guide: true });
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("follower"));
        assert!(loaded.warnings[1].contains("FITS"));
        assert_eq!(loaded.settings.reschedule_errors, Some(false));
        assert_eq!(loaded.settings.error_delay_secs, Some(300));
        assert_eq!(loaded.settings.pre_startup.as_deref(), Some("/home/x/startup.sh"));
        assert_eq!(loaded.settings.post_startup, None);
        assert_eq!(loaded.settings.post_shutdown.as_deref(), Some("/home/x/shutdown.sh"));
    }

    #[test]
    fn rejects_other_roots_and_bad_xml() {
        assert!(read("<SequenceQueue version='2.6'></SequenceQueue>").unwrap_err().contains("SchedulerList"));
        assert!(read("not xml").is_err());
    }
}
