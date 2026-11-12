//! The connection to `indiserver`: one TCP socket for every device the
//! guider drives, one reader thread that feeds the [`PropertyStore`] and
//! queues the BLOBs, and the wrappers' writes behind a mutex.
//!
//! PHD2 opens one libindiclient connection per device, each wrapper being an
//! `INDI::BaseClient`. Here they share one, and what each of those clients
//! did on connect is done per device: a `getProperties` for each watched
//! device (`watchDevice`), `enableBLOB Also` naming the camera's BLOB
//! property once it is defined (`setBLOBMode`), and the device connected
//! through its `CONNECTION` switch by the wrapper that needs it
//! (`connectDevice`). The reader answers the server's pings. Nothing here
//! uses async: the engine thread calls the wrappers, which block in
//! [`IndiClient::wait`] until the reader has applied what they wait for, and
//! look at their cancel token at least every 100 ms (PHD2's
//! `WaitTimeout(100)` loops).
//!
//! Deliberate addition (PLAN): when the server goes away, the reader forgets
//! every device (libindi's `clear`) and dials again, 250 ms after the drop
//! and then backing off to 8 s, and sends the same `getProperties` again,
//! after which each `enableBLOB` follows its property's definition. The
//! link's generation number tells a wrapper its device dropped meanwhile:
//! it then reports itself disconnected, as PHD2's did on
//! `serverDisconnected`, until the daemon connects it again.
//!
//! The device chooser keeps a device by the interface bits its
//! `DRIVER_INFO` had when defined (`INDIConfig::newProperty`, never
//! re-tested on updates); a device that has not defined `DRIVER_INFO` stays
//! listed, as in PHD2's dialog. The writes mirror libindi's local copy
//! ([`PropertyStore::sent`]), except `CONNECTION`'s: see
//! [`IndiClient::set_driver_connection`]. [`IndiClient::set_driver_connection`]
//! follows INDI libs/indiabstractclient/abstractbaseclient.cpp
//! (`setDriverConnection`, LGPL-2.1-or-later, see THIRD_PARTY.md).
//!
//! Ported from PHD2 src/config_indi.cpp, src/cam_indi.cpp, src/scope_indi.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::store::{
    AO_INTERFACE, Blob, CCD_INTERFACE, ChangeKind, GUIDER_INTERFACE, PropertyStore, ROTATOR_INTERFACE,
    TELESCOPE_INTERFACE,
};
use super::xml::{self, BlobMode, Element, Kind, Message, PropState, Update, Value, XmlStream};
use crate::engine::calibration::ProfileAccess;
use crate::engine::traits::{CancelToken, Dir};

/// How long a dial may take: libindi's default connection timeout.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a wrapper waits for its device to connect: PHD2's 300 × 100 ms.
pub const DEVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a wrapper whose device just connected waits for the properties
/// it works with, before it says it is connected anyway (as PHD2 does at
/// once, its first use coming much later).
pub const READY_GRACE: Duration = Duration::from_secs(2);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
/// The longest a wait sleeps before it looks at its cancel token again.
const WAIT_SLICE: Duration = Duration::from_millis(100);
/// BLOBs kept for the wrappers that asked for them, the oldest dropped: a
/// camera reads the newest only, a video stack drains them as they come.
const MAX_QUEUED_BLOBS: usize = 8;
/// A write blocked longer than this means the server is gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a device is chosen for: PHD2's `IndiDevType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Camera,
    Mount,
    AuxMount,
    Ao,
    Rotator,
}

impl Role {
    /// The chooser's test on `DRIVER_INTERFACE` (config_indi.cpp:395). The
    /// mount leaves out cameras with an ST-4 port (CCD and GUIDER bits) and,
    /// per the PLAN, the AOs too, which PHD2 lists since they carry the
    /// GUIDER bit: an AO's own pulses are the "On AO" mount.
    pub fn includes(self, interfaces: u16) -> bool {
        let has = |bit: u16| interfaces & bit != 0;
        match self {
            Role::Camera => has(CCD_INTERFACE),
            Role::Mount => !has(CCD_INTERFACE) && !has(AO_INTERFACE) && has(GUIDER_INTERFACE),
            Role::AuxMount => has(TELESCOPE_INTERFACE),
            Role::Ao => has(AO_INTERFACE),
            Role::Rotator => has(ROTATOR_INTERFACE),
        }
    }
}

/// A device as the chooser lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub name: String,
    /// `DRIVER_INFO.DRIVER_INTERFACE`, 0 until it is known.
    pub interfaces: u16,
    /// Its `CONNECTION.CONNECT` switch is On.
    pub connected: bool,
}

/// Why [`IndiClient::wait`] gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    Timeout,
    Cancelled,
}

