//! The INDI AO, PHD2's `StepGuiderSxAoINDI` (the SX AO driver's
//! properties): steps through `AO_NS {AO_N, AO_S}` and `AO_WE {AO_E, AO_W}`,
//! fire-and-forget; recentring through `AO_CENTER {CENTER, UNJAM}`; the
//! end-of-travel lights of `AT_LIMIT`; and its own ST-4 port through
//! `TELESCOPE_TIMED_GUIDE_*`, the "On AO" mount's guide output, which PHD2
//! sends without waiting for any answer.
//!
//! Quirks kept: the class has no `updateProperty`, so it only reads
//! `CONNECTION` when the property is defined. Connecting a device whose
//! driver is not connected yet sends `CONNECT` and then waits the 30 s out
//! and fails; the next attempt, which finds `CONNECT` On, succeeds at once.
//! Nothing but the server going and the device going disconnects it, and
//! neither raises an alert. Its name (`Mount::Name`) is empty: the class
//! sets a private `m_Name` that hides the base class's.
//!
//! Ported from PHD2 src/stepguider_sxao_indi.cpp, src/scope_onboard_st4.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::time::Instant;

use super::client::{self, IndiClient, Link, TIMED_GUIDE_NS, TIMED_GUIDE_WE, WaitError};
use super::xml::{Kind, PropState, Value, scan_double};
use crate::engine::calibration::ProfileAccess;
use crate::engine::mount::GuideOutput;
use crate::engine::stepguider::{StepGuiderDevice, StepResult};
use crate::engine::traits::{CancelToken, Dir, MoveResult};

/// `/indi/INDIao`'s default: no AO chosen yet.
pub const UNSET_AO: &str = "INDI SXV-AO-LF";
/// The AO's travel, in its own profile key.
pub const MAX_STEPS_KEY: &str = "/stepguider/sxao/MaxSteps";
/// `DefaultMaxSteps`.
pub const DEFAULT_MAX_STEPS: i32 = 45;

/// The AO's profile keys (`StepGuiderSxAoINDI`'s constructor).
#[derive(Clone, Debug, PartialEq)]
pub struct AoOptions {
    /// `/indi/INDIao`.
    pub device: String,
    /// [`MAX_STEPS_KEY`].
    pub max_steps: i32,
    /// `/indi/PulseMinWait`.
    pub pulse_min_wait: bool,
}

impl AoOptions {
    pub fn from_profile(p: &impl ProfileAccess) -> Self {
        AoOptions {
            device: p.get_str("/indi/INDIao", UNSET_AO),
            max_steps: p.get_i32(MAX_STEPS_KEY, DEFAULT_MAX_STEPS),
            pulse_min_wait: client::pulse_min_wait(p),
        }
    }
}

pub struct IndiAo {
    client: IndiClient,
    device: String,
    attached: Option<u64>,
    max_steps: i32,
    profile_write: Option<(&'static str, i32)>,
    pulse_min_wait: bool,
    pulse_min_wait_logged: bool,
    debug: Vec<String>,
    alerts: Vec<String>,
}

impl IndiAo {
    pub fn new(client: IndiClient, opts: &AoOptions) -> Self {
        IndiAo {
            client,
            device: opts.device.clone(),
            attached: None,
            max_steps: opts.max_steps,
            profile_write: None,
            pulse_min_wait: opts.pulse_min_wait,
            pulse_min_wait_logged: false,
            debug: Vec::new(),
            alerts: Vec::new(),
        }
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// `Connect`: connected when `CONNECTION` is On as the AO first sees it
    /// (see the module's quirks).
    pub fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        if self.attached.is_some() {
            return Ok(());
        }
        if self.device.is_empty() || self.device == UNSET_AO {
            return Err("No INDI AO is selected".into());
        }
        let (host, port) = (self.client.host().to_owned(), self.client.port());
        self.debug.push(format!("Connecting to INDI server {host} on port {port}, device {}", self.device));
        self.debug.push(format!("Waiting for 30s for [{}] to connect...", self.device));
        let generation =
            self.client.wait_up(cancel).ok_or_else(|| format!("Cannot connect to INDI server {host}:{port}"))?;
        self.client.watch(&self.device);
        let start = Instant::now();
        let device = self.device.clone();
        let first = self.client.wait(client::DEVICE_CONNECT_TIMEOUT, Some(cancel), |l| {
            if !l.is_up() || l.generation() != generation {
                return Some(None);
            }
            l.device_connected(&device).map(Some)
        });
        match first {
            Ok(Some(true)) => {
                self.attached = Some(generation);
                self.check_state(cancel);
                return Ok(());
            }
            Ok(None) => return Err("INDI server disconnected".into()),
            Err(WaitError::Cancelled) => return Err("Connection cancelled".into()),
            Err(WaitError::Timeout) => return Err(format!("Device {device} did not connect within 30 s")),
            Ok(Some(false)) => {}
        }
        // `newProperty(DRIVER_INFO)` connects the device, whose answer this
        // class never reads: the poll runs out.
        let rest = || client::DEVICE_CONNECT_TIMEOUT.saturating_sub(start.elapsed());
        let r = self.client.wait(rest(), Some(cancel), |l| {
            l.store.property(&device, "DRIVER_INFO").map(|_| self.client.set_driver_connection(l, &device, true))
        });
        if r == Err(WaitError::Cancelled) || client::sleep_ms(rest().as_millis() as u64, cancel) {
            return Err("Connection cancelled".into());
        }
        Err(format!("Device {device} did not connect within 30 s"))
    }

