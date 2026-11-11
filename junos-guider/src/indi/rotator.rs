//! The INDI rotator, PHD2's `RotatorINDI`: the angle of
//! `ABS_ROTATOR_ANGLE.ANGLE`, read only.
//!
//! Quirks kept: the rotator counts as connected as soon as the server is
//! (`serverConnected` calls `Rotator::Connect`), and it never connects its
//! driver, so its angle stays unknown until something else does. The angle
//! goes through a `float`. It drops when the server goes ("INDI server
//! disconnected"), when its device goes (no alert), or when `CONNECT`
//! turns Off once it had its angle ("INDI rotator was disconnected").
//!
//! Ported from PHD2 src/rotator_indi.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::cell::Cell;

use super::client::{IndiClient, Link};
use super::xml::Kind;
use crate::engine::calibration::ROTATOR_POSITION_UNKNOWN;
use crate::engine::traits::CancelToken;

/// The rotator's profile key, and its default: no rotator chosen yet.
pub const DEVICE_KEY: &str = "/indi/INDIrotator";
pub const UNSET_ROTATOR: &str = "INDI Rotator";

pub struct IndiRotator {
    client: IndiClient,
    device: String,
    attached: Option<u64>,
    /// The device was seen since the rotator connected: a later absence is
    /// `removeDevice`.
    seen: Cell<bool>,
    /// The sequence number of `CONNECTION` when the rotator first saw it:
    /// only a later update turning it Off drops the rotator, as only
    /// `updateProperty` looks at it.
    conn_seq: Cell<Option<u64>>,
    debug: Vec<String>,
}

impl IndiRotator {
    pub fn new(client: IndiClient, device: &str) -> Self {
        IndiRotator {
            client,
            device: device.to_owned(),
            attached: None,
            seen: Cell::new(false),
            conn_seq: Cell::new(None),
            debug: Vec::new(),
        }
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// `m_Name`.
    pub fn name(&self) -> String {
        format!("INDI Rotator [{}]", self.device)
    }

    /// `Connect`: connected once the server is.
    pub fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        if self.attached.is_some() {
            return Ok(());
        }
        if self.device.is_empty() || self.device == UNSET_ROTATOR {
            return Err("No INDI rotator is selected".into());
        }
        self.debug.push(format!("Waiting for 30s for [{}] to connect...", self.device));
        let (host, port) = (self.client.host(), self.client.port());
        let generation =
            self.client.wait_up(cancel).ok_or_else(|| format!("Cannot connect to INDI server {host}:{port}"))?;
        self.client.watch(&self.device);
        self.debug.push("INDI Rotator connection succeeded".into());
        self.attached = Some(generation);
        self.seen.set(false);
        self.conn_seq.set(None);
        Ok(())
    }

    /// `Disconnect`.
    pub fn disconnect(&mut self) {
        self.attached = None;
    }

    pub fn is_connected(&self) -> bool {
        self.is_connected_in(&self.client.lock())
    }

    /// The drop PHD2 notices on its INDI thread, noticed here: the rotator
    /// is disconnected, and the alert PHD2 raises for it, if any.
    pub fn check_connection(&mut self) -> Option<String> {
        let alert = self.lost(&self.client.lock())?;
        self.disconnect();
        alert.map(str::to_owned)
    }

    /// PHD2's debug-log lines since the last call.
    pub fn take_debug(&mut self) -> Vec<String> {
        std::mem::take(&mut self.debug)
    }

    /// `Position`, degrees: [`ROTATOR_POSITION_UNKNOWN`] when disconnected
    /// or before the driver gives its angle.
    pub fn position(&self) -> f64 {
        let link = self.client.lock();
        let angle = link.store.property(&self.device, "ABS_ROTATOR_ANGLE").filter(|p| p.kind == Kind::Number);
        match angle.and_then(|p| p.number("ANGLE")).filter(|_| self.is_connected_in(&link)) {
            // `float m_angle`.
            Some(a) => a as f32 as f64,
            None => ROTATOR_POSITION_UNKNOWN,
        }
    }

    fn is_connected_in(&self, link: &Link) -> bool {
        self.attached.is_some() && self.lost(link).is_none()
    }

