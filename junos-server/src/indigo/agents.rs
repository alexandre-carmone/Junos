//! The INDIGO agents: which ones are on the server, loading the missing ones,
//! and keeping their device selections filled.
//!
//! INDIGO's filter agents (`indigo_filter.c`) pick their devices through
//! `FILTER_*_LIST` switches — and *they* connect the device they select. A
//! device that is already connected is refused ("'X' is busy or in use"), so
//! the relay must not connect a device an agent is about to claim. This
//! module is therefore a reconciler, not an imperative "connect everything":
//! `plan` compares each list with the roles and returns the writes that move
//! the server one step closer, rate-limited, and connects directly only the
//! devices no agent claims (which, with no agents at all, is every device —
//! the pre-agent behaviour).
//!
//! The roles follow the agents (`roles.rs`): a list that already names a
//! device is never overridden here, only a list still at `NONE` is filled
//! with the default pick. `profile_stop` releases every list and disconnects
//! the leftovers; `profile_start` fills them again.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::commands::{reply_msg, PROFILE_NAME};
use super::protocol::{interface, new_vector_msg, Change, Kind as PropKind, PropertyStore, State};
use super::roles::Roles;

/// Item 0 of every `FILTER_*_LIST`.
pub const NONE: &str = "NONE";

/// How long after the session start (or the last driver load) direct
/// connects are held back, so an agent's own selection comes first.
const DISCOVERY: Duration = Duration::from_secs(5);
const DISCOVERY_AFTER_LOAD: Duration = Duration::from_secs(10);
/// Minimum spacing between identical writes (connects, releases, inits).
const RESEND: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Imager,
    Guider,
    Astrometry,
    Mount,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Imager, Kind::Guider, Kind::Astrometry, Kind::Mount];

    /// The driver to `LOAD` when the agent is missing.
    pub fn driver(self) -> &'static str {
        match self {
            Self::Imager => "indigo_agent_imager",
            Self::Guider => "indigo_agent_guider",
            Self::Astrometry => "indigo_agent_astrometry",
            Self::Mount => "indigo_agent_mount",
        }
    }

    /// A property only this agent defines — the discovery key, so a renamed
    /// or remote (`… @ host`) agent is still recognised.
    fn marker(self) -> &'static str {
        match self {
            Self::Imager => "AGENT_IMAGER_BATCH",
            Self::Guider => "AGENT_GUIDER_SETTINGS",
            Self::Astrometry => "AGENT_PLATESOLVER_WCS",
            Self::Mount => "FILTER_MOUNT_LIST",
        }
    }

    /// The default device name, for messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::Imager => "Imager Agent",
            Self::Guider => "Guider Agent",
            Self::Astrometry => "Astrometry Agent",
            Self::Mount => "Mount Agent",
        }
    }
}

/// The agents found on the server, by device name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentSet {
    pub imager: Option<String>,
    pub guider: Option<String>,
    pub astrometry: Option<String>,
    pub mount: Option<String>,
}

impl AgentSet {
    pub fn discover(store: &PropertyStore) -> Self {
        let mut set = Self::default();
        for device in store.device_names() {
            if store.interface(device) & interface::AGENT == 0 {
                continue;
            }
            for kind in Kind::ALL {
                if store.get(device, kind.marker()).is_some() && set.get(kind).is_none() {
                    *set.slot(kind) = Some(device.to_string());
                }
            }
        }
        set
    }

    pub fn get(&self, kind: Kind) -> Option<&str> {
        match kind {
            Kind::Imager => self.imager.as_deref(),
            Kind::Guider => self.guider.as_deref(),
            Kind::Astrometry => self.astrometry.as_deref(),
            Kind::Mount => self.mount.as_deref(),
        }
    }

    fn slot(&mut self, kind: Kind) -> &mut Option<String> {
        match kind {
            Kind::Imager => &mut self.imager,
            Kind::Guider => &mut self.guider,
            Kind::Astrometry => &mut self.astrometry,
            Kind::Mount => &mut self.mount,
        }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        Kind::ALL.into_iter().filter_map(|k| self.get(k))
    }