    /// `CheckState`, once the AO's properties are in: the driver must be
    /// 1.12 or later, and a firmware "V000" needs flashing. PHD2 shows
    /// these in message boxes; they come back as alerts.
    fn check_state(&mut self, cancel: &CancelToken) {
        let device = &self.device;
        let needed = |l: &mut Link| {
            let has = |p: &str, k: Kind| l.store.property(device, p).is_some_and(|x| x.kind == k);
            has("AO_NS", Kind::Number) && has("AO_WE", Kind::Number) && has("AO_CENTER", Kind::Switch)
        };
        if self.client.wait(client::READY_GRACE, Some(cancel), |l| needed(l).then_some(())).is_err() {
            return;
        }
        let (driver, version, firmware) = {
            let link = self.client.lock();
            let info = link.store.property(device, "DRIVER_INFO");
            let text = |m: &str| info.and_then(|p| p.text(m)).unwrap_or_default().to_owned();
            let firmware = link.store.property(device, "INFO").and_then(|p| p.text("FIRMWARE")).map(str::to_owned);
            (text("DRIVER_EXEC"), text("DRIVER_VERSION"), firmware)
        };
        // `atof`.
        if scan_double(&version).unwrap_or(0.0) < 1.12 {
            self.alerts.push(format!(
                "We need at least INDI driver {driver} version 1.12 to get the Firmware version and the Limit switch \
                 states."
            ));
            return;
        }
        // `FirmwareVersion`: the three digits after the first character.
        let Some(firmware) = firmware else { return };
        let digits = firmware.as_bytes().get(1..4).filter(|d| d.iter().all(u8::is_ascii_digit));
        let Some(version) = digits.map(|d| d.iter().fold(0u32, |v, c| v * 10 + (c - b'0') as u32)) else {
            return;
        };
        self.debug.push(format!("StepGuiderSxAoINDI::Firmwareversion {version}"));
        if version == 0 {
            self.alerts.push(format!(
                "This AO device has firmware version {version:03} which means it needs to be flashed.\nThe SXV-AO \
                 Utility v104 or newer, available at http://www.sxccd.com/drivers-downloads,\ncontains the firmware."
            ));
            return;
        }
        self.debug.push(format!("StepGuiderSxAoINDI::CheckState is ready, firmware {version:03}"));
    }

    /// `Disconnect`.
    pub fn disconnect(&mut self) {
        if self.attached.take().is_some() {
            self.debug.push("StepGuiderSxAoINDI::Disconnect".into());
        }
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected_in(&self.client.lock())
    }

    fn is_connected_in(&self, link: &Link) -> bool {
        self.attached
            .is_some_and(|g| link.is_up() && link.generation() == g && link.store.device(&self.device).is_some())
    }

    /// A server or device drop disconnects the AO, without an alert.
    pub fn check_connection(&mut self) {
        if self.attached.is_some() && !self.is_connected() {
            self.disconnect();
        }
    }

    /// PHD2's debug-log lines since the last call.
    pub fn take_debug(&mut self) -> Vec<String> {
        std::mem::take(&mut self.debug)
    }

    /// The firmware and driver-version message boxes.
    pub fn take_alerts(&mut self) -> Vec<String> {
        std::mem::take(&mut self.alerts)
    }

    /// The profile write of the last [`StepGuiderDevice::set_max_position`].
    pub fn take_profile_write(&mut self) -> Option<(&'static str, i32)> {
        self.profile_write.take()
    }

