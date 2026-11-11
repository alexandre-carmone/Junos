//! The equipment `set_connected` connects: PHD2's gear dialog. The devices
//! are built from the profile's choices ([`Choices`]: the gear dialog's
//! `/camera/LastMenuChoice`, `/scope/LastMenuChoice`,
//! `/scope/LastAuxMenuChoice`, `/stepguider/LastMenuChoice`,
//! `/rotator/LastMenuChoice`, and the `/indi/*` device names), connected off
//! the engine thread in `OnButtonConnectAll`'s order (camera, AO, mount,
//! aux mount, rotator), and handed to the engine as a [`Gear`]. The daemon
//! keeps the same devices in a [`Rig`] on the engine thread, to disconnect
//! them, write the profile key a device owes and hear what they report.
//!
//! Each connect builds its devices anew, so the engine never waits on a
//! device being connected; the engine keeps its mounts' state across them
//! (it matches them by name). The INDI devices share one connection
//! ([`IndiClient`]), kept from one connect to the next while the server
//! stays the same, and closed by `set_connected false`.
//!
//! The tick polls the devices as PHD2's INDI thread hears from them: a drop
//! raises PHD2's alert and disconnects the mount or AO in the engine; once
//! the INDI connection is back (the client redials by itself, the PLAN's
//! addition) the daemon connects the gear again and attaches it, once per
//! new connection.
//!
//! With `--sim` the rig is the gear simulator, on the real clock: its
//! camera, the "On Camera" mount pulsing through the camera with the
//! simulator's mount as the pointing source, and its AO when the profile
//! chooses an AO (`/stepguider/LastMenuChoice` other than "None").
//!
//! Ported from PHD2 src/gear_dialog.cpp (OnButtonConnectAll and the per-device connect handlers, OnButtonDisconnectAll, ConnectAll's failure text), src/scope_onboard_st4.cpp (ConnectOnboardST4) — BSD-3-Clause, see THIRD_PARTY.md

use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::calibration::{ProfileAccess, ROTATOR_POSITION_UNKNOWN};
use crate::engine::camera::{CameraSettings, CaptureError, CaptureKind, CoolerStatus, GuideCamera};
use crate::engine::image::UsImage;
use crate::engine::mount::{AlertKind, GuideOutput, PointingSource};
use crate::engine::stepguider::{StepGuiderDevice, StepResult};
use crate::engine::traits::{CancelToken, Dir, MoveResult, PierSide, Rect};
use crate::engine::{Command, DeviceView, Engine, Gear, Rotator, ScopeGear, ScopeOutput, Which};
use crate::indi::ao::{AoOptions, IndiAo};
use crate::indi::camera::{CameraOptions, IndiCamera};
use crate::indi::client::IndiClient;
use crate::indi::mount::{IndiMount, MountOptions};
use crate::indi::rotator::{self, IndiRotator};

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
    /// What PHD2's INDI thread raises: a drop's alert, the device's own.
    fn check(&mut self, _e: &mut Engine) {}
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
    camera: String,
    scope: String,
    aux: String,
    ao: String,
    rotator: String,
    camera_opts: CameraOptions,
    mount_opts: MountOptions,
    ao_opts: AoOptions,
    rotator_device: String,
    host: String,
    port: u16,
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
        let p = e.store().profile();
        let choice = |key: &str| p.get_str(key, "None");
        Choices {
            camera: choice("/camera/LastMenuChoice"),
            scope: choice("/scope/LastMenuChoice"),
            aux: choice("/scope/LastAuxMenuChoice"),
            ao: choice("/stepguider/LastMenuChoice"),
            rotator: choice("/rotator/LastMenuChoice"),
            camera_opts: CameraOptions::from_profile(&p),
            mount_opts: MountOptions::from_profile(&p),
            ao_opts: AoOptions::from_profile(&p),
            rotator_device: p.get_str(rotator::DEVICE_KEY, rotator::UNSET_ROTATOR),
            host: p.get_str("/indi/INDIhost", &opts.indi_host),
            port: p.get_i32("/indi/INDIport", opts.indi_port as i32) as u16,
            #[cfg(any(test, feature = "sim"))]
            sim,
        }
    }

    #[cfg(any(test, feature = "sim"))]
    fn sim(&self) -> Option<&(crate::sim::SimParams, bool)> {
        self.sim.as_ref()
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
    /// `None` is the "None" choice (or one this build cannot make).
    camera: Option<Slot<Box<dyn GuideCamera>>>,
    scope: Option<Slot<ScopeGear>>,
    aux: Option<Slot<(String, Box<dyn PointingSource + Send>)>>,
    ao: Option<Slot<Box<dyn StepGuiderDevice + Send>>>,
    rotator: Option<Slot<Box<dyn Rotator>>>,
    client: Option<IndiClient>,
    /// The INDI connection the devices were connected in: another one
    /// means the server came back, and the gear is connected again.
    generation: u64,
}