    /// The device an agent's list names, `None` for `NONE` or no such list.
    pub fn selected(store: &PropertyStore, agent: &str, list: &str) -> Option<String> {
        store
            .get(agent, list)?
            .selected()
            .filter(|s| *s != NONE)
            .map(str::to_string)
    }

    /// Every device some agent list currently holds.
    pub fn held(&self, store: &PropertyStore) -> HashSet<String> {
        let mut out = HashSet::new();
        for agent in self.names() {
            for p in store.properties(agent) {
                if p.name.starts_with("FILTER_") && p.name.ends_with("_LIST") && p.name != "FILTER_RELATED_AGENT_LIST" {
                    if let Some(d) = p.selected().filter(|s| *s != NONE) {
                        out.insert(d.to_string());
                    }
                }
            }
        }
        out
    }

    /// Whether an agent is running one of its processes.
    pub fn busy(store: &PropertyStore, agent: &str) -> bool {
        store.get(agent, "AGENT_START_PROCESS").is_some_and(|p| p.state == State::Busy)
    }
}

/// Which device each agent list should name.
fn desires<'a>(set: &'a AgentSet, roles: &'a Roles) -> Vec<(&'a str, &'static str, Option<&'a str>)> {
    let mut out = Vec::new();
    if let Some(a) = set.imager.as_deref() {
        out.push((a, "FILTER_CCD_LIST", roles.camera.as_deref()));
        out.push((a, "FILTER_WHEEL_LIST", roles.wheel.as_deref()));
        out.push((a, "FILTER_FOCUSER_LIST", roles.focuser.as_deref()));
    }
    if let Some(a) = set.guider.as_deref() {
        out.push((a, "FILTER_CCD_LIST", roles.guider.as_deref()));
        out.push((a, "FILTER_GUIDER_LIST", roles.guider_port.as_deref()));
    }
    if let Some(a) = set.mount.as_deref() {
        out.push((a, "FILTER_MOUNT_LIST", roles.mount.as_deref()));
    }
    out
}

/// Which agents each agent should relate to (the other side follows, the
/// relation is symmetric by default): the solver takes images through the
/// imager and syncs through the mount agent, the guider learns the mount's
/// coordinates, the imager dithers through the guider.
fn related<'a>(set: &'a AgentSet) -> Vec<(&'a str, Vec<&'a str>)> {
    let mut out = Vec::new();
    if let Some(a) = set.astrometry.as_deref() {
        out.push((a, set.imager.iter().chain(set.mount.iter()).map(String::as_str).collect()));
    }
    if let Some(a) = set.guider.as_deref() {
        out.push((a, set.mount.iter().map(String::as_str).collect()));
    }
    if let Some(a) = set.imager.as_deref() {
        out.push((a, set.guider.iter().map(String::as_str).collect()));
    }
    out
}

#[derive(Default)]
pub struct Plan {
    pub to_indigo: Vec<String>,
    pub to_browser: Vec<String>,
}

struct Record {
    sent_at: Instant,
    attempts: u32,
    desired: String,
}

pub struct Agents {
    pub set: AgentSet,
    active: bool,
    ready_at: Instant,
    load_sent: HashSet<Kind>,
    unavailable: HashSet<Kind>,
    /// Selection writes per `(agent, list)`, for the backoff.
    records: HashMap<(String, String), Record>,
    /// Every other write, keyed by `device\0property\0item`, for `RESEND`.
    sent: HashMap<String, Instant>,
    /// Direct connects per device: a driver that can't connect (the file
    /// simulator without a file) would otherwise be retried every `RESEND`.
    connect_attempts: HashMap<String, u32>,
}

impl Agents {
    pub fn new(now: Instant) -> Self {
        Self {
            set: AgentSet::default(),
            active: true,
            ready_at: now + DISCOVERY,
            load_sent: HashSet::new(),
            unavailable: HashSet::new(),
            records: HashMap::new(),
            sent: HashMap::new(),
            connect_attempts: HashMap::new(),
        }
    }

