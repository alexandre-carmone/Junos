//! The equipment `set_connected` connects: PHD2's gear dialog. The devices
//! are built from the profile's choices ([`Choices`]), connected off the
//! engine thread in `OnButtonConnectAll`'s order (camera, AO, mount, aux
//! mount, rotator), and handed to the engine as a [`Gear`]. The daemon
//! keeps the same devices in a [`Rig`] on the engine thread, to disconnect
//! them, write the profile key a device owes and hear what they report.
//!
//! Each connect builds its devices anew, so the engine never waits on a
//! device being connected; the engine keeps its mounts' state across them
//! (it matches them by name).
//!
//! With `--sim` the rig is the gear simulator, on the real clock: its
//! camera, the "On Camera" mount pulsing through the camera with the
//! simulator's mount as the pointing source, and its AO when the profile
//! chooses an AO (`/stepguider/LastMenuChoice` other than "None").
//!
//! Ported from PHD2 src/gear_dialog.cpp (OnButtonConnectAll and the per-device connect handlers, OnButtonDisconnectAll, ConnectAll's failure text) — BSD-3-Clause, see THIRD_PARTY.md

use crate::engine::calibration::ProfileAccess;
use crate::engine::camera::GuideCamera;
use crate::engine::traits::CancelToken;
use crate::engine::{Engine, Gear, ScopeGear};
use crate::indi::client::IndiClient;

use super::DaemonOptions;

/// A device as the daemon handles it, beside the trait the engine drives
/// it through.
trait Device: Send {
    fn connect(&mut self, cancel: &CancelToken) -> Result<(), String>;
    fn disconnect(&mut self);
    fn is_connected(&self) -> bool;
    /// Its debug-log lines since the last call.
    fn take_debug(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// A profile key the device set itself (an AO's travel).
    fn take_profile_write(&mut self) -> Option<(&'static str, i32)> {
        None
    }
}

/// What the connect tells the engine before handing it the gear: the
/// devices' debug-log lines and PHD2's status messages.
pub(super) enum Note {
    Debug(String),
    Status(String),
}

impl Note {
    pub(super) fn apply(self, e: &mut Engine) {
        match self {
            Note::Debug(l) => e.debug(&l),
            Note::Status(s) => e.status(&s),
        }
    }
}

/// The gear the profile chooses, read on the engine thread.
pub(super) struct Choices {
    /// `--sim`: the simulator's settings (`load_sim_params`) and whether
    /// it has its AO.
    #[cfg(any(test, feature = "sim"))]
    sim: Option<(crate::sim::SimParams, bool)>,
}

impl Choices {
    pub(super) fn read(e: &mut Engine, opts: &DaemonOptions) -> Choices {
        #[cfg(any(test, feature = "sim"))]
        let sim = opts.sim.then(|| {
            // The image scale at connect (`GetCameraPixelScale`), 1 when unknown.
            let scale = e.snapshot().pixel_scale;
            let p = e.store().profile();
            let ao = p.get_str("/stepguider/LastMenuChoice", "None") != "None";
            (crate::sim::SimParams::from_profile(&p, scale), ao)
        });
        #[cfg(not(any(test, feature = "sim")))]
        let _ = (e, opts);
        Choices {
            #[cfg(any(test, feature = "sim"))]
            sim,
        }
    }
}

/// A role's device and how to make the engine's side of it.
struct Slot<T> {
    dev: Box<dyn Device>,
    engine: Box<dyn Fn() -> T + Send>,
}

/// The devices of the last connect: PHD2's `m_pCamera`, `m_pScope`, …
#[derive(Default)]
pub(super) struct Rig {
    /// `None` is the "None" choice, or a camera that could not be built.
    camera: Option<Slot<Box<dyn GuideCamera>>>,
    scope: Option<Slot<ScopeGear>>,
    ao: Option<Slot<Box<dyn crate::engine::stepguider::StepGuiderDevice + Send>>>,
    client: Option<IndiClient>,
}

impl Rig {
    /// `OnButtonConnectAll`: the chosen devices built and connected in
    /// PHD2's order; the gear for the engine holds the connected ones.
    pub(super) fn connect(choices: &Choices, client: Option<IndiClient>, cancel: &CancelToken) -> (Rig, Gear, Vec<Note>) {
        let mut rig = Rig { client, ..Rig::default() };
        let mut notes = Vec::new();
        #[cfg(any(test, feature = "sim"))]
        if let Some((params, ao)) = &choices.sim {
            rig.build_sim(params.clone(), *ao);
        }
        #[cfg(not(any(test, feature = "sim")))]
        let _ = choices;

        let mut connect = |what: &str, dev: &mut Box<dyn Device>| {
            notes.push(Note::Status(format!("Connecting to {what} ...")));
            let r = dev.connect(cancel);
            notes.extend(dev.take_debug().into_iter().map(Note::Debug));
            if let Err(e) = r {
                notes.push(Note::Debug(e));
                notes.push(Note::Status(format!("{what} Connect Failed")));
            }
        };
        if let Some(s) = rig.camera.as_mut() {
            connect("Camera", &mut s.dev);
        }
        if let Some(s) = rig.ao.as_mut() {
            connect("AO", &mut s.dev);
        }
        if let Some(s) = rig.scope.as_mut() {
            connect("Mount", &mut s.dev);
        }
        let gear = rig.gear();
        (rig, gear, notes)
    }