impl Rig {
    /// `OnButtonConnectAll`: the chosen devices built and connected in
    /// PHD2's order; the gear for the engine holds the connected ones.
    /// `client` is the previous connect's INDI connection.
    pub(super) fn connect(
        choices: &Choices,
        client: Option<IndiClient>,
        cancel: &CancelToken,
    ) -> (Rig, Gear, Vec<Note>) {
        let mut rig = Rig::default();
        #[cfg(any(test, feature = "sim"))]
        if let Some((params, ao)) = choices.sim() {
            rig.build_sim(params.clone(), *ao);
        } else {
            rig.build_indi(choices, client);
        }
        #[cfg(not(any(test, feature = "sim")))]
        rig.build_indi(choices, client);

        let mut notes = Vec::new();
        let mut connect = |what: &str, dev: &mut Box<dyn Device>| {
            notes.push(Note::Status(format!("Connecting to {what} ...")));
            let r = dev.connect(cancel);
            notes.extend(dev.take_debug().into_iter().map(Note::Debug));
            match r {
                Ok(()) => notes.push(Note::Debug(format!("Connected {what}"))),
                Err(e) => {
                    notes.push(Note::Debug(e));
                    notes.push(Note::Status(format!("{what} Connect Failed")));
                }
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
        if let Some(s) = rig.aux.as_mut() {
            connect("Aux Mount", &mut s.dev);
        }
        if let Some(s) = rig.rotator.as_mut() {
            connect("Rotator", &mut s.dev);
        }
        rig.generation = rig.client.as_ref().map_or(0, |c| c.lock().generation());
        let gear = rig.gear();
        (rig, gear, notes)
    }

    #[cfg(any(test, feature = "sim"))]
    fn build_sim(&mut self, params: crate::sim::SimParams, ao: bool) {
        use crate::sim::{SimCamera, SimTime};
        let cam = SimCamera::new(params, SimTime::Real);
        let c = cam.clone();
        self.camera = Some(Slot { dev: Box::new(cam.clone()), engine: Box::new(move || Box::new(c.clone())) });
        let (c, mount) = (cam.clone(), cam.mount());
        self.scope = Some(Slot {
            dev: Box::new(Port::new(move || Some((GuideCamera::is_connected(&c), c.has_guide_output())))),
            engine: Box::new(move || ScopeGear {
                name: "On Camera".into(),
                output: ScopeOutput::OnCamera,
                pointing: Some(Box::new(mount.clone())),
            }),
        });
        if ao {
            let a = cam.ao();
            self.ao = Some(Slot { dev: Box::new(a.clone()), engine: Box::new(move || Box::new(a.clone())) });
        }
    }

    /// The factories of `GearDialog`'s choices: the INDI devices, the
    /// "On-camera" and "On-AO" mounts. Any other choice (a simulator
    /// without `--sim`) is not made, and fails as a missing device.
    fn build_indi(&mut self, c: &Choices, client: Option<IndiClient>) {
        let indi = |choice: &str| choice.contains("INDI");
        let client = match client {
            Some(cl) if cl.host() == c.host && cl.port() == c.port => cl,
            old => {
                if let Some(old) = old {
                    old.shutdown();
                }
                let mut watched = Vec::new();
                if indi(&c.camera) {
                    watched.push(c.camera_opts.device.as_str());
                }
                if indi(&c.scope) || indi(&c.aux) {
                    watched.push(c.mount_opts.device.as_str());
                }
                if c.ao.contains("(INDI)") {
                    watched.push(c.ao_opts.device.as_str());
                }
                if indi(&c.rotator) {
                    watched.push(c.rotator_device.as_str());
                }
                IndiClient::start(&c.host, c.port, &watched)
            }
        };
        let camera = indi(&c.camera).then(|| Shared::new(IndiCamera::new(client.clone(), &c.camera_opts)));
        if let Some(cam) = &camera {
            let e = cam.clone();
            self.camera = Some(Slot { dev: Box::new(cam.clone()), engine: Box::new(move || Box::new(e.clone())) });
        }
        // `SX AO (INDI)`, `SBIG AO (INDI)`: the same INDI AO.
        let ao = c.ao.contains("(INDI)").then(|| Shared::new(IndiAo::new(client.clone(), &c.ao_opts)));
        if let Some(a) = &ao {
            let e = a.clone();
            self.ao = Some(Slot { dev: Box::new(a.clone()), engine: Box::new(move || Box::new(e.clone())) });
        }
        if indi(&c.scope) {
            let m = Shared::new(IndiMount::new(client.clone(), &c.mount_opts, false));
            let e = m.clone();
            self.scope = Some(Slot {
                dev: Box::new(m),
                engine: Box::new(move || ScopeGear {
                    name: e.lock().name(),
                    output: ScopeOutput::Mount(Box::new(e.clone())),
                    pointing: Some(Box::new(e.clone())),
                }),
            });
        } else if c.scope == "On-camera" {
            let cam = camera.clone();
            self.scope = Some(Slot {
                dev: Box::new(Port::new(move || {
                    cam.as_ref().map(|c| (GuideCamera::is_connected(c), c.has_guide_output()))
                })),
                engine: Box::new(|| ScopeGear { name: "On Camera".into(), output: ScopeOutput::OnCamera, pointing: None }),
            });
        } else if c.scope == "On-AO" {
            let (probe, out) = (ao.clone(), ao.clone());
            self.scope = Some(Slot {
                dev: Box::new(Port::new(move || probe.as_ref().map(|a| (a.lock().is_connected(), true)))),
                engine: Box::new(move || ScopeGear {
                    name: "On StepGuider".into(),
                    output: match &out {
                        Some(a) => ScopeOutput::OnAo(Box::new(a.clone())),
                        None => ScopeOutput::OnCamera,
                    },
                    pointing: None,
                }),
            });
        }
        if indi(&c.aux) {
            let m = Shared::new(IndiMount::new(client.clone(), &c.mount_opts, true));
            let e = m.clone();
            self.aux = Some(Slot {
                dev: Box::new(m),
                engine: Box::new(move || (e.lock().name(), Box::new(e.clone()) as Box<dyn PointingSource + Send>)),
            });
        }
        if indi(&c.rotator) {
            let r = Shared::new(IndiRotator::new(client.clone(), &c.rotator_device));
            let e = r.clone();
            self.rotator = Some(Slot { dev: Box::new(r), engine: Box::new(move || Box::new(e.clone())) });
        }
        self.client = Some(client);
    }

    /// The engine's side of the connected devices.
    fn gear(&self) -> Gear {
        fn connected<T>(s: &Option<Slot<T>>) -> Option<T> {
            s.as_ref().filter(|s| s.dev.is_connected()).map(|s| (s.engine)())
        }
        Gear {
            camera: connected(&self.camera),
            scope: connected(&self.scope),
            aux_mount: connected(&self.aux),
            ao: connected(&self.ao),
            rotator: connected(&self.rotator),
        }
    }

    fn devices(&mut self) -> impl Iterator<Item = &mut Box<dyn Device>> {
        // `OnButtonDisconnectAll`'s order: mount, aux mount, camera, AO,
        // rotator.
        [
            self.scope.as_mut().map(|s| &mut s.dev),
            self.aux.as_mut().map(|s| &mut s.dev),
            self.camera.as_mut().map(|s| &mut s.dev),
            self.ao.as_mut().map(|s| &mut s.dev),
            self.rotator.as_mut().map(|s| &mut s.dev),
        ]
        .into_iter()
        .flatten()
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

    /// `ConnectAll`'s failure list: " camera mount aux mount AO Rotator",
    /// empty when all connected.
    pub(super) fn failures(&self) -> String {
        fn down<T>(s: &Option<Slot<T>>) -> bool {
            s.as_ref().is_some_and(|s| !s.dev.is_connected())
        }
        let mut fail = String::new();
        if self.camera.as_ref().is_none_or(|s| !s.dev.is_connected()) {
            fail += " camera";
        }
        for (down, what) in [
            (down(&self.scope), " mount"),
            (down(&self.aux), " aux mount"),
            (down(&self.ao), " AO"),
            (down(&self.rotator), " Rotator"),
        ] {
            if down {
                fail += what;
            }
        }
        fail
    }

    /// The INDI connection the devices share, for the next connect.
    pub(super) fn client(&self) -> Option<IndiClient> {
        self.client.clone()
    }

    /// `OnButtonDisconnectAll` once the engine let go: each connected
    /// device disconnected, its lines to the debug log, the INDI
    /// connection closed.
    pub(super) fn disconnect_all(&mut self, e: &mut Engine) {
        for d in self.devices() {
            if d.is_connected() {
                d.disconnect();
            }
            for l in d.take_debug() {
                e.debug(&l);
            }
        }
        if let Some(c) = self.client.take() {
            c.shutdown();
        }
    }

    /// The tick: what the devices report, a drop told to the engine, and
    /// the gear connected again once the INDI server is back.
    pub(super) fn poll(&mut self, e: &mut Engine, daemon: &super::Daemon) {
        for d in self.devices() {
            d.check(e);
            for l in d.take_debug() {
                e.debug(&l);
            }
            if let Some((key, v)) = d.take_profile_write() {
                // The device's own key, as PHD2's simulator and SX AO write it.
                e.store().profile().set_i32(key, v);
            }
        }
        let snap = e.snapshot();
        fn lost<T>(view: Option<&DeviceView>, slot: &Option<Slot<T>>) -> bool {
            view.is_some_and(|v| v.connected) && slot.as_ref().is_some_and(|s| !s.dev.is_connected())
        }
        if lost(snap.mount.as_ref(), &self.scope) {
            let _ = e.handle(Command::DeviceLost(Which::Mount));
        }
        if lost(snap.ao.as_ref(), &self.ao) {
            let _ = e.handle(Command::DeviceLost(Which::Ao));
        }
        // The PLAN's addition: once the server is back, connect again.
        let back = self.client.as_ref().map(|c| {
            let l = c.lock();
            (l.is_up(), l.generation())
        });
        if let Some((true, generation)) = back
            && generation != self.generation
        {
            self.generation = generation;
            e.debug("INDI server connection back: connecting the gear again");
            let d = daemon.clone();
            std::thread::spawn(move || {
                let _ = d.connect_all();
            });
        }
    }
}

/// A device behind a lock: the engine drives it through its trait, the
/// rig disconnects and polls it. Once attached both run on the engine
/// thread, so the lock is never waited for.
struct Shared<T>(Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

impl<T> Shared<T> {
    fn new(t: T) -> Self {
        Shared(Arc::new(Mutex::new(t)))
    }

    fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// An ST-4 port as a mount, "On-camera" or "On-AO"
/// (`ScopeOnboardST4::ConnectOnboardST4`): `host` tells whether the port's
/// device is there, connected and has a guide output.
struct Port {
    host: Box<dyn Fn() -> Option<(bool, bool)> + Send>,
    connected: bool,
}

impl Port {
    fn new(host: impl Fn() -> Option<(bool, bool)> + Send + 'static) -> Port {
        Port { host: Box::new(host), connected: false }
    }
}

impl Device for Port {
    fn connect(&mut self, _cancel: &CancelToken) -> Result<(), String> {
        match (self.host)() {
            None => Err("Attempt to Connect OnboardST4 mount with pOnboardHost == NULL".into()),
            Some((_, false)) => Err("Attempt to Connect Onboard ST4 mount when host does not have guide output".into()),
            Some((false, _)) => Err("Attempt to Connect Onboard ST4 mount when host is not connected".into()),
            Some((true, true)) => {
                self.connected = true;
                Ok(())
            }
        }
    }
    fn disconnect(&mut self) {
        self.connected = false;
    }
    /// Connected while its host is (the port has no driver of its own).
    fn is_connected(&self) -> bool {
        self.connected && (self.host)().is_some_and(|(c, _)| c)
    }
}

impl Device for Shared<IndiCamera> {
    fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        self.lock().connect(cancel)
    }
    fn disconnect(&mut self) {
        self.lock().disconnect();
    }
    fn is_connected(&self) -> bool {
        GuideCamera::is_connected(&*self.lock())
    }
    fn take_debug(&mut self) -> Vec<String> {
        IndiCamera::take_debug(&mut self.lock())
    }
    fn check(&mut self, e: &mut Engine) {
        let (lost, alerts) = {
            let mut c = self.lock();
            (c.check_connection(), c.take_alerts())
        };
        if let Some(err) = lost {
            e.camera_lost(&err);
        }
        for a in alerts {
            e.alert(a, AlertKind::Warning);
        }
    }
}

impl Device for Shared<IndiMount> {
    fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        self.lock().connect(cancel)
    }
    fn disconnect(&mut self) {
        self.lock().disconnect();
    }
    fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }
    fn take_debug(&mut self) -> Vec<String> {
        self.lock().take_debug()
    }
    fn check(&mut self, e: &mut Engine) {
        let alert = self.lock().check_connection();
        if let Some(a) = alert {
            e.alert(a, AlertKind::Warning);
        }
    }
}

impl Device for Shared<IndiAo> {
    fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        let mut ao = self.lock();
        ao.connect(cancel)?;
        // The connect handler's `Center`; a failure disconnects the AO.
        if !StepGuiderDevice::center(&mut *ao) {
            ao.disconnect();
            return Err("StepGuider unable to center".into());
        }
        Ok(())
    }
    fn disconnect(&mut self) {
        self.lock().disconnect();
    }
    fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }
    fn take_debug(&mut self) -> Vec<String> {
        self.lock().take_debug()
    }
    fn take_profile_write(&mut self) -> Option<(&'static str, i32)> {
        self.lock().take_profile_write()
    }
    fn check(&mut self, e: &mut Engine) {
        let alerts = {
            let mut ao = self.lock();
            ao.check_connection();
            ao.take_alerts()
        };
        for a in alerts {
            e.alert(a, AlertKind::Warning);
        }
    }
}