    /// `profile_start` / `profile_stop`.
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
        self.records.clear();
        self.sent.clear();
        self.connect_attempts.clear();
    }

    /// Whether `kind` was looked for and its driver isn't on the server.
    #[cfg(test)]
    pub fn unavailable(&self, kind: Kind) -> bool {
        self.unavailable.contains(&kind)
    }

    /// Whether a change may alter the plan (so the session runs it at once
    /// rather than waiting for the tick).
    pub fn relevant(change: &Change) -> bool {
        match change {
            Change::Defined { name, .. } | Change::Updated { name, .. } => {
                name == "INFO"
                    || name == "CONNECTION"
                    || name == "DRIVERS"
                    || name == "AGENT_START_PROCESS"
                    || (name.starts_with("FILTER_") && name.ends_with("_LIST"))
            }
            Change::Deleted { .. } => true,
            Change::Message { .. } => false,
        }
    }

    /// Bookkeeping on a property change: re-discover agents, and notice a
    /// refused selection so the next attempt waits.
    pub fn on_change(&mut self, change: &Change, store: &PropertyStore, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        match change {
            Change::Defined { name, .. } if name == "INFO" || Kind::ALL.iter().any(|k| k.marker() == name) => {
                self.set = AgentSet::discover(store);
            }
            Change::Deleted { device, name } => {
                self.set = AgentSet::discover(store);
                if name.is_none() {
                    self.records.retain(|(a, _), _| a != device);
                }
            }
            Change::Updated { device, name } if name == "LOAD" => {
                // A driver the server doesn't have: the LOAD comes back Alert.
                let Some(p) = store.get(device, name) else { return out };
                if p.state == State::Alert {
                    if let Some(kind) = Kind::ALL.into_iter().find(|k| p.text("DRIVER") == Some(k.driver())) {
                        if self.load_sent.contains(&kind) {
                            self.unavailable.insert(kind);
                            out.push(reply_msg(
                                "device_message",
                                json!({ "device": device, "message": format!("{} could not be loaded: {}", kind.label(), p.message.clone().unwrap_or_default()) }),
                            ));
                        }
                    }
                }
            }
            Change::Updated { device, name } if name.starts_with("FILTER_") && name.ends_with("_LIST") => {
                let Some(p) = store.get(device, name) else { return out };
                let refused = p.state == State::Alert
                    && p.on(NONE)
                    && p.message.as_deref().is_some_and(|m| m.contains("busy or in use"));
                if refused {
                    if let Some(r) = self.records.get_mut(&(device.clone(), name.clone())) {
                        r.attempts += 1;
                        r.sent_at = now;
                    }
                    out.push(reply_msg(
                        "device_message",
                        json!({ "device": device, "message": p.message.clone().unwrap_or_default() }),
                    ));
                }
            }
            _ => {}
        }
        out
    }

    /// Devices some agent will claim (so the session must not connect them).
    fn claimed(&self, roles: &Roles) -> HashSet<String> {
        desires(&self.set, roles)
            .into_iter()
            .filter_map(|(_, _, d)| d.map(str::to_string))
            .collect()
    }

    fn due(&mut self, key: String, now: Instant, every: Duration) -> bool {
        match self.sent.get(&key) {
            Some(t) if now.duration_since(*t) < every => false,
            _ => {
                self.sent.insert(key, now);
                true
            }
        }
    }

    /// A rate-limited switch write, only when the store doesn't already show
    /// the item on.
    fn ensure_on(&mut self, store: &PropertyStore, device: &str, prop: &str, item: &str, now: Instant, out: &mut Vec<String>) {
        let Some(p) = store.get(device, prop) else { return };
        if p.on(item) {
            return;
        }
        if self.due(format!("{device}\0{prop}\0{item}"), now, RESEND) {
            out.push(new_vector_msg(PropKind::Switch, device, prop, &[(item.to_string(), json!(true))]));
        }
    }

    /// The writes that move the server towards the desired selection state.
    pub fn plan(&mut self, store: &PropertyStore, roles: &Roles, now: Instant) -> Plan {
        let mut plan = Plan::default();
        self.set = AgentSet::discover(store);
        let w = &mut plan.to_indigo;

        // One-time agent setup. The solver would otherwise solve every image
        // the imager produces, and its preview JPEG is what the Align frame
        // shows.
        if let Some(a) = self.set.astrometry.clone() {
            self.ensure_on(store, &a, "AGENT_PLATESOLVER_SOLVE_IMAGES", "DISABLED", now, w);
            self.ensure_on(store, &a, "CCD_PREVIEW", "ENABLED", now, w);
        }

        // Load the missing agents, once each. `DRIVERS` only lists the
        // loaded drivers, so a driver that isn't there is loaded blind; the
        // server answers an unknown one with an Alert on `LOAD`
        // (`on_change` then marks the kind unavailable).
        let server = store
            .device_names()
            .find(|d| store.get(d, "LOAD").is_some() && store.get(d, "DRIVERS").is_some())
            .map(str::to_string);
        if let Some(server) = server {
            let drivers = store.get(&server, "DRIVERS").cloned();
            for kind in Kind::ALL {
                if self.set.get(kind).is_some() || self.load_sent.contains(&kind) || self.unavailable.contains(&kind) {
                    continue;
                }
                // Loaded but its properties haven't arrived yet: wait.
                if drivers.as_ref().is_some_and(|p| p.on(kind.driver())) {
                    continue;
                }
                w.push(new_vector_msg(PropKind::Text, &server, "LOAD", &[("DRIVER".into(), json!(kind.driver()))]));
                self.load_sent.insert(kind);
                self.ready_at = self.ready_at.max(now + DISCOVERY_AFTER_LOAD);
            }
        }

        // Running processes: never touch their agent; abort them when stopping.
        let busy: HashSet<String> = self.set.names().filter(|a| AgentSet::busy(store, a)).map(str::to_string).collect();
        if !self.active {
            for a in busy.clone() {
                self.ensure_on(store, &a, "AGENT_ABORT_PROCESS", "ABORT", now, w);
            }
        }

        // The device lists.
        let held = self.set.held(store);
        let desires: Vec<(String, &'static str, Option<String>)> = desires(&self.set, roles)
            .into_iter()
            .map(|(a, l, d)| (a.to_string(), l, d.map(str::to_string)))
            .collect();
        for (agent, list, desired) in &desires {
            if busy.contains(agent) {
                continue;
            }
            let Some(p) = store.get(agent, list) else { continue };
            let selected = p.selected().filter(|s| *s != NONE).map(str::to_string);
            if !self.active {
                if selected.is_some() {
                    self.ensure_on(store, agent, list, NONE, now, w);
                }
                continue;
            }
            if p.state == State::Busy {
                continue;
            }
            let key = (agent.clone(), list.to_string());
            if selected.is_some() {
                self.records.remove(&key);
                continue;
            }
            let Some(desired) = desired else { continue };
            if p.item(desired).is_none() {
                continue;
            }
            // The agent only accepts a disconnected device.
            let Some(conn) = store.get(desired, "CONNECTION") else { continue };
            if conn.on("CONNECTED") || conn.state == State::Busy {
                if conn.state != State::Busy && !held.contains(desired) {
                    self.ensure_on(store, desired, "CONNECTION", "DISCONNECTED", now, w);
                }
                continue;
            }
            let record = self.records.entry(key).or_insert(Record { sent_at: now - Duration::from_secs(3600), attempts: 0, desired: desired.clone() });
            if record.desired != *desired {
                record.attempts = 0;
                record.desired = desired.clone();
            }
            let backoff = Duration::from_secs((3u64 << record.attempts.min(4)).min(30));
            if now.duration_since(record.sent_at) < backoff {
                continue;
            }
            record.sent_at = now;
            record.attempts += 1;
            w.push(new_vector_msg(PropKind::Switch, agent, list, &[(desired.clone(), json!(true))]));
        }

        // The related-agent links.
        if self.active {
            let set = self.set.clone();
            for (agent, wanted) in related(&set) {
                if busy.contains(agent) {
                    continue;
                }
                let Some(p) = store.get(agent, "FILTER_RELATED_AGENT_LIST") else { continue };
                let missing: Vec<(String, Value)> = wanted
                    .iter()
                    .filter(|n| p.item(n).is_some_and(|i| !i.on()))
                    .map(|n| (n.to_string(), json!(true)))
                    .collect();
                if missing.is_empty() {
                    continue;
                }
                let key = format!("{agent}\0FILTER_RELATED_AGENT_LIST\0{}", missing.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(","));
                if self.due(key, now, RESEND) {
                    w.push(new_vector_msg(PropKind::Switch, agent, "FILTER_RELATED_AGENT_LIST", &missing));
                }
            }
        }

        // Everything no agent claims is connected directly — once the agents
        // have had their chance. When the profile is stopped, everything that
        // is still connected is disconnected (the agents release theirs on
        // NONE; this catches the rest).
        let claimed = if self.active { self.claimed(roles) } else { HashSet::new() };
        let devices: Vec<String> = store
            .device_names()
            .filter(|d| store.interface(d) & interface::AGENT == 0)
            .filter(|d| store.get(d, "CONNECTION").is_some_and(|p| p.state != State::Busy))
            .map(str::to_string)
            .collect();
        for d in devices {
            let connected = store.connected(&d);
            if self.active {
                if connected {
                    self.connect_attempts.remove(&d);
                } else if now >= self.ready_at && !claimed.contains(&d) && !held.contains(&d) {
                    let attempts = *self.connect_attempts.get(&d).unwrap_or(&0);
                    let every = RESEND * (1 << attempts.min(5));
                    if self.due(format!("{d}\0CONNECTION\0CONNECTED"), now, every) {
                        self.connect_attempts.insert(d.clone(), attempts + 1);
                        w.push(new_vector_msg(PropKind::Switch, &d, "CONNECTION", &[("CONNECTED".to_string(), json!(true))]));
                    }
                }
            } else if connected {
                self.ensure_on(store, &d, "CONNECTION", "DISCONNECTED", now, w);
            }
        }
        plan
    }

    /// The `device_message` for a command that needs an agent the server
    /// doesn't have.
    pub fn missing_msg(&self, kind: Kind) -> String {
        let why = if self.unavailable.contains(&kind) {
            "its driver is not installed on the INDIGO server"
        } else {
            "it is not loaded on the INDIGO server yet"
        };
        reply_msg("device_message", json!({ "device": PROFILE_NAME, "message": format!("No {}: {why}", kind.label()) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indigo::protocol::parse_frame;

    fn info(device: &str, iface: u32) -> String {
        format!(r#"{{"defTextVector":{{"device":"{device}","name":"INFO","perm":"ro","state":"Ok","items":[{{"name":"DEVICE_INTERFACE","value":"{iface}"}}]}}}}"#)
    }

    fn connection(device: &str, connected: bool) -> String {
        format!(r#"{{"defSwitchVector":{{"device":"{device}","name":"CONNECTION","perm":"rw","state":"Ok","items":[{{"name":"CONNECTED","value":{connected}}},{{"name":"DISCONNECTED","value":{}}}]}}}}"#, !connected)
    }

    fn list(agent: &str, name: &str, state: &str, items: &[(&str, bool)]) -> String {
        let items: Vec<String> = std::iter::once((NONE, !items.iter().any(|(_, v)| *v)))
            .chain(items.iter().copied())
            .map(|(n, v)| format!(r#"{{"name":"{n}","value":{v}}}"#))
            .collect();
        format!(r#"{{"defSwitchVector":{{"device":"{agent}","name":"{name}","perm":"rw","state":"{state}","rule":"OneOfMany","items":[{}]}}}}"#, items.join(","))
    }

    fn number(device: &str, name: &str) -> String {
        format!(r#"{{"defNumberVector":{{"device":"{device}","name":"{name}","perm":"rw","state":"Ok","items":[{{"name":"X","value":0}}]}}}}"#)
    }

    fn apply(s: &mut PropertyStore, frames: &[String]) {
        for f in frames {
            s.apply(parse_frame(f).unwrap());
        }
    }

    fn writes(plan: &Plan) -> Vec<(String, String, Vec<(String, Value)>)> {
        plan.to_indigo
            .iter()
            .map(|m| {
                let v: Value = serde_json::from_str(m).unwrap();
                let (_, b) = v.as_object().unwrap().iter().next().unwrap();
                let items = b["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| (i["name"].as_str().unwrap().to_string(), i["value"].clone()))
                    .collect();
                (b["device"].as_str().unwrap().to_string(), b["name"].as_str().unwrap().to_string(), items)
            })
            .collect()
    }

    fn rig() -> PropertyStore {
        let mut s = PropertyStore::default();
        apply(&mut s, &[
            info("Cam", interface::CCD),
            connection("Cam", false),
            info("Mount", interface::MOUNT | interface::GUIDER),
            connection("Mount", false),
            info("Dome", interface::DOME),
            connection("Dome", false),
        ]);
        s
    }

    fn with_imager(s: &mut PropertyStore, cam_item: &str, state: &str, on: bool) {
        apply(s, &[
            info("Imager Agent", interface::AGENT | interface::CCD),
            number("Imager Agent", "AGENT_IMAGER_BATCH"),
            list("Imager Agent", "FILTER_CCD_LIST", state, &[(cam_item, on)]),
        ]);
    }

    #[test]
    fn no_agents_connects_everything_after_discovery() {
        let s = rig();
        let roles = Roles::derive(&s, &AgentSet::default());
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        assert!(writes(&a.plan(&s, &roles, t0)).is_empty(), "discovery phase holds connects");
        let p = a.plan(&s, &roles, t0 + Duration::from_secs(6));
        let w = writes(&p);
        let devices: Vec<&str> = w.iter().map(|(d, _, _)| d.as_str()).collect();
        assert_eq!(devices, ["Cam", "Mount", "Dome"]);
        assert!(w.iter().all(|(_, n, i)| n == "CONNECTION" && i[0].0 == "CONNECTED"));
        // Not resent within the resend window, and each retry waits longer.
        assert!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(7))).is_empty());
        assert!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(12))).is_empty(), "second try waits 10 s");
        assert_eq!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(17))).len(), 3);
        assert!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(30))).is_empty(), "third try waits 20 s");
        assert_eq!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(38))).len(), 3);
    }

    #[test]
    fn selects_the_camera_instead_of_connecting_it() {
        let mut s = rig();
        with_imager(&mut s, "Cam", "Ok", false);
        let set = AgentSet::discover(&s);
        assert_eq!(set.imager.as_deref(), Some("Imager Agent"));
        let roles = Roles::derive(&s, &set);
        assert_eq!(roles.camera.as_deref(), Some("Cam"));
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        let w = writes(&a.plan(&s, &roles, t0 + Duration::from_secs(6)));
        let select: Vec<_> = w.iter().filter(|(d, n, _)| d == "Imager Agent" && n == "FILTER_CCD_LIST").collect();
        assert_eq!(select.len(), 1);
        assert_eq!(select[0].2[0], ("Cam".to_string(), json!(true)));
        assert!(!w.iter().any(|(d, n, _)| d == "Cam" && n == "CONNECTION"), "claimed: not connected directly");
        assert!(w.iter().any(|(d, n, _)| d == "Dome" && n == "CONNECTION"), "unclaimed: connected directly");
        // Backoff: no second select right away.
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(7))).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        // The list went Busy (connecting): nothing; then Ok with Cam on: nothing.
        apply(&mut s, &[list("Imager Agent", "FILTER_CCD_LIST", "Busy", &[("Cam", true)])]);
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(60))).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        apply(&mut s, &[list("Imager Agent", "FILTER_CCD_LIST", "Ok", &[("Cam", true)])]);
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(120))).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        assert!(a.records.is_empty());
    }

    #[test]
    fn connected_camera_is_released_before_selection() {
        let mut s = rig();
        apply(&mut s, &[connection("Cam", true)]);
        with_imager(&mut s, "Cam", "Ok", false);
        let set = AgentSet::discover(&s);
        let roles = Roles::derive(&s, &set);
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        let w = writes(&a.plan(&s, &roles, t0 + Duration::from_secs(6)));
        assert!(w.iter().any(|(d, n, i)| d == "Cam" && n == "CONNECTION" && i[0].0 == "DISCONNECTED"));
        assert!(!w.iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        apply(&mut s, &[connection("Cam", false)]);
        let w = writes(&a.plan(&s, &roles, t0 + Duration::from_secs(12)));
        assert!(w.iter().any(|(_, n, i)| n == "FILTER_CCD_LIST" && i[0].0 == "Cam"));
    }

    #[test]
    fn refusal_backs_off_and_tells_the_browser() {
        let mut s = rig();
        with_imager(&mut s, "Cam", "Ok", false);
        let set = AgentSet::discover(&s);
        let roles = Roles::derive(&s, &set);
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        let t1 = t0 + Duration::from_secs(6);
        assert!(writes(&a.plan(&s, &roles, t1)).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        s.apply(parse_frame(r#"{"setSwitchVector":{"device":"Imager Agent","name":"FILTER_CCD_LIST","state":"Alert","message":"'Cam' is busy or in use and can not be selected.","items":[{"name":"NONE","value":true},{"name":"Cam","value":false}]}}"#).unwrap());
        let c = Change::Updated { device: "Imager Agent".into(), name: "FILTER_CCD_LIST".into() };
        let msgs = a.on_change(&c, &s, t1 + Duration::from_secs(1));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("busy or in use"));
        // attempts: 1 (sent) + 1 (refused) → 6 s backoff from the refusal.
        assert!(!writes(&a.plan(&s, &roles, t1 + Duration::from_secs(4))).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
        assert!(writes(&a.plan(&s, &roles, t1 + Duration::from_secs(20))).iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
    }

    #[test]
    fn busy_agent_is_left_alone() {
        let mut s = rig();
        with_imager(&mut s, "Cam", "Ok", false);
        apply(&mut s, &[
            list("Imager Agent", "AGENT_START_PROCESS", "Busy", &[("FOCUSING", true)]),
            r#"{"defSwitchVector":{"device":"Imager Agent","name":"AGENT_ABORT_PROCESS","perm":"rw","state":"Ok","items":[{"name":"ABORT","value":false}]}}"#.to_string(),
        ]);
        let set = AgentSet::discover(&s);
        let roles = Roles::derive(&s, &set);
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(6))).iter().any(|(d, _, _)| d == "Imager Agent"));
        // Stopping the profile aborts it first.
        a.set_active(false);
        let w = writes(&a.plan(&s, &roles, t0 + Duration::from_secs(7)));
        assert!(w.iter().any(|(d, n, i)| d == "Imager Agent" && n == "AGENT_ABORT_PROCESS" && i[0].0 == "ABORT"));
        assert!(!w.iter().any(|(_, n, _)| n == "FILTER_CCD_LIST"));
    }

    #[test]
    fn loads_missing_agents_once_and_marks_unavailable() {
        let mut s = rig();
        apply(&mut s, &[
            r#"{"defTextVector":{"device":"Server","name":"LOAD","perm":"rw","state":"Ok","items":[{"name":"DRIVER","value":""}]}}"#.to_string(),
            r#"{"defSwitchVector":{"device":"Server","name":"DRIVERS","perm":"rw","state":"Ok","rule":"AnyOfMany","items":[{"name":"indigo_agent_imager","value":false},{"name":"indigo_agent_mount","value":true},{"name":"indigo_ccd_simulator","value":true}]}}"#.to_string(),
        ]);
        let roles = Roles::derive(&s, &AgentSet::default());
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        let w = writes(&a.plan(&s, &roles, t0));
        let loads: Vec<_> = w.iter().filter(|(d, n, _)| d == "Server" && n == "LOAD").collect();
        // Mount is loaded (properties pending); the other three are loaded blind.
        assert_eq!(loads.len(), 3);
        assert_eq!(loads[0].2[0].1, json!("indigo_agent_imager"));
        assert!(!loads.iter().any(|(_, _, i)| i[0].1 == json!("indigo_agent_mount")));
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(1))).iter().any(|(_, n, _)| n == "LOAD"));
        // The server rejects one of them: that kind becomes unavailable.
        s.apply(parse_frame(r#"{"setTextVector":{"device":"Server","name":"LOAD","state":"Alert","message":"Driver indigo_agent_guider not found","items":[{"name":"DRIVER","value":"indigo_agent_guider"}]}}"#).unwrap());
        let msgs = a.on_change(&Change::Updated { device: "Server".into(), name: "LOAD".into() }, &s, t0);
        assert_eq!(msgs.len(), 1);
        assert!(a.unavailable(Kind::Guider) && !a.unavailable(Kind::Astrometry));
        // A load extends the discovery phase: still nothing connected at 6 s.
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(6))).iter().any(|(_, n, _)| n == "CONNECTION"));
        assert!(writes(&a.plan(&s, &roles, t0 + Duration::from_secs(11))).iter().any(|(_, n, _)| n == "CONNECTION"));
        assert!(a.missing_msg(Kind::Guider).contains("not installed"));
    }

    #[test]
    fn related_links_and_solver_init() {
        let mut s = rig();
        with_imager(&mut s, "Cam", "Ok", true);
        apply(&mut s, &[
            info("Astrometry Agent", interface::AGENT),
            number("Astrometry Agent", "AGENT_PLATESOLVER_WCS"),
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"FILTER_RELATED_AGENT_LIST","perm":"rw","state":"Ok","rule":"AnyOfMany","items":[{"name":"Imager Agent","value":true},{"name":"Mount Agent","value":false}]}}"#.to_string(),
            r#"{"defSwitchVector":{"device":"Astrometry Agent","name":"AGENT_PLATESOLVER_SOLVE_IMAGES","perm":"rw","state":"Ok","items":[{"name":"ENABLED","value":true},{"name":"DISABLED","value":false}]}}"#.to_string(),
            info("Mount Agent", interface::AGENT | interface::MOUNT),
            list("Mount Agent", "FILTER_MOUNT_LIST", "Ok", &[("Mount", true)]),
        ]);
        let set = AgentSet::discover(&s);
        assert_eq!(set.astrometry.as_deref(), Some("Astrometry Agent"));
        assert_eq!(set.mount.as_deref(), Some("Mount Agent"));
        let roles = Roles::derive(&s, &set);
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        let w = writes(&a.plan(&s, &roles, t0));
        let rel: Vec<_> = w.iter().filter(|(_, n, _)| n == "FILTER_RELATED_AGENT_LIST").collect();
        assert_eq!(rel.len(), 1);
        assert_eq!(rel[0].2, vec![("Mount Agent".to_string(), json!(true))]);
        assert!(w.iter().any(|(d, n, i)| d == "Astrometry Agent" && n == "AGENT_PLATESOLVER_SOLVE_IMAGES" && i[0].0 == "DISABLED"));
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(1))).iter().any(|(_, n, _)| n == "AGENT_PLATESOLVER_SOLVE_IMAGES"));
    }

    #[test]
    fn stop_releases_lists_and_disconnects_the_rest() {
        let mut s = rig();
        apply(&mut s, &[connection("Cam", true), connection("Dome", true)]);
        with_imager(&mut s, "Cam", "Ok", true);
        let set = AgentSet::discover(&s);
        let roles = Roles::derive(&s, &set);
        let t0 = Instant::now();
        let mut a = Agents::new(t0);
        assert!(!writes(&a.plan(&s, &roles, t0 + Duration::from_secs(6))).iter().any(|(d, _, _)| d == "Cam" || d == "Dome"));
        a.set_active(false);
        let w = writes(&a.plan(&s, &roles, t0 + Duration::from_secs(7)));
        assert!(w.iter().any(|(d, n, i)| d == "Imager Agent" && n == "FILTER_CCD_LIST" && i[0].0 == NONE));
        assert!(w.iter().any(|(d, n, i)| d == "Dome" && n == "CONNECTION" && i[0].0 == "DISCONNECTED"));
        assert!(w.iter().any(|(d, n, i)| d == "Cam" && n == "CONNECTION" && i[0].0 == "DISCONNECTED"));
        assert!(!w.iter().any(|(_, n, i)| n == "CONNECTION" && i[0].0 == "CONNECTED"));
    }
}