    /// `IsAtLimit`: the direction's `AT_LIMIT` light is Alert; `None`
    /// before the lights are known. Nothing in PHD2 calls it.
    pub fn is_at_limit(&mut self, dir: Dir) -> Option<bool> {
        let (member, word) = match dir {
            Dir::North => ("AT_LIMIT_N", "North"),
            Dir::South => ("AT_LIMIT_S", "South"),
            Dir::East => ("AT_LIMIT_E", "East"),
            Dir::West => ("AT_LIMIT_W", "West"),
        };
        let state = {
            let link = self.client.lock();
            let lights = link.store.property(&self.device, "AT_LIMIT").filter(|p| p.kind == Kind::Light);
            lights.map(|p| match p.member(member).map(|m| &m.value) {
                Some(Value::Light(s)) => *s == PropState::Alert,
                _ => false,
            })
        };
        match state {
            Some(_) => self.debug.push(format!("StepGuiderSxAoINDI::IsAtLimit {word}")),
            None => self.debug.push("StepGuiderSxAoINDI::IsAtLimit called before we received any ao_limit".into()),
        }
        state
    }
}

impl StepGuiderDevice for IndiAo {
    /// Quirk kept: empty (see the module).
    fn name(&self) -> String {
        String::new()
    }

    fn step(&mut self, dir: Dir, steps: u32) -> StepResult {
        let (prop, member, other) = match dir {
            Dir::North => ("AO_NS", "AO_N", "AO_S"),
            Dir::South => ("AO_NS", "AO_S", "AO_N"),
            Dir::East => ("AO_WE", "AO_E", "AO_W"),
            Dir::West => ("AO_WE", "AO_W", "AO_E"),
        };
        let link = self.client.lock();
        let has = |p: &str| link.store.property(&self.device, p).is_some_and(|x| x.kind == Kind::Number);
        if !self.is_connected_in(&link) || !has("AO_NS") || !has("AO_WE") {
            return StepResult::Error;
        }
        self.client.send_number(&link, &self.device, prop, &[(member, steps as f64), (other, 0.0)]);
        StepResult::Ok
    }

    fn max_position(&self, _dir: Dir) -> i32 {
        self.max_steps
    }

    fn set_max_position(&mut self, steps: i32) -> bool {
        self.debug.push(format!("StepGuiderSxAoINDI: setting max steps = {steps}"));
        self.max_steps = steps;
        self.profile_write = Some((MAX_STEPS_KEY, steps));
        false
    }

    /// `Center`: `CENTER` On. A failed centring is not detected (PHD2's
    /// TODO), so the position is zeroed anyway.
    fn center(&mut self) -> bool {
        self.debug.push("StepGuiderSxAoINDI::Center".into());
        let link = self.client.lock();
        let has = link.store.property(&self.device, "AO_CENTER").is_some_and(|p| p.kind == Kind::Switch);
        if !has || !self.is_connected_in(&link) {
            return false;
        }
        self.client.send_switch(&link, &self.device, "AO_CENTER", &[("CENTER", true), ("UNJAM", false)]);
        true
    }
}

/// The "On AO" mount (`ScopeOnboardST4::Guide` over `ST4PulseGuideScope`):
/// the pulse is sent and the call returns. With `/indi/PulseMinWait` (the
/// PLAN's addition) it sleeps the pulse's length first.
impl GuideOutput for IndiAo {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        if !self.is_connected() {
            self.debug.push(
                "Error thrown from scope_onboard_st4.cpp:133->Attempt to Guide On Camera mount when camera is not \
                 connected"
                    .into(),
            );
            return MoveResult::Error;
        }
        let (prop, pulsed, other) = client::timed_guide_members(dir);
        {
            let link = self.client.lock();
            let has = |p: &str| link.store.property(&self.device, p).is_some_and(|x| x.kind == Kind::Number);
            if !has(TIMED_GUIDE_NS) || !has(TIMED_GUIDE_WE) {
                return MoveResult::Error;
            }
            self.client.send_number(&link, &self.device, prop, &[(pulsed, ms as f64), (other, 0.0)]);
        }
        if self.pulse_min_wait {
            if !self.pulse_min_wait_logged {
                self.pulse_min_wait_logged = true;
                self.debug.push(format!(
                    "INDI AO: waiting out the {ms} ms pulse the AO's ST-4 port answers nothing to (PulseMinWait, \
                     logged once)"
                ));
            }
            client::sleep_ms(ms as u64, cancel);
        }
        MoveResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::indi::fake::{self, Fake, Profile};

    const DEV: &str = "SX AO";

    /// The SX AO driver: its version, its firmware, and whether the device
    /// is connected when the client first asks.
    #[derive(Clone, Copy)]
    struct Sx {
        version: &'static str,
        firmware: &'static str,
        connected: bool,
    }