impl Device for Shared<IndiRotator> {
    fn connect(&mut self, cancel: &CancelToken) -> Result<(), String> {
        self.lock().connect(cancel)
    }
    fn disconnect(&mut self) {
        self.lock().disconnect();
    }
    fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }
    fn take_debug(&mut self) -> Vec<String> {
        self.lock().take_debug()
    }
    fn check(&mut self, e: &mut Engine) {
        let alert = self.lock().check_connection();
        if let Some(a) = alert {
            e.alert(a, AlertKind::Warning);
        }
    }
}

impl<C: GuideCamera> GuideCamera for Shared<C> {
    fn name(&self) -> String {
        self.lock().name()
    }
    fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }
    fn frame_size(&self) -> (i32, i32) {
        self.lock().frame_size()
    }
    fn binning(&self) -> u8 {
        self.lock().binning()
    }
    fn set_binning(&mut self, binning: u8) {
        self.lock().set_binning(binning);
    }
    fn max_binning(&self) -> u8 {
        self.lock().max_binning()
    }
    fn pixel_size_um(&self) -> Option<f64> {
        self.lock().pixel_size_um()
    }
    fn bits_per_pixel(&self) -> u8 {
        self.lock().bits_per_pixel()
    }
    fn has_bayer(&self) -> bool {
        self.lock().has_bayer()
    }
    fn pixel_size_xy(&self) -> Option<(f64, f64)> {
        self.lock().pixel_size_xy()
    }
    fn has_gain_control(&self) -> bool {
        self.lock().has_gain_control()
    }
    fn has_shutter(&self) -> bool {
        self.lock().has_shutter()
    }
    fn has_subframes(&self) -> bool {
        self.lock().has_subframes()
    }
    fn has_cooler(&self) -> bool {
        self.lock().has_cooler()
    }
    fn set_cooler_on(&mut self, on: bool) -> bool {
        self.lock().set_cooler_on(on)
    }
    fn set_cooler_setpoint(&mut self, celsius: f64) -> bool {
        self.lock().set_cooler_setpoint(celsius)
    }
    fn cooler_status(&self) -> Option<CoolerStatus> {
        self.lock().cooler_status()
    }
    fn sensor_temperature(&self) -> Option<f64> {
        self.lock().sensor_temperature()
    }
    fn has_guide_output(&self) -> bool {
        self.lock().has_guide_output()
    }
    fn st4_pulse(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> Result<(), String> {
        self.lock().st4_pulse(dir, ms, cancel)
    }
    fn take_debug(&mut self) -> Vec<String> {
        GuideCamera::take_debug(&mut *self.lock())
    }
    fn capture(
        &mut self,
        dur_ms: u32,
        kind: CaptureKind,
        subframe: Option<Rect>,
        settings: &CameraSettings,
        cancel: &CancelToken,
    ) -> Result<UsImage, CaptureError> {
        self.lock().capture(dur_ms, kind, subframe, settings, cancel)
    }
}