    /// Whether the rotator dropped, and the alert PHD2 raises for it.
    fn lost(&self, link: &Link) -> Option<Option<&'static str>> {
        let generation = self.attached?;
        if !link.is_up() || link.generation() != generation {
            return Some(Some("INDI server disconnected"));
        }
        let Some(device) = link.store.device(&self.device) else {
            return self.seen.get().then_some(None);
        };
        self.seen.set(true);
        let conn = device.property("CONNECTION").filter(|p| p.kind == Kind::Switch);
        let first = *self.conn_seq.get().get_or_insert(conn.map_or(u64::MAX, |p| p.updated));
        self.conn_seq.set(conn.map(|_| first));
        let updated_off = conn.is_some_and(|p| p.updated > first && p.switch("CONNECT") == Some(false));
        // `m_ready`: the angle property.
        let ready = device.property("ABS_ROTATOR_ANGLE").is_some_and(|p| p.kind == Kind::Number);
        (ready && updated_off).then_some(Some("INDI rotator was disconnected"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::indi::fake::{self, Fake};

    const DEV: &str = "Rotator Simulator";

    fn rotator() -> Fake {
        Fake::start(|e, peer| {
            if e.name == "getProperties" {
                peer.send(&fake::driver(DEV, 0x1000, false));
            }
        })
    }

    #[test]
    fn the_angle_of_a_driver_someone_else_connects() {
        let fake = rotator();
        let mut rot = IndiRotator::new(IndiClient::start("127.0.0.1", fake.port, &[DEV]), DEV);
        assert_eq!(rot.name(), "INDI Rotator [Rotator Simulator]");
        // Connected with the server, the driver still off: no angle.
        rot.connect(&CancelToken::new()).unwrap();
        assert!(rot.is_connected());
        rot.client.wait(Duration::from_secs(2), None, |l| l.store.device(DEV).map(|_| ())).unwrap();
        assert_eq!(rot.position(), ROTATOR_POSITION_UNKNOWN);
        // Someone connects the driver, which defines its angle.
        let on = fake::set_switch(DEV, "CONNECTION", "Ok", &[("CONNECT", true), ("DISCONNECT", false)]);
        fake.push(&(on + &fake::def_number(DEV, "ABS_ROTATOR_ANGLE", "Ok", &[("ANGLE", 123.456789, 0.0, 360.0)])));
        let has = |l: &mut Link| l.store.property(DEV, "ABS_ROTATOR_ANGLE").map(|_| ());
        rot.client.wait(Duration::from_secs(2), None, has).unwrap();
        assert_eq!(rot.position(), 123.456787109375);
        // The rotator sends nothing at all.
        assert!(fake.received().iter().all(|e| e.name == "getProperties"));
        assert_eq!(
            rot.take_debug(),
            ["Waiting for 30s for [Rotator Simulator] to connect...", "INDI Rotator connection succeeded"]
        );

        // The driver switches off once the angle is known.
        fake.push(&fake::set_switch(DEV, "CONNECTION", "Ok", &[("CONNECT", false), ("DISCONNECT", true)]));
        let off = |l: &mut Link| (l.device_connected(DEV) == Some(false)).then_some(());
        rot.client.wait(Duration::from_secs(2), None, off).unwrap();
        assert_eq!(rot.position(), ROTATOR_POSITION_UNKNOWN);
        assert_eq!(rot.check_connection().as_deref(), Some("INDI rotator was disconnected"));
        assert_eq!(rot.check_connection(), None);

        // Connected again, the driver still off: connected (only an update
        // to Off drops it). Then the device goes: no alert.
        rot.connect(&CancelToken::new()).unwrap();
        assert!(rot.is_connected());
        assert_eq!(rot.position(), 123.456787109375);
        fake.push(&format!("<delProperty device='{DEV}'/>\n"));
        rot.client.wait(Duration::from_secs(2), None, |l| l.store.device(DEV).is_none().then_some(())).unwrap();
        assert_eq!(rot.check_connection(), None);
        assert!(!rot.is_connected());
        rot.connect(&CancelToken::new()).unwrap();
        fake.drop_conns();
        rot.client.wait(Duration::from_secs(3), None, |l| (l.generation() == 2).then_some(())).unwrap();
        assert_eq!(rot.check_connection().as_deref(), Some("INDI server disconnected"));
        rot.client.shutdown();
    }
}