/// What the reader thread keeps, behind the client's lock.
#[derive(Default)]
pub struct Link {
    /// The watched devices, as the server last described them; empty while
    /// the link is down.
    pub store: PropertyStore,
    up: bool,
    generation: u64,
    watched: Vec<String>,
    /// `(device, property)` of the BLOBs asked for.
    blob_props: Vec<(String, String)>,
    blobs: Vec<Blob>,
    debug: Vec<String>,
    /// Per device, the last update that turned its `CONNECT` switch Off.
    switched_off: Vec<(String, SwitchedOff)>,
}

/// What a device had defined when an update turned its `CONNECT` switch
/// Off. PHD2 tests its `ready` flag on that update, but a driver deletes
/// the properties `ready` stands for in the same burst, right after it
/// (defaultdevice.cpp:1190-1196: `setConnected(false)`, then
/// `updateProperties`): a wrapper looking later finds them gone.
#[derive(Debug)]
pub struct SwitchedOff {
    /// The update's sequence number.
    pub seq: u64,
    props: Vec<(String, Kind)>,
}

impl SwitchedOff {
    /// The device had `name`, of `kind`, defined then.
    pub fn had(&self, name: &str, kind: Kind) -> bool {
        self.props.iter().any(|(n, k)| n == name && *k == kind)
    }
}

impl Link {
    /// The socket is connected.
    pub fn is_up(&self) -> bool {
        self.up
    }

    /// The number of connections made to the server so far: a wrapper that
    /// sees another one than when it attached knows its device dropped.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The queued BLOBs of `device` whose member is `member`, oldest first.
    pub fn take_blobs(&mut self, device: &str, member: &str) -> Vec<Blob> {
        let (take, keep) =
            std::mem::take(&mut self.blobs).into_iter().partition(|b| b.device == device && b.member == member);
        self.blobs = keep;
        take
    }

    /// The `CONNECTION.CONNECT` switch of `device`, `None` before it is defined.
    pub fn device_connected(&self, device: &str) -> Option<bool> {
        self.store.property(device, "CONNECTION").filter(|p| p.kind == Kind::Switch).and_then(|p| p.switch("CONNECT"))
    }

    /// The last update that turned `device`'s `CONNECT` switch Off, if it
    /// came after the sequence number `since`.
    pub fn switched_off(&self, device: &str, since: u64) -> Option<&SwitchedOff> {
        self.switched_off.iter().find(|(d, _)| d == device).map(|(_, o)| o).filter(|o| o.seq > since)
    }
}