impl<M: GuideOutput> GuideOutput for Shared<M> {
    fn guide(&mut self, dir: Dir, ms: u32, cancel: &CancelToken) -> MoveResult {
        self.lock().guide(dir, ms, cancel)
    }
    fn can_pulse_guide(&self) -> bool {
        self.lock().can_pulse_guide()
    }
}

impl<M: PointingSource> PointingSource for Shared<M> {
    fn coordinates(&self) -> Option<(f64, f64, f64)> {
        self.lock().coordinates()
    }
    fn pier_side(&self) -> PierSide {
        self.lock().pier_side()
    }
    fn guide_rates(&self) -> Option<(f64, f64)> {
        self.lock().guide_rates()
    }
    fn is_slewing(&self) -> bool {
        self.lock().is_slewing()
    }
    fn can_report_position(&self) -> bool {
        self.lock().can_report_position()
    }
    fn can_slew_async(&self) -> bool {
        self.lock().can_slew_async()
    }
    fn slew_to_async(&mut self, ra_hours: f64, dec_deg: f64) -> bool {
        self.lock().slew_to_async(ra_hours, dec_deg)
    }
    fn abort_slew(&mut self) {
        self.lock().abort_slew();
    }
    fn site_lat_long(&self) -> Option<(f64, f64)> {
        self.lock().site_lat_long()
    }
    fn declination_radians(&self) -> Option<f64> {
        self.lock().declination_radians()
    }
}

impl<A: StepGuiderDevice> StepGuiderDevice for Shared<A> {
    fn name(&self) -> String {
        self.lock().name()
    }
    fn step(&mut self, dir: Dir, steps: u32) -> StepResult {
        self.lock().step(dir, steps)
    }
    fn max_position(&self, dir: Dir) -> i32 {
        self.lock().max_position(dir)
    }
    fn set_max_position(&mut self, steps: i32) -> bool {
        self.lock().set_max_position(steps)
    }
    fn center(&mut self) -> bool {
        self.lock().center()
    }
}

impl Rotator for Shared<IndiRotator> {
    fn name(&self) -> String {
        self.lock().name()
    }
    fn is_connected(&self) -> bool {
        self.lock().is_connected()
    }
    /// `Rotator::RotatorPosition`: unknown while disconnected or before the
    /// driver gives its angle.
    fn position_deg(&self) -> Option<f64> {
        Some(self.lock().position()).filter(|&a| a != ROTATOR_POSITION_UNKNOWN)
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
        StepGuiderDevice::center(self);
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