    const SXAO: Sx = Sx { version: "1.15", firmware: "V104", connected: false };

    fn ao_props(cfg: Sx) -> String {
        let mut s = String::new();
        for (p, a, b) in
            [(TIMED_GUIDE_NS, "TIMED_GUIDE_N", "TIMED_GUIDE_S"), (TIMED_GUIDE_WE, "TIMED_GUIDE_W", "TIMED_GUIDE_E")]
        {
            s += &fake::def_number(DEV, p, "Idle", &[(a, 0.0, 0.0, 6e4), (b, 0.0, 0.0, 6e4)]);
        }
        s += &fake::def_number(DEV, "AO_NS", "Idle", &[("AO_N", 0.0, 0.0, 80.0), ("AO_S", 0.0, 0.0, 80.0)]);
        s += &fake::def_number(DEV, "AO_WE", "Idle", &[("AO_E", 0.0, 0.0, 80.0), ("AO_W", 0.0, 0.0, 80.0)]);
        s += &fake::def_switch(DEV, "AO_CENTER", "OneOfMany", &[("CENTER", false), ("UNJAM", false)]);
        s += &fake::def_text(DEV, "INFO", &[("FIRMWARE", cfg.firmware)]);
        let lights = [("AT_LIMIT_N", "Idle"), ("AT_LIMIT_S", "Idle"), ("AT_LIMIT_E", "Alert"), ("AT_LIMIT_W", "Idle")];
        s + &fake::def_light(DEV, "AT_LIMIT", &lights)
    }

    fn sx(cfg: Sx) -> Fake {
        Fake::start(move |e, peer| match (e.name.as_str(), e.attr("name").unwrap_or_default()) {
            ("getProperties", _) => {
                let on = cfg.connected;
                let mut s = fake::def_switch(DEV, "CONNECTION", "OneOfMany", &[("CONNECT", on), ("DISCONNECT", !on)]);
                let info =
                    [("DRIVER_EXEC", "indi_sx_ao"), ("DRIVER_VERSION", cfg.version), ("DRIVER_INTERFACE", "260")];
                s += &fake::def_text(DEV, "DRIVER_INFO", &info);
                if on {
                    s += &ao_props(cfg);
                }
                peer.send(&s);
            }
            ("newSwitchVector", "CONNECTION") => {
                peer.send(&fake::connection_reply(e).unwrap());
                peer.send(&ao_props(cfg));
            }
            _ => {}
        })
    }

    fn opts() -> AoOptions {
        AoOptions { device: DEV.into(), max_steps: DEFAULT_MAX_STEPS, pulse_min_wait: true }
    }

    fn sent(fake: &Fake) -> Vec<String> {
        fake.received().iter().filter(|e| e.name != "getProperties").map(fake::summary).collect()
    }

    #[test]
    fn options_from_the_profile() {
        let mut p = Profile::default();
        let unset = AoOptions { device: "INDI SXV-AO-LF".into(), ..opts() };
        assert_eq!(AoOptions::from_profile(&p), unset);
        p.0.insert(MAX_STEPS_KEY.into(), "30".into());
        assert_eq!(AoOptions::from_profile(&p).max_steps, 30);
        let mut ao = IndiAo::new(IndiClient::start("127.0.0.1", 9, &[]), &unset);
        assert_eq!(ao.connect(&CancelToken::new()), Err("No INDI AO is selected".into()));
        ao.client.shutdown();
    }

