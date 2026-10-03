//! Files › Planning: reading the Scheduler's files into what the views show.
//!
//! - `.esl` — `SchedulerProcess::saveScheduler` (schedulerprocess.cpp:2977):
//!   `<SchedulerList>` with `<Profile>`, an optional `<Mosaic>`, one `<Job>`
//!   per job, then the startup / shutdown procedures.
//! - `.esq` — `<SequenceQueue>` with one `<Job>` per frame row, as written by
//!   `sequence_editor::build_esq_xml` or KStars' own Capture module (gain and
//!   offset in `<Properties>`, or as `<Gain>` / `<Offset>` in older files).
//! - Task-queue `.json` — `{name, description, tasks:[{template_id, device,
//!   parameters}]}` (`scheduler/queue_model.rs`).
//!
//! KStars writes file paths (`<Sequence>`, `<FITS>`) and the mosaic target
//! unescaped, so a `&` in a folder name makes the XML ill-formed;
//! [`escape_stray_amps`] repairs that before parsing.

use std::borrow::Cow;

use roxmltree::{Document, Node};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Schedules,
    Sequences,
    Queues,
    Scripts,
}

impl Kind {
    pub(crate) const ALL: [Kind; 4] = [Kind::Schedules, Kind::Sequences, Kind::Queues, Kind::Scripts];

    /// The `kind` of `/api/planning/*`.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Kind::Schedules => "schedules",
            Kind::Sequences => "sequences",
            Kind::Queues => "queues",
            Kind::Scripts => "scripts",
        }
    }

    /// The kind a captures file reads as (a mosaic import writes `.esl` and
    /// `.esq` next to its frames).
    pub(crate) fn from_ext(ext: &str) -> Option<Kind> {
        match ext.to_ascii_lowercase().as_str() {
            "esl" => Some(Kind::Schedules),
            "esq" => Some(Kind::Sequences),
            _ => None,
        }
    }
}