struct Shared {
    host: String,
    port: u16,
    link: Mutex<Link>,
    changed: Condvar,
    writer: Mutex<Option<TcpStream>>,
    stop: AtomicBool,
    /// Skip the rest of the backoff: a wrapper is waiting for the link.
    redial: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// A handle on the connection; clones share it. Locks are always taken
/// link first, writer second.
#[derive(Clone)]
pub struct IndiClient {
    sh: Arc<Shared>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl IndiClient {
    /// Starts the reader thread, which dials `host:port` at once and again
    /// whenever the connection drops, until [`shutdown`](Self::shutdown).
    /// `devices` are watched from the first connection (the profile's
    /// gear); with none, the server is asked for every device: the device
    /// chooser's client (PHD2's `INDIConfig` has its own connection).
    pub fn start(host: &str, port: u16, devices: &[&str]) -> IndiClient {
        let link = Link { watched: devices.iter().map(|d| d.to_string()).collect(), ..Link::default() };
        let sh = Arc::new(Shared {
            host: host.to_owned(),
            port,
            link: Mutex::new(link),
            changed: Condvar::new(),
            writer: Mutex::new(None),
            stop: AtomicBool::new(false),
            redial: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let reader = Arc::clone(&sh);
        let t = thread::Builder::new()
            .name("indi-client".into())
            .spawn(move || run(&reader))
            .expect("spawn the INDI reader thread");
        *lock(&sh.thread) = Some(t);
        IndiClient { sh }
    }

    pub fn host(&self) -> &str {
        &self.sh.host
    }

    pub fn port(&self) -> u16 {
        self.sh.port
    }

    /// [`shutdown`](Self::shutdown) was called: the client dials no more.
    pub fn is_stopped(&self) -> bool {
        self.sh.stop.load(Ordering::SeqCst)
    }

    /// The same connection as `other` (a clone of it).
    pub fn same(&self, other: &IndiClient) -> bool {
        Arc::ptr_eq(&self.sh, &other.sh)
    }

    /// Closes the connection and stops the reader thread.
    pub fn shutdown(&self) {
        self.sh.stop.store(true, Ordering::SeqCst);
        if let Some(s) = lock(&self.sh.writer).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.sh.changed.notify_all();
        let t = lock(&self.sh.thread).take();
        if let Some(t) = t.filter(|t| t.thread().id() != thread::current().id()) {
            let _ = t.join();
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Link> {
        lock(&self.sh.link)
    }

    /// The client's debug-log lines since the last call.
    pub fn take_debug(&self) -> Vec<String> {
        std::mem::take(&mut self.lock().debug)
    }

    /// Blocks until `f` answers, checking it each time the reader applied
    /// something and every 100 ms. `f` runs under the lock.
    pub fn wait<R>(
        &self,
        timeout: Duration,
        cancel: Option<&CancelToken>,
        mut f: impl FnMut(&mut Link) -> Option<R>,
    ) -> Result<R, WaitError> {
        let deadline = Instant::now() + timeout;
        let mut link = self.lock();
        loop {
            if let Some(r) = f(&mut link) {
                return Ok(r);
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(WaitError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(WaitError::Timeout);
            }
            link = self
                .sh
                .changed
                .wait_timeout(link, (deadline - now).min(WAIT_SLICE))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Waits for the socket to be connected, at most one dial's time
    /// (PHD2's `connectServer`); the generation then.
    pub fn wait_up(&self, cancel: &CancelToken) -> Option<u64> {
        self.wait(DIAL_TIMEOUT, Some(cancel), |l| {
            // Down: the reader skips the rest of its backoff.
            self.sh.redial.store(!l.up, Ordering::SeqCst);
            l.up.then_some(l.generation)
        })
        .ok()
    }

    /// Writes to the server; false when the link is down or the write
    /// failed, which drops the link.
    pub fn send(&self, text: &str) -> bool {
        let mut w = lock(&self.sh.writer);
        let Some(s) = w.as_mut() else { return false };
        if s.write_all(text.as_bytes()).is_ok() {
            return true;
        }
        let _ = s.shutdown(Shutdown::Both);
        false
    }

    /// `watchDevice`: the device's properties are asked for now and on
    /// every reconnect.
    pub fn watch(&self, device: &str) {
        let mut link = self.lock();
        if link.watched.iter().any(|d| d == device) {
            return;
        }
        link.watched.push(device.to_owned());
        if link.up {
            self.send(&xml::get_properties(Some(device), None));
        }
    }

    /// `setBLOBMode(B_ALSO, device, property)`: sent when the property is
    /// defined, and again after each reconnect; its BLOBs are queued for
    /// [`Link::take_blobs`].
    pub fn want_blob(&self, device: &str, property: &str) {
        let mut link = self.lock();
        if !link.blob_props.iter().any(|(d, p)| d == device && p == property) {
            link.blob_props.push((device.to_owned(), property.to_owned()));
        }
        if link.store.property(device, property).is_some_and(|p| p.kind == Kind::Blob) {
            self.send(&xml::enable_blob(device, Some(property), BlobMode::Also));
        }
    }

    /// Stops the BLOBs of a property (what closing PHD2's per-device
    /// connection did) and drops the queued ones.
    pub fn drop_blob(&self, device: &str, property: &str) {
        let mut link = self.lock();
        link.blob_props.retain(|(d, p)| !(d == device && p == property));
        link.blobs.retain(|b| !(b.device == device && b.property == property));
        if link.up {
            self.send(&xml::enable_blob(device, Some(property), BlobMode::Never));
        }
    }

    /// The devices the chooser offers for `role`, in the order they came.
    /// Only a client started with no device sees them all: indiserver
    /// sends every device only to a client whose first request named none
    /// (ClInfo.cpp:51), and filters the others by the devices they named.
    pub fn devices(&self, role: Role) -> Vec<DeviceEntry> {
        let link = self.lock();
        link.store
            .devices()
            .iter()
            .filter(|d| d.interface_at_definition.is_none_or(|i| role.includes(i)))
            .map(|d| DeviceEntry {
                name: d.name.clone(),
                interfaces: d.driver_interface(),
                connected: link.device_connected(&d.name) == Some(true),
            })
            .collect()
    }

    /// `sendNewNumber` of a defined number vector: every member, in the
    /// driver's order, with the values given and the others as they are;
    /// the store's copy goes Busy with them, as libindi's does.
    pub fn send_number(&self, link: &mut Link, device: &str, property: &str, values: &[(&str, f64)]) -> bool {
        let Some(p) = link.store.property(device, property).filter(|p| p.kind == Kind::Number) else { return false };
        let members: Vec<(&str, f64)> = p
            .members
            .iter()
            .map(|m| {
                let current = match m.value {
                    Value::Number { value, .. } => value,
                    _ => 0.0,
                };
                (m.name.as_str(), values.iter().find(|(n, _)| *n == m.name).map_or(current, |(_, v)| *v))
            })
            .collect();
        let sent = self.send(&xml::new_number_vector(device, property, &members));
        let local: Vec<Update> = members.iter().map(|&(_, value)| Update::Number { value, min: None, max: None }).collect();
        link.store.sent(device, property, &local);
        sent
    }

    /// `sendNewSwitch` of a defined switch vector, as [`send_number`](Self::send_number).
    pub fn send_switch(&self, link: &mut Link, device: &str, property: &str, states: &[(&str, bool)]) -> bool {
        let Some(p) = link.store.property(device, property).filter(|p| p.kind == Kind::Switch) else { return false };
        let members: Vec<(&str, bool)> = p
            .members
            .iter()
            .map(|m| {
                let current = matches!(m.value, Value::Switch(true));
                (m.name.as_str(), states.iter().find(|(n, _)| *n == m.name).map_or(current, |(_, on)| *on))
            })
            .collect();
        let sent = self.send(&xml::new_switch_vector(device, property, p.rule, &members));
        let local: Vec<Update> = members.iter().map(|&(_, on)| Update::Switch(on)).collect();
        link.store.sent(device, property, &local);
        sent
    }

    /// libindi's `setDriverConnection`: the `CONNECTION` members by
    /// position, the first On to connect, the second to disconnect, every
    /// other Off; nothing when that member is On already. False when there
    /// was nothing to send to. libindi also sets its own copy On; the store's
    /// stays as the driver last said, since the wrappers read `CONNECT` as
    /// PHD2's `Connected`, which only the driver's updates set
    /// (cam_indi.cpp:326, 575).
    pub fn set_driver_connection(&self, link: &Link, device: &str, on: bool) -> bool {
        let Some(p) = link.store.property(device, "CONNECTION").filter(|p| p.kind == Kind::Switch) else {
            return false;
        };
        let target = if on { 0 } else { 1 };
        match p.members.get(target).map(|m| &m.value) {
            None => false,
            Some(Value::Switch(true)) => true,
            Some(_) => {
                let states: Vec<(&str, bool)> =
                    p.members.iter().enumerate().map(|(i, m)| (m.name.as_str(), i == target)).collect();
                self.send(&xml::new_switch_vector(device, "CONNECTION", p.rule, &states))
            }
        }
    }
}

/// The reader thread: dial, serve until the link drops, back off, again.
fn run(sh: &Shared) {
    let mut backoff = MIN_BACKOFF;
    while !sh.stop.load(Ordering::SeqCst) {
        if let Some(sock) = dial(sh) {
            backoff = MIN_BACKOFF;
            serve(sh, sock);
        }
        let until = Instant::now() + backoff;
        while !sh.stop.load(Ordering::SeqCst) && !sh.redial.swap(false, Ordering::SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Each address the host resolves to in turn, until one answers or the
/// client is shut down.
fn dial(sh: &Shared) -> Option<TcpStream> {
    (sh.host.as_str(), sh.port)
        .to_socket_addrs()
        .ok()?
        .take_while(|_| !sh.stop.load(Ordering::SeqCst))
        .find_map(|a| TcpStream::connect_timeout(&a, DIAL_TIMEOUT).ok())
}

/// One connection: say hello, apply what comes, clear up when it ends.
fn serve(sh: &Shared, sock: TcpStream) {
    let _ = sock.set_nodelay(true);
    let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
    let Ok(mut reader) = sock.try_clone() else { return };
    {
        let mut link = lock(&sh.link);
        let mut writer = lock(&sh.writer);
        // A shutdown that came while dialling.
        if sh.stop.load(Ordering::SeqCst) {
            return;
        }
        sh.redial.store(false, Ordering::SeqCst);
        link.store.clear();
        link.blobs.clear();
        link.switched_off.clear();
        link.up = true;
        link.generation += 1;
        link.debug.push(format!("INDI: connected to {}:{}", sh.host, sh.port));
        // `connectServer`: one request per watched device, or one for all
        // when nothing is watched.
        let hello: String = if link.watched.is_empty() {
            xml::get_properties(None, None)
        } else {
            link.watched.iter().map(|d| xml::get_properties(Some(d), None)).collect()
        };
        let _ = (&sock).write_all(hello.as_bytes());
        *writer = Some(sock);
    }
    sh.changed.notify_all();

    let mut stream = XmlStream::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let elements = stream.push(&buf[..n]);
        if elements.is_empty() {
            continue;
        }
        {
            let mut link = lock(&sh.link);
            for e in elements {
                handle(sh, &mut link, e);
            }
        }
        sh.changed.notify_all();
    }

    {
        let mut link = lock(&sh.link);
        link.up = false;
        link.store.clear();
        link.blobs.clear();
        link.switched_off.clear();
        link.debug.push(format!("INDI: lost the connection to {}:{}", sh.host, sh.port));
        if let Some(s) = lock(&sh.writer).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
    sh.changed.notify_all();
}

/// Writes from the reader thread (pings, BLOB modes).
fn write(sh: &Shared, text: &str) {
    if let Some(s) = lock(&sh.writer).as_mut() {
        let _ = s.write_all(text.as_bytes());
    }
}

fn handle(sh: &Shared, link: &mut Link, e: Element) {
    let Some(msg) = Message::from_element(e) else { return };
    if let Message::PingRequest { uid } = &msg {
        write(sh, &xml::ping_reply(uid));
        return;
    }
    let out = link.store.apply(msg);
    for c in &out.changes {
        // PHD2's wrappers test `ready` on every `CONNECTION` update with
        // `CONNECT` Off (cam_indi.cpp:333, scope_indi.cpp:424,
        // rotator_indi.cpp:372): keep what the device had at that moment.
        if c.kind == ChangeKind::Updated && c.property == "CONNECTION" && link.device_connected(&c.device) == Some(false)
        {
            let props = link.store.device(&c.device).map(|d| d.properties.iter().map(|p| (p.name.clone(), p.kind)));
            let off = SwitchedOff { seq: c.seq, props: props.into_iter().flatten().collect() };
            link.switched_off.retain(|(d, _)| *d != c.device);
            link.switched_off.push((c.device.clone(), off));
        }
        let wanted = link.blob_props.iter().any(|(d, p)| *d == c.device && *p == c.property);
        if c.kind == ChangeKind::Defined && wanted {
            write(sh, &xml::enable_blob(&c.device, Some(&c.property), BlobMode::Also));
        }
    }
    for b in out.blobs {
        if link.blob_props.iter().any(|(d, p)| *d == b.device && *p == b.property) {
            link.blobs.push(b);
            if link.blobs.len() > MAX_QUEUED_BLOBS {
                link.blobs.remove(0);
            }
        }
    }
}

/// Sleeps `ms`, waking every 100 ms for `cancel` (`WorkerThread::MilliSleep`
/// with `INT_ANY`): true when cancelled.
pub fn sleep_ms(ms: u64, cancel: &CancelToken) -> bool {
    let until = Instant::now() + Duration::from_millis(ms);
    loop {
        if cancel.is_cancelled() {
            return true;
        }
        let now = Instant::now();
        if now >= until {
            return false;
        }
        thread::sleep((until - now).min(WAIT_SLICE));
    }
}

/// The camera's and the mount's `Connect` (cam_indi.cpp:604,
/// scope_indi.cpp:272): watch the device, connect it through `CONNECTION`
/// once its `DRIVER_INFO` is there (`newProperty`, which sends it once), and
/// wait up to 30 s for `CONNECT` to be On. On success the link's generation
/// and the store's sequence number then: a later [`Link::switched_off`]
/// is the wrapper's to see.
pub fn connect_device(client: &IndiClient, device: &str, cancel: &CancelToken) -> Result<(u64, u64), String> {
    let generation = client
        .wait_up(cancel)
        .ok_or_else(|| format!("Cannot connect to INDI server {}:{}", client.host(), client.port()))?;
    client.watch(device);
    let mut asked = false;
    let r = client.wait(DEVICE_CONNECT_TIMEOUT, Some(cancel), |link| {
        if !link.up || link.generation != generation {
            return Some(Err("INDI server disconnected".to_string()));
        }
        if link.device_connected(device) == Some(true) {
            return Some(Ok((generation, link.store.seq())));
        }
        if !asked && link.store.property(device, "DRIVER_INFO").is_some_and(|p| p.kind == Kind::Text) {
            asked = true;
            client.set_driver_connection(link, device, true);
        }
        None
    });
    match r {
        Ok(r) => r,
        Err(WaitError::Cancelled) => Err("Connection cancelled".into()),
        Err(WaitError::Timeout) => Err(format!("Device {device} did not connect within 30 s")),
    }
}

/// `/indi/PulseMinWait`, default on: see [`TimedGuide`].
pub fn pulse_min_wait(p: &impl ProfileAccess) -> bool {
    p.get_bool("/indi/PulseMinWait", true)
}

/// The two pulse properties: `TELESCOPE_TIMED_GUIDE_NS` / `_WE`.
pub(crate) const TIMED_GUIDE_NS: &str = "TELESCOPE_TIMED_GUIDE_NS";
pub(crate) const TIMED_GUIDE_WE: &str = "TELESCOPE_TIMED_GUIDE_WE";

/// The property and members of a pulse: `(property, member pulsed, member set to 0)`.
pub(crate) fn timed_guide_members(dir: Dir) -> (&'static str, &'static str, &'static str) {
    match dir {
        Dir::East => (TIMED_GUIDE_WE, "TIMED_GUIDE_E", "TIMED_GUIDE_W"),
        Dir::West => (TIMED_GUIDE_WE, "TIMED_GUIDE_W", "TIMED_GUIDE_E"),
        Dir::North => (TIMED_GUIDE_NS, "TIMED_GUIDE_N", "TIMED_GUIDE_S"),
        Dir::South => (TIMED_GUIDE_NS, "TIMED_GUIDE_S", "TIMED_GUIDE_N"),
    }
}

/// How a timed pulse ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PulseEnd {
    Done,
    /// A pulse is still in flight: "Cannot guide with guide pulse in progress!".
    Busy,
    /// The device lacks one of the two properties.
    Missing,
    /// The cancel token: "interrupt requested".
    Interrupted,
    /// No answer within `max(dur + 2 s, 5 s)` (the PLAN's deadline; PHD2
    /// waits for ever).
    TimedOut,
}

/// The pulse protocol of PHD2's INDI camera and mount
/// (`ST4PulseGuideScope`, `ScopeINDI::Guide`): `guide_active` set with its
/// axis before the pulse is sent, the pulsed member at the duration in ms
/// and the opposite one at 0, then a wait for the axis to answer with
/// anything but Busy. "Answer" is an update after the request: the store's
/// sequence number is recorded before sending, and the property's must
/// pass it.
#[derive(Debug, Default)]
pub(crate) struct TimedGuide {
    /// `guide_active`: the property of the pulse in flight, the sequence
    /// number recorded before it was sent, and the link generation.
    active: Option<(&'static str, u64, u64)>,
    /// `/indi/PulseMinWait` (deliberate addition, PLAN): when the driver
    /// answers before the pulse could have ended, sleep the rest of it.
    pub min_wait: bool,
    min_wait_logged: bool,
}

impl TimedGuide {
    pub(crate) fn new(min_wait: bool) -> Self {
        TimedGuide { min_wait, ..Default::default() }
    }

    /// Whether `guide_active` is set. PHD2 clears it when the axis answers
    /// with anything but Busy, or the server goes (`ClearStatus`), and sets
    /// it when either axis reports Busy while it is clear (a pulse someone
    /// else sent).
    fn in_progress(&mut self, link: &Link, device: &str) -> bool {
        if let Some((prop, seq0, generation)) = self.active {
            let answered = link.store.property(device, prop).is_none_or(|p| p.updated > seq0 && p.state != PropState::Busy);
            if answered || !link.up || link.generation != generation {
                self.active = None;
            }
        }
        self.active.is_some()
            || [TIMED_GUIDE_NS, TIMED_GUIDE_WE]
                .iter()
                .any(|n| link.store.property(device, n).is_some_and(|p| p.state == PropState::Busy))
    }

    /// One pulse; `who` prefixes the PulseMinWait debug line.
    pub(crate) fn pulse(
        &mut self,
        client: &IndiClient,
        device: &str,
        dir: Dir,
        ms: u32,
        cancel: &CancelToken,
        who: &str,
        debug: &mut Vec<String>,
    ) -> PulseEnd {
        let (prop, pulsed, other) = timed_guide_members(dir);
        let start = Instant::now();
        let (seq0, generation) = {
            let mut link = client.lock();
            let has = |n: &str| link.store.property(device, n).is_some_and(|p| p.kind == Kind::Number);
            if !has(TIMED_GUIDE_NS) || !has(TIMED_GUIDE_WE) {
                return PulseEnd::Missing;
            }
            if self.in_progress(&link, device) {
                debug.push("Cannot guide with guide pulse in progress!".into());
                return PulseEnd::Busy;
            }
            let seq0 = link.store.seq();
            let generation = link.generation;
            self.active = Some((prop, seq0, generation));
            // Despite the INDI standard, every driver reads milliseconds.
            client.send_number(&mut link, device, prop, &[(pulsed, ms as f64), (other, 0.0)]);
            (seq0, generation)
        };
        let deadline = Duration::from_millis((ms as u64 + 2000).max(5000));
        let r = client.wait(deadline, Some(cancel), |link| {
            // The server went: PHD2's ClearStatus clears guide_active and
            // wakes the wait, which then reports the pulse done.
            if !link.up || link.generation != generation {
                return Some(());
            }
            let p = link.store.property(device, prop)?;
            // An update, not a redefinition: PHD2 clears guide_active in updateProperty only.
            (p.updated > seq0 && p.state != PropState::Busy).then_some(())
        });
        match r {
            Ok(()) => self.active = None,
            Err(WaitError::Cancelled) => {
                // guide_active stays set until the axis answers.
                debug.push("interrupt requested".into());
                return PulseEnd::Interrupted;
            }
            Err(WaitError::Timeout) => {
                // guide_active stays set too: PHD2 clears it only on the
                // axis' answer (it waits for that answer for ever), so
                // every later pulse is refused until the axis answers. The
                // deadline only gives the engine its thread back.
                debug.push(format!("{who}: no answer to a {ms} ms pulse within {} ms", deadline.as_millis()));
                return PulseEnd::TimedOut;
            }
        }
        let elapsed = start.elapsed().as_millis() as u64;
        if self.min_wait && elapsed < ms as u64 {
            if !self.min_wait_logged {
                self.min_wait_logged = true;
                debug.push(format!(
                    "{who}: the driver completed a {ms} ms pulse after {elapsed} ms; waiting out the pulse \
                     (PulseMinWait, logged once)"
                ));
            }
            sleep_ms(ms as u64 - elapsed, cancel);
        }
        PulseEnd::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indi::fake::{self, Fake};

    /// What the client asked, one line per element, for the assertions.
    fn asked(fake: &Fake) -> Vec<String> {
        fake.received().iter().map(fake::summary).collect()
    }

    fn wait_up(client: &IndiClient) -> u64 {
        client.wait_up(&CancelToken::new()).expect("the client connects")
    }

    #[test]
    fn hello_watch_ping_and_reconnect() {
        let fake = Fake::start(|e, peer| {
            if e.name == "getProperties" && e.attr("device") == Some("CCD Simulator") {
                peer.send(&fake::driver("CCD Simulator", 6, false));
            }
        });
        let client = IndiClient::start("127.0.0.1", fake.port, &[]);
        // Nothing watched yet: one request for every device.
        assert_eq!(wait_up(&client), 1);
        assert!(fake.wait_for(|r| r.len() == 1));
        client.watch("CCD Simulator");
        client.watch("CCD Simulator");
        assert!(fake.wait_for(|r| r.len() == 2));
        client
            .wait(Duration::from_secs(2), None, |l| l.store.property("CCD Simulator", "DRIVER_INFO").map(|_| ()))
            .unwrap();
        assert_eq!(client.lock().device_connected("CCD Simulator"), Some(false));

        fake.push("<pingRequest uid='p42'/>\n");
        assert!(fake.wait_for(|r| r.len() == 3));
        assert_eq!(
            asked(&fake),
            ["getProperties version=1.7", "getProperties version=1.7 device=CCD Simulator", "pingReply uid=p42",]
        );

        // The server drops: the client dials again 250 ms later and asks
        // for the watched device only.
        let seq = client.lock().store.seq();
        let t = Instant::now();
        fake.drop_conns();
        client.wait(Duration::from_secs(3), None, |l| (l.is_up() && l.generation() == 2).then_some(())).unwrap();
        assert!(t.elapsed() >= MIN_BACKOFF);
        assert!(fake.wait_for(|r| r.len() == 4));
        assert_eq!(fake.accepted(), 2);
        assert_eq!(asked(&fake)[3], "getProperties version=1.7 device=CCD Simulator");
        let again = client
            .wait(Duration::from_secs(2), None, |l| l.store.property("CCD Simulator", "DRIVER_INFO").map(|p| p.seq))
            .unwrap();
        // Defined again (the store forgot the device: a second definition
        // would have been ignored), and the sequence kept growing.
        assert!(again > seq);
        let debug = client.take_debug();
        assert_eq!(debug.len(), 3);
        assert!(debug[1].starts_with("INDI: lost the connection to 127.0.0.1:"));
        client.shutdown();
    }

    #[test]
    fn the_chooser_filters_by_interface() {
        let fake = Fake::start(|e, peer| {
            if e.name == "getProperties" {
                let mut all = String::new();
                for (dev, bits) in [
                    ("CCD Simulator", 0x6),        // CCD | GUIDER: a camera with ST-4
                    ("Telescope Simulator", 0x5),  // TELESCOPE | GUIDER
                    ("SX AO", 0x104),              // AO | GUIDER
                    ("Rotator Simulator", 0x1000), // ROTATOR
                    ("Focuser Simulator", 0x8),    // FOCUSER
                    ("Guide Port", 0x4),           // GUIDER alone (an ST-4 box)
                ] {
                    all += &fake::driver(dev, bits, dev == "Telescope Simulator");
                }
                // A device that never defines DRIVER_INFO stays listed.
                all += &fake::def_switch(
                    "Mystery",
                    "CONNECTION",
                    "OneOfMany",
                    &[("CONNECT", false), ("DISCONNECT", true)],
                );
                peer.send(&all);
            }
        });
        let client = IndiClient::start("127.0.0.1", fake.port, &[]);
        client.wait(Duration::from_secs(5), None, |l| (l.store.devices().len() == 7).then_some(())).unwrap();
        let names = |role| client.devices(role).into_iter().map(|d| d.name).collect::<Vec<_>>();
        assert_eq!(names(Role::Camera), ["CCD Simulator", "Mystery"]);
        assert_eq!(names(Role::Mount), ["Telescope Simulator", "Guide Port", "Mystery"]);
        assert_eq!(names(Role::AuxMount), ["Telescope Simulator", "Mystery"]);
        assert_eq!(names(Role::Ao), ["SX AO", "Mystery"]);
        assert_eq!(names(Role::Rotator), ["Rotator Simulator", "Mystery"]);
        let scope = &client.devices(Role::Mount)[0];
        assert_eq!((scope.interfaces, scope.connected), (5, true));
        // Nothing watched: the hello asks for every device, once.
        assert_eq!(asked(&fake), ["getProperties version=1.7"]);
        client.shutdown();
    }

    #[test]
    fn connection_switch_blob_mode_and_queue() {
        let fake = Fake::start(|e, peer| {
            if e.name == "getProperties" {
                let blob = fake::def_blob("CCD Simulator", "CCD1", &["CCD1", "OTHER"]);
                peer.send(&(fake::driver("CCD Simulator", 6, false) + &blob));
            }
        });
        let client = IndiClient::start("127.0.0.1", fake.port, &[]);
        client.watch("CCD Simulator");
        // Asked before the definition: the reader enables it when it comes.
        client.want_blob("CCD Simulator", "CCD1");
        client.wait(Duration::from_secs(2), None, |l| l.store.property("CCD Simulator", "CCD1").map(|_| ())).unwrap();
        {
            let link = client.lock();
            assert!(client.set_driver_connection(&link, "CCD Simulator", true));
            // Disconnecting a device whose DISCONNECT is On sends nothing.
            assert!(client.set_driver_connection(&link, "CCD Simulator", false));
            assert!(!client.set_driver_connection(&link, "Nobody", true));
        }
        assert!(fake.wait_for(|r| r.len() == 3));
        assert_eq!(
            asked(&fake),
            [
                "getProperties version=1.7 device=CCD Simulator",
                "enableBLOB device=CCD Simulator name=CCD1 Also",
                "newSwitchVector CCD Simulator.CONNECTION CONNECT=On",
            ]
        );
        // Ten frames: the queue keeps the newest eight; BLOBs of another
        // member stay out of the camera's take.
        let mut frames = String::new();
        for i in 0..10u8 {
            frames += &fake::set_blob("CCD Simulator", "CCD1", "CCD1", ".fits", &[i; 3]);
        }
        frames += &fake::set_blob("CCD Simulator", "CCD1", "OTHER", ".fits", &[99]);
        fake.push(&frames);
        client
            .wait(Duration::from_secs(2), None, |l| (l.blobs.last().is_some_and(|b| b.member == "OTHER")).then_some(()))
            .unwrap();
        let taken = client.lock().take_blobs("CCD Simulator", "CCD1");
        assert_eq!(taken.iter().map(|b| b.data[0]).collect::<Vec<_>>(), [3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(client.lock().blobs.len(), 1);
        client.drop_blob("CCD Simulator", "CCD1");
        assert!(client.lock().blobs.is_empty());
        assert!(fake.wait_for(|r| r.len() == 4));
        assert_eq!(asked(&fake)[3], "enableBLOB device=CCD Simulator name=CCD1 Never");
        client.shutdown();
    }

    #[test]
    fn a_device_is_connected_once_its_driver_info_is_known() {
        let fake = Fake::start(|e, peer| {
            if e.name == "getProperties" {
                // CONNECTION comes first, DRIVER_INFO 100 ms later.
                peer.send(&fake::def_switch(
                    "Telescope Simulator",
                    "CONNECTION",
                    "OneOfMany",
                    &[("CONNECT", false), ("DISCONNECT", true)],
                ));
                peer.send_after(100, fake::driver("Telescope Simulator", 5, false));
            }
            if let Some(reply) = fake::connection_reply(e) {
                peer.send(&reply);
            }
        });
        // The profile's gear is watched from the first connection.
        let client = IndiClient::start("127.0.0.1", fake.port, &["Telescope Simulator"]);
        let t = Instant::now();
        let generation = |r: Result<(u64, u64), String>| r.map(|(g, _)| g);
        assert_eq!(generation(connect_device(&client, "Telescope Simulator", &CancelToken::new())), Ok(1));
        assert!(t.elapsed() >= Duration::from_millis(100));
        assert_eq!(
            asked(&fake),
            [
                "getProperties version=1.7 device=Telescope Simulator",
                "newSwitchVector Telescope Simulator.CONNECTION CONNECT=On",
            ]
        );
        // Connected already: nothing is sent.
        assert_eq!(generation(connect_device(&client, "Telescope Simulator", &CancelToken::new())), Ok(1));
        assert_eq!(fake.received().len(), 2);
        client.shutdown();
    }

    #[test]
    fn waits_end_on_cancel_and_on_timeout() {
        // No server at all: the client never comes up. Port 0 refuses every
        // dial; a port freed by a listener could be bound again by another
        // test's fake, which would then count this client's hello.
        let port = 0;
        let client = IndiClient::start("127.0.0.1", port, &[]);
        let cancel = CancelToken::new();
        let t = Instant::now();
        assert_eq!(client.wait(Duration::from_millis(250), Some(&cancel), |_| None::<()>), Err(WaitError::Timeout));
        assert!(t.elapsed() >= Duration::from_millis(250));
        cancel.cancel();
        assert_eq!(client.wait(Duration::from_secs(10), Some(&cancel), |_| None::<()>), Err(WaitError::Cancelled));
        assert_eq!(
            connect_device(&client, "CCD Simulator", &cancel),
            Err("Cannot connect to INDI server 127.0.0.1:".to_string() + &port.to_string())
        );
        assert!(!client.send("<x/>"));
        let t = Instant::now();
        assert!(sleep_ms(1000, &cancel));
        assert!(t.elapsed() < Duration::from_millis(50));
        client.shutdown();
    }
}