    #[cfg(any(test, feature = "sim"))]
    fn build_sim(&mut self, params: crate::sim::SimParams, ao: bool) {
        use crate::sim::{SimCamera, SimTime};
        let cam = SimCamera::new(params, SimTime::Real);
        let c = cam.clone();
        self.camera = Some(Slot { dev: Box::new(cam.clone()), engine: Box::new(move || Box::new(c.clone())) });
        let mount = cam.mount();
        self.scope = Some(Slot {
            dev: Box::new(OnCamera { camera: cam.clone(), connected: false }),
            engine: Box::new(move || ScopeGear {
                name: "On Camera".into(),
                output: crate::engine::ScopeOutput::OnCamera,
                pointing: Some(Box::new(mount.clone())),
            }),
        });
        if ao {
            let a = cam.ao();
            self.ao = Some(Slot { dev: Box::new(a.clone()), engine: Box::new(move || Box::new(a.clone())) });
        }
    }

    /// The engine's side of the connected devices.
    fn gear(&self) -> Gear {
        fn connected<T>(s: &Option<Slot<T>>) -> Option<T> {
            s.as_ref().filter(|s| s.dev.is_connected()).map(|s| (s.engine)())
        }
        Gear { camera: connected(&self.camera), scope: connected(&self.scope), ao: connected(&self.ao), ..Gear::default() }
    }

    fn devices(&mut self) -> impl Iterator<Item = &mut Box<dyn Device>> {
        // `OnButtonDisconnectAll`'s order: mount, camera, AO.
        let scope = self.scope.as_mut().map(|s| &mut s.dev);
        let camera = self.camera.as_mut().map(|s| &mut s.dev);
        let ao = self.ao.as_mut().map(|s| &mut s.dev);
        [scope, camera, ao].into_iter().flatten()
    }

    /// `ConnectAll`'s first test: a camera, and every chosen device,
    /// connected.
    pub(super) fn all_connected(&mut self) -> bool {
        self.camera.is_some() && self.devices().all(|d| d.is_connected())
    }

    /// `DisconnectAll`'s first test, negated.
    pub(super) fn any_connected(&mut self) -> bool {
        self.devices().any(|d| d.is_connected())
    }

    /// `ConnectAll`'s failure list: " camera mount AO …", empty when all
    /// connected.
    pub(super) fn failures(&self) -> String {
        let mut fail = String::new();
        let down = |s: Option<&dyn Device>| s.is_some_and(|d| !d.is_connected());
        if self.camera.as_ref().is_none_or(|s| !s.dev.is_connected()) {
            fail += " camera";
        }
        if down(self.scope.as_ref().map(|s| &*s.dev)) {
            fail += " mount";
        }
        if down(self.ao.as_ref().map(|s| &*s.dev)) {
            fail += " AO";
        }
        fail
    }

    /// The INDI connection the devices share, for the next connect.
    pub(super) fn client(&self) -> Option<IndiClient> {
        self.client.clone()
    }

    /// `OnButtonDisconnectAll` once the engine let go: each connected
    /// device disconnected, its lines to the debug log.
    pub(super) fn disconnect_all(&mut self, e: &mut Engine) {
        for d in self.devices() {
            if d.is_connected() {
                d.disconnect();
            }
            for l in d.take_debug() {
                e.debug(&l);
            }
        }
    }

    /// The tick: what the devices report.
    pub(super) fn poll(&mut self, e: &mut Engine, _daemon: &super::Daemon) {
        for d in self.devices() {
            if let Some((key, v)) = d.take_profile_write() {
                // The device's own key, as PHD2's simulator and SX AO write it.
                e.store().profile().set_i32(key, v);
            }
        }
    }
}

/// "On Camera": pulses through the camera's ST-4 port
/// (`ScopeOnCamera::Connect`: the camera must be connected and have one).
#[cfg(any(test, feature = "sim"))]
struct OnCamera {
    camera: crate::sim::SimCamera,
    connected: bool,
}

#[cfg(any(test, feature = "sim"))]
impl Device for OnCamera {
    fn connect(&mut self, _cancel: &CancelToken) -> Result<(), String> {
        if !GuideCamera::is_connected(&self.camera) {
            return Err("Camera is not connected".into());
        }
        if !self.camera.has_guide_output() {
            return Err("Camera does not have an ST4 port".into());
        }
        self.connected = true;
        Ok(())
    }
    fn disconnect(&mut self) {
        self.connected = false;
    }
    fn is_connected(&self) -> bool {
        self.connected && GuideCamera::is_connected(&self.camera)
    }
}

#[cfg(any(test, feature = "sim"))]
impl Device for crate::sim::SimCamera {
    /// `CameraSimulator::Connect`: a new star field.
    fn connect(&mut self, _cancel: &CancelToken) -> Result<(), String> {
        crate::sim::SimCamera::connect(self);
        Ok(())
    }
    fn disconnect(&mut self) {
        crate::sim::SimCamera::disconnect(self);
    }
    fn is_connected(&self) -> bool {
        GuideCamera::is_connected(self)
    }
}

#[cfg(any(test, feature = "sim"))]
impl Device for crate::sim::SimAo {
    /// `StepGuiderSimulator::Connect`, then `Center` (the connect handler's,
    /// which never fails on the simulator).
    fn connect(&mut self, _cancel: &CancelToken) -> Result<(), String> {
        crate::sim::SimAo::connect(self);
        crate::engine::stepguider::StepGuiderDevice::center(self);
        Ok(())
    }
    fn disconnect(&mut self) {
        crate::sim::SimAo::disconnect(self);
    }
    fn is_connected(&self) -> bool {
        crate::sim::SimAo::is_connected(self)
    }
    fn take_profile_write(&mut self) -> Option<(&'static str, i32)> {
        crate::sim::SimAo::take_profile_write(self)
    }
}