// ── Shapes ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Schedule {
    pub profile: String,
    pub mosaic: Option<MosaicInfo>,
    pub jobs: Vec<ScheduleJob>,
    pub startup: Procedure,
    pub shutdown: Procedure,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MosaicInfo {
    pub target: String,
    pub grid_w: String,
    pub grid_h: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ScheduleJob {
    /// A follower job (`<JobType lead='false'/>`) has no target of its own.
    pub lead: bool,
    pub name: String,
    pub group: String,
    pub ra_h: Option<f64>,
    pub dec_deg: Option<f64>,
    pub sequence: String,
    pub start: Condition,
    pub completion: Condition,
    pub min_alt: Option<f64>,
    pub moon_sep: Option<f64>,
    pub moon_max_alt: Option<f64>,
    pub twilight: bool,
    pub horizon: bool,
    /// "Track", "Focus", "Align", "Guide", in file order.
    pub steps: Vec<String>,
}

/// A `<Condition [value='…']>Name</Condition>`: `ASAP`, `At` (ISO time),
/// `Sequence`, `Repeat` (count), `Loop`, or whatever an older file holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Condition {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Procedure {
    pub enabled: bool,
    pub pre: String,
    pub post: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Sequence {
    pub frames: Vec<SeqRow>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SeqRow {
    pub frame_type: String,
    pub filter: String,
    pub exposure: Option<f64>,
    pub count: Option<u32>,
    pub bin: String,
    pub gain: String,
    pub offset: String,
    pub iso: String,
    pub target: String,
    pub dir: String,
}

impl SeqRow {
    pub(crate) fn duration_secs(&self) -> f64 {
        self.exposure.unwrap_or(0.0) * f64::from(self.count.unwrap_or(0))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Queue {
    pub title: String,
    pub description: String,
    pub tasks: Vec<QueueTask>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QueueTask {
    /// Only set in KStars' queue format, where a custom INDI step has no
    /// template id to name it.
    pub name: String,
    pub template_id: String,
    pub device: String,
    /// `(name, value)` in file order.
    pub params: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Parsed {
    Schedule(Schedule),
    Sequence(Sequence),
    Queue(Queue),
    Script,
}

pub(crate) fn parse(kind: Kind, text: &str) -> Result<Parsed, String> {
    match kind {
        Kind::Schedules => parse_schedule(text).map(Parsed::Schedule),
        Kind::Sequences => parse_sequence(text).map(Parsed::Sequence),
        Kind::Queues => parse_queue(text).map(Parsed::Queue),
        Kind::Scripts => Ok(Parsed::Script),
    }
}

// ── XML helpers ──────────────────────────────────────────────────────────────

/// `&` that doesn't start one of XML's five entities or a character
/// reference becomes `&amp;`.
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
    child(n, tag).map(|c| own_text(c)).unwrap_or_default()
}

fn own_text(n: Node) -> String {
    n.text().map(str::trim).unwrap_or_default().to_string()
}

fn num(n: Node, tag: &str) -> Option<f64> {
    text(n, tag).parse().ok()
}

fn condition(n: Option<Node>) -> Condition {
    let Some(c) = n.and_then(|n| child(n, "Condition")) else { return Condition::default() };
    Condition { name: own_text(c), value: c.attribute("value").unwrap_or_default().to_string() }
}

fn root_named<'a, 'i>(doc: &'a Document<'i>, tag: &str) -> Result<Node<'a, 'i>, String> {
    let root = doc.root_element();
    if root.has_tag_name(tag) {
        Ok(root)
    } else {
        Err(format!("<{}> where <{tag}> was expected", root.tag_name().name()))
    }
}

// ── .esl ─────────────────────────────────────────────────────────────────────

pub(crate) fn parse_schedule(xml: &str) -> Result<Schedule, String> {
    let xml = escape_stray_amps(xml);
    let doc = Document::parse(&xml).map_err(|e| e.to_string())?;
    let root = root_named(&doc, "SchedulerList")?;

    let mosaic = child(root, "Mosaic").map(|m| MosaicInfo {
        target: text(m, "Target"),
        grid_w: text(m, "GridW"),
        grid_h: text(m, "GridH"),
    });

    let jobs = children(root, "Job")
        .map(|j| {
            let coords = child(j, "Coordinates");
            let mut job = ScheduleJob {
                lead: child(j, "JobType").and_then(|t| t.attribute("lead")) != Some("false"),
                name: text(j, "Name"),
                group: text(j, "Group"),
                ra_h: coords.and_then(|c| num(c, "J2000RA")),
                dec_deg: coords.and_then(|c| num(c, "J2000DE")),
                sequence: text(j, "Sequence"),
                start: condition(child(j, "StartupCondition")),
                completion: condition(child(j, "CompletionCondition")),
                ..ScheduleJob::default()
            };
            if let Some(cs) = child(j, "Constraints") {
                for c in children(cs, "Constraint") {
                    let value = c.attribute("value").and_then(|v| v.parse().ok());
                    match own_text(c).as_str() {
                        "MinimumAltitude" => job.min_alt = value,
                        "MoonSeparation" => job.moon_sep = value,
                        "MoonMaxAltitude" => job.moon_max_alt = value,
                        "EnforceTwilight" => job.twilight = true,
                        "EnforceArtificialHorizon" => job.horizon = true,
                        _ => {}
                    }
                }
            }
            if let Some(steps) = child(j, "Steps") {
                job.steps = children(steps, "Step").map(own_text).filter(|s| !s.is_empty()).collect();
            }
            job
        })
        .collect();

    // KStars 3.8+ names a task queue per slot; older files a startup /
    // shutdown script.
    let procedure = |tag: &str, pre: &[&str], post: &str| {
        child(root, tag).map_or_else(Procedure::default, |p| Procedure {
            enabled: p.attribute("enabled") == Some("true"),
            pre: pre.iter().map(|t| text(p, t)).find(|v| !v.is_empty()).unwrap_or_default(),
            post: text(p, post),
        })
    };

    Ok(Schedule {
        profile: text(root, "Profile"),
        mosaic,
        jobs,
        startup: procedure("StartupProcedure", &["PreStartupQueue", "StartupScript"], "PostStartupQueue"),
        shutdown: procedure("ShutdownProcedure", &["PreShutdownQueue"], "PostShutdownQueue"),
    })
}

// ── .esq ─────────────────────────────────────────────────────────────────────

/// A `<PropertyVector name='…'>`'s first `<OneElement>` under `<Properties>`.
fn property(job: Node, vector: &str) -> String {
    child(job, "Properties")
        .and_then(|p| children(p, "PropertyVector").find(|v| v.attribute("name") == Some(vector)))
        .and_then(|v| child(v, "OneElement"))
        .map(|e| own_text(e))
        .unwrap_or_default()
}

pub(crate) fn parse_sequence(xml: &str) -> Result<Sequence, String> {
    let xml = escape_stray_amps(xml);
    let doc = Document::parse(&xml).map_err(|e| e.to_string())?;
    let root = root_named(&doc, "SequenceQueue")?;

    let frames = children(root, "Job")
        .map(|j| {
            let bin = child(j, "Binning").map_or_else(String::new, |b| {
                let axis = |t| Some(text(b, t)).filter(|v| !v.is_empty()).unwrap_or_else(|| "1".into());
                format!("{}\u{00d7}{}", axis("X"), axis("Y"))
            });
            let or_legacy = |modern: String, legacy: &str| if modern.is_empty() { text(j, legacy) } else { modern };
            SeqRow {
                frame_type: text(j, "Type"),
                filter: text(j, "Filter"),
                exposure: num(j, "Exposure"),
                count: text(j, "Count").parse().ok(),
                bin,
                gain: or_legacy(property(j, "CCD_GAIN"), "Gain"),
                offset: or_legacy(property(j, "CCD_OFFSET"), "Offset"),
                iso: text(j, "ISOIndex"),
                target: text(j, "TargetName"),
                dir: text(j, "FITSDirectory"),
            }
        })
        .collect();
    Ok(Sequence { frames })
}

// ── task-queue .json ─────────────────────────────────────────────────────────

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A task collection (`tasks`) or a queue in KStars' own format (`items`,
/// each wrapping its `task`) — the Scheduler editor writes the latter once a
/// queue holds a custom INDI step.
pub(crate) fn parse_queue(text: &str) -> Result<Queue, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let tasks: Vec<&Value> = match (doc["tasks"].as_array(), doc["items"].as_array()) {
        (Some(tasks), _) => tasks.iter().collect(),
        (None, Some(items)) => items.iter().map(|item| &item["task"]).collect(),
        (None, None) => return Err("no \"tasks\" or \"items\" array".to_string()),
    };
    Ok(Queue {
        title: value_str(&doc["name"]),
        description: value_str(&doc["description"]),
        tasks: tasks
            .into_iter()
            .map(|t| QueueTask {
                name: value_str(&t["name"]),
                template_id: value_str(&t["template_id"]),
                device: value_str(&t["device"]),
                params: t["parameters"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), value_str(v))).collect())
                    .unwrap_or_default(),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a KStars 3.8 save (schedulerprocess.cpp:2977), with the
    /// unescaped `&` it writes in paths.
    const ESL: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<SchedulerList version='2.2'>
<Profile>Simulators</Profile>
<Job>
<JobType lead='true'/>
<Name>M31 &amp; M110</Name>
<Group></Group>
<Coordinates>
<J2000RA>0.712</J2000RA>
<J2000DE>41.269</J2000DE>
</Coordinates>
<Sequence>/home/u/.junos-sequences/M31 & co.esq</Sequence>
<StartupCondition>
<Condition>ASAP</Condition>
</StartupCondition>
<Constraints>
<Constraint value='30'>MinimumAltitude</Constraint>
<Constraint value='20'>MoonSeparation</Constraint>
<Constraint>EnforceTwilight</Constraint>
</Constraints>
<CompletionCondition>
<Condition value='3'>Repeat</Condition>
</CompletionCondition>
<Steps>
<Step>Track</Step>
<Step>Focus</Step>
<Step>Guide</Step>
</Steps>
</Job>
<Job>
<JobType lead='false'/>
<Sequence>/home/u/.junos-sequences/flats.esq</Sequence>
<CompletionCondition>
<Condition>Sequence</Condition>
</CompletionCondition>
</Job>
<SchedulerAlgorithm value='1'/>
<StartupProcedure enabled='true'>
<PreStartupQueue>/q/junos_pre_startup.json</PreStartupQueue>
</StartupProcedure>
<ShutdownProcedure enabled='false'>
</ShutdownProcedure>
</SchedulerList>
";

    #[test]
    fn reads_a_schedule() {
        let s = parse_schedule(ESL).unwrap();
        assert_eq!(s.profile, "Simulators");
        assert_eq!(s.jobs.len(), 2);
        let j = &s.jobs[0];
        assert!(j.lead);
        assert_eq!(j.name, "M31 & M110");
        assert_eq!(j.ra_h, Some(0.712));
        assert_eq!(j.dec_deg, Some(41.269));
        assert_eq!(j.sequence, "/home/u/.junos-sequences/M31 & co.esq");
        assert_eq!(j.start, Condition { name: "ASAP".into(), value: String::new() });
        assert_eq!(j.completion, Condition { name: "Repeat".into(), value: "3".into() });
        assert_eq!((j.min_alt, j.moon_sep, j.moon_max_alt), (Some(30.0), Some(20.0), None));
        assert!(j.twilight && !j.horizon);
        assert_eq!(j.steps, ["Track", "Focus", "Guide"]);
        assert!(!s.jobs[1].lead);
        assert_eq!(s.jobs[1].completion.name, "Sequence");
        assert_eq!(s.startup, Procedure { enabled: true, pre: "/q/junos_pre_startup.json".into(), post: String::new() });
        assert!(!s.shutdown.enabled);
    }

    #[test]
    fn reads_our_own_sequence() {
        use crate::components::sequence_editor::{build_esq_xml, SeqFrame};
        let light = SeqFrame {
            filter: "Ha".into(),
            exposure: "300".into(),
            count: "12".into(),
            bin_x: "2".into(),
            bin_y: "2".into(),
            ..SeqFrame::default()
        };
        let xml = build_esq_xml("NGC 7000", "/data/A&B", &[light], false);
        let s = parse_sequence(&xml).unwrap();
        assert_eq!(s.frames.len(), 1);
        let f = &s.frames[0];
        assert_eq!((f.frame_type.as_str(), f.filter.as_str()), ("Light", "Ha"));
        assert_eq!((f.exposure, f.count), (Some(300.0), Some(12)));
        assert_eq!((f.bin.as_str(), f.gain.as_str(), f.offset.as_str()), ("2\u{00d7}2", "100", ""));
        assert_eq!((f.target.as_str(), f.dir.as_str()), ("NGC 7000", "/data/A&B"));
        assert_eq!(f.duration_secs(), 3600.0);
    }

    #[test]
    fn reads_legacy_gain_elements() {
        let xml = "<SequenceQueue version='2.0'><Job><Exposure>1.5</Exposure><Type>Flat</Type>\
                   <Count>20</Count><Gain>50</Gain><Offset>10</Offset></Job></SequenceQueue>";
        let f = &parse_sequence(xml).unwrap().frames[0];
        assert_eq!((f.gain.as_str(), f.offset.as_str(), f.bin.as_str()), ("50", "10", ""));
    }

    #[test]
    fn rejects_the_wrong_root() {
        assert!(parse_schedule("<SequenceQueue/>").is_err());
        assert!(parse_sequence("not xml").is_err());
    }

    #[test]
    fn repairs_only_stray_ampersands() {
        assert_eq!(escape_stray_amps("a & b"), "a &amp; b");
        assert_eq!(escape_stray_amps("&amp; &lt; &#38; &#x26; &nbsp;"), "&amp; &lt; &#38; &#x26; &amp;nbsp;");
        assert_eq!(escape_stray_amps("trailing &"), "trailing &amp;");
        assert!(matches!(escape_stray_amps("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn reads_a_queue() {
        let q = parse_queue(r#"{"name":"Pre","description":"Created with Junos","version":"1.0",
            "tasks":[{"template_id":"mount_park","device":"","failure_action":2,"parameters":{"wait_timeout":60}},
                     {"template_id":"script_execute","parameters":{"script_path":"/s/roof.sh","timeout":300}}]}"#).unwrap();
        assert_eq!(q.title, "Pre");
        assert_eq!(q.tasks.len(), 2);
        assert_eq!(q.tasks[0].params, [("wait_timeout".to_string(), "60".to_string())]);
        assert_eq!(q.tasks[1].params[0], ("script_path".to_string(), "/s/roof.sh".to_string()));
        assert!(parse_queue("{}").is_err());

        let q = parse_queue(r#"{"name":"Post","items":[{"id":"junos-1","task":{"name":"Set Dome · DOME_SHUTTER.SHUTTER_OPEN = On",
            "template_id":"","device":"Dome","parameters":{},"actions":[{"type":"SET"}]}}]}"#).unwrap();
        assert_eq!(q.tasks.len(), 1);
        assert_eq!(q.tasks[0].device, "Dome");
        assert!(q.tasks[0].name.starts_with("Set Dome"));
    }
}