    #[test]
    fn it_connects_only_when_connect_is_on_as_first_seen() {
        let fake = sx(SXAO);
        let mut ao = IndiAo::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts());
        // The device is off: CONNECT is sent, the driver connects, and the
        // AO, which never reads the update, waits until cancelled.
        let cancel = CancelToken::new();
        let stop = cancel.clone();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            stop.cancel();
        });
        assert_eq!(ao.connect(&cancel), Err("Connection cancelled".into()));
        stopper.join().unwrap();
        assert!(!ao.is_connected());
        assert_eq!(sent(&fake), ["newSwitchVector SX AO.CONNECTION CONNECT=On"]);
        // The next attempt finds it On.
        ao.connect(&CancelToken::new()).unwrap();
        assert!(ao.is_connected());
        assert_eq!(ao.name(), "");
        let port = fake.port;
        assert_eq!(
            ao.take_debug(),
            [
                format!("Connecting to INDI server 127.0.0.1 on port {port}, device SX AO"),
                "Waiting for 30s for [SX AO] to connect...".into(),
                format!("Connecting to INDI server 127.0.0.1 on port {port}, device SX AO"),
                "Waiting for 30s for [SX AO] to connect...".into(),
                "StepGuiderSxAoINDI::Firmwareversion 104".into(),
                "StepGuiderSxAoINDI::CheckState is ready, firmware 104".into(),
            ]
        );
        assert!(ao.take_alerts().is_empty());
        // The server goes: disconnected, without an alert.
        fake.drop_conns();
        ao.client.wait(Duration::from_secs(3), None, |l| (l.generation() == 2).then_some(())).unwrap();
        assert!(!ao.is_connected());
        ao.check_connection();
        assert_eq!(ao.take_debug(), ["StepGuiderSxAoINDI::Disconnect"]);
        ao.client.shutdown();
    }

    #[test]
    fn steps_centring_limits_and_the_st4_port() {
        let fake = sx(Sx { connected: true, ..SXAO });
        let mut ao = IndiAo::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts());
        ao.connect(&CancelToken::new()).unwrap();
        assert_eq!(ao.step(Dir::East, 3), StepResult::Ok);
        assert_eq!(ao.step(Dir::North, 2), StepResult::Ok);
        assert!(ao.center());
        assert_eq!((ao.is_at_limit(Dir::East), ao.is_at_limit(Dir::North)), (Some(true), Some(false)));
        assert_eq!(ao.max_position(Dir::West), 45);
        assert!(!ao.set_max_position(30));
        assert_eq!((ao.max_position(Dir::South), ao.take_profile_write()), (30, Some((MAX_STEPS_KEY, 30))));
        // The "On AO" pulse: sent, then waited out (PulseMinWait).
        let cancel = CancelToken::new();
        let t = std::time::Instant::now();
        assert_eq!(ao.guide(Dir::West, 120, &cancel), MoveResult::Ok);
        assert!(t.elapsed() >= Duration::from_millis(120));
        ao.pulse_min_wait = false;
        let t = std::time::Instant::now();
        assert_eq!(ao.guide(Dir::South, 1000, &cancel), MoveResult::Ok);
        assert!(t.elapsed() < Duration::from_millis(100));
        assert!(fake.wait_for(|r| r.len() == 6));
        assert_eq!(
            sent(&fake),
            [
                "newNumberVector SX AO.AO_WE AO_E=3 AO_W=0",
                "newNumberVector SX AO.AO_NS AO_N=2 AO_S=0",
                "newSwitchVector SX AO.AO_CENTER CENTER=On",
                "newNumberVector SX AO.TELESCOPE_TIMED_GUIDE_WE TIMED_GUIDE_W=120 TIMED_GUIDE_E=0",
                "newNumberVector SX AO.TELESCOPE_TIMED_GUIDE_NS TIMED_GUIDE_N=0 TIMED_GUIDE_S=1000",
            ]
        );
        let debug = ao.take_debug();
        assert_eq!(
            debug[4..9],
            [
                "StepGuiderSxAoINDI::Center",
                "StepGuiderSxAoINDI::IsAtLimit East",
                "StepGuiderSxAoINDI::IsAtLimit North",
                "StepGuiderSxAoINDI: setting max steps = 30",
                "INDI AO: waiting out the 120 ms pulse the AO's ST-4 port answers nothing to (PulseMinWait, logged once)",
            ]
        );
        // Disconnected: nothing moves.
        ao.disconnect();
        assert_eq!(ao.step(Dir::East, 1), StepResult::Error);
        assert!(!ao.center());
        assert_eq!(ao.guide(Dir::East, 10, &cancel), MoveResult::Error);
        ao.client.shutdown();
    }

    #[test]
    fn an_old_driver_or_an_unflashed_ao_raises_an_alert() {
        let fake = sx(Sx { connected: true, version: "1.10", ..SXAO });
        let mut ao = IndiAo::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts());
        ao.connect(&CancelToken::new()).unwrap();
        assert_eq!(
            ao.take_alerts(),
            ["We need at least INDI driver indi_sx_ao version 1.12 to get the Firmware version and the Limit switch \
              states."]
        );
        ao.client.shutdown();

        let fake = sx(Sx { connected: true, firmware: "V000", ..SXAO });
        let mut ao = IndiAo::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), &opts());
        ao.connect(&CancelToken::new()).unwrap();
        assert_eq!(
            ao.take_alerts(),
            [
                "This AO device has firmware version 000 which means it needs to be flashed.\nThe SXV-AO Utility v104 or \
              newer, available at http://www.sxccd.com/drivers-downloads,\ncontains the firmware."
            ]
        );
        ao.client.shutdown();
    }
}
