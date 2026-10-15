//! What the agent modules (`focus.rs`, `guide.rs`, `align.rs`) share: the
//! edge detector on an agent's `AGENT_START_PROCESS`, the newest-first log
//! KStars' modules keep, and the reaction a property change produces.

use std::collections::VecDeque;

use super::protocol::{Property, State};

/// What a property change asks the session to do.
#[derive(Default)]
pub struct Reaction {
    pub to_browser: Vec<String>,
    pub to_indigo: Vec<String>,
    /// A preview JPEG to fetch (BLOB link as INDIGO gave it) and forward.
    pub fetch_preview: Option<String>,
}

/// An agent process property (`AGENT_START_PROCESS`, AtMostOne): Busy while
/// a process runs with its item on, then Ok or Alert with the items cleared.
#[derive(Default)]
pub struct Process {
    last: Option<State>,
    /// The item that is running, while Busy.
    pub running: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Edge {
    /// Idle/Ok/Alert → Busy with `item` on, or the item changed while Busy
    /// (the guider switches CALIBRATION_AND_GUIDING to GUIDING in flight).
    Started(String),
    /// Busy → Ok (`ok`) or Alert; `item` is what was running.
    Finished { ok: bool, item: String },
    Still,
}

impl Process {
    pub fn observe(&mut self, p: &Property) -> Edge {
        let state = p.state;
        let was = self.last.replace(state);
        let selected = p.selected().map(str::to_string);
        if state == State::Busy {
            if was != Some(State::Busy) || (selected.is_some() && selected != self.running) {
                let item = selected.unwrap_or_default();
                self.running = Some(item.clone());
                return Edge::Started(item);
            }
            return Edge::Still;
        }
        if was == Some(State::Busy) {
            let item = self.running.take().unwrap_or_default();
            return Edge::Finished { ok: state == State::Ok, item };
        }
        Edge::Still
    }

    pub fn is_running(&self) -> bool {
        self.last == Some(State::Busy)
    }
}

/// A module log in KStars' shape: newest line first, one text blob.
#[derive(Default)]
pub struct Log {
    lines: VecDeque<String>,
}

const LOG_CAP: usize = 200;

impl Log {
    /// Adds a line unless it is empty or repeats the latest one. Returns
    /// whether anything changed.
    pub fn push(&mut self, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() || self.lines.front().is_some_and(|l| l == text) {
            return false;
        }
        self.lines.push_front(text.to_string());
        self.lines.truncate(LOG_CAP);
        true
    }

    pub fn text(&self) -> String {
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn latest(&self) -> Option<&str> {
        self.lines.front().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::{parse_frame, Frame};

    fn prop(state: &str, on: Option<&str>) -> Property {
        let items = ["PREVIEW", "FOCUSING"]
            .iter()
            .map(|i| format!(r#"{{"name":"{i}","value":{}}}"#, Some(*i) == on))
            .collect::<Vec<_>>()
            .join(",");
        let f = format!(r#"{{"defSwitchVector":{{"device":"A","name":"AGENT_START_PROCESS","perm":"rw","state":"{state}","rule":"AtMostOne","items":[{items}]}}}}"#);
        match parse_frame(&f) {
            Some(Frame::Def(p)) => p,
            _ => panic!("def"),
        }
    }

    #[test]
    fn edges() {
        let mut p = Process::default();
        assert_eq!(p.observe(&prop("Ok", None)), Edge::Still);
        assert_eq!(p.observe(&prop("Busy", Some("FOCUSING"))), Edge::Started("FOCUSING".into()));
        assert!(p.is_running());
        assert_eq!(p.observe(&prop("Busy", Some("FOCUSING"))), Edge::Still);
        assert_eq!(p.observe(&prop("Busy", Some("PREVIEW"))), Edge::Started("PREVIEW".into()));
        assert_eq!(p.observe(&prop("Ok", None)), Edge::Finished { ok: true, item: "PREVIEW".into() });
        assert!(!p.is_running());
        assert_eq!(p.observe(&prop("Busy", Some("FOCUSING"))), Edge::Started("FOCUSING".into()));
        assert_eq!(p.observe(&prop("Alert", None)), Edge::Finished { ok: false, item: "FOCUSING".into() });
        assert_eq!(p.observe(&prop("Alert", None)), Edge::Still);
    }

    #[test]
    fn log_is_newest_first_and_deduplicated() {
        let mut l = Log::default();
        assert!(l.push("a"));
        assert!(!l.push("a"));
        assert!(!l.push("  "));
        assert!(l.push("b"));
        assert_eq!(l.text(), "b\na");
        assert_eq!(l.latest(), Some("b"));
    }
}
