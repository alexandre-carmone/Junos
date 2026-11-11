//! The daemon around the engine: the engine's thread, its command queue,
//! its events and snapshots, the gear it connects, its files.
//!
//! The [`Engine`] runs on one `std::thread` (PHD2's worker thread and main
//! thread in one): it takes the queued jobs first, then steps while it has
//! work ([`Engine::has_work`]), and otherwise waits for a job at most until
//! the next 1 s tick (every tick a long exposure held up is caught up on
//! at once, so the Guiding Assistant counts real seconds). It closes the
//! guide log when it ends. A panic on it is logged and published in the
//! snapshot ([`Snapshot::engine_error`], [`Daemon::failed`]), and the
//! process exits rather than serve a dead engine. Every command comes with a reply channel and is
//! answered after the snapshot was published and its events went out on
//! the broadcast (in that order: PHD2 updates its state before it
//! notifies), so a client that drains the events before writing the reply
//! keeps PHD2's order. The frames do not ride the broadcast, whose backlog
//! a client that stops reading would fill with them: the latest one is in
//! a `watch` ([`Daemon::frames`]). The snapshot ([`Snapshot`], in a `watch`) is
//! published after every job, step and tick: the read-only RPCs and the
//! relay read it without touching the engine thread, which may be inside
//! an exposure. Each tick also sends [`Command::Tick`], writes the profile
//! when it changed and polls the gear ([`gear`]).
//!
//! Commands wait for the frame in progress. Three cut it short through the
//! engine's cancel token, so a blocking capture or pulse returns at once:
//! `stop_capture` while capturing ([`StopRequest`], which keeps PHD2's
//! order of the stop and the interrupted frame), `set_connected false` (not
//! while capturing: PHD2 refuses that, and the refusal is answered from the
//! snapshot at once), and [`Daemon::shutdown`].
//!
//! `set_connected true` runs off the engine thread ([`gear`]: an INDI
//! device may take 30 s); meanwhile the engine is told it is connecting, and
//! `loop` and `guide` answer "all equipment must be connected first". One
//! connect runs at a time: a `set_connected true` that comes while another
//! runs (the INDI reconnect's, another client's) waits for it, then finds
//! the gear connected or tries again what failed. PHD2 answers its modal
//! dialog's error there, which Ekos takes for a disconnect.
//!
//! [`files`] puts the engine's files on the disk.
//!
//! Ported from PHD2 src/gear_dialog.cpp (ConnectAll, DisconnectAll, Shutdown), src/myframe.cpp (OnClose) — BSD-3-Clause, see THIRD_PARTY.md

pub mod files;
mod gear;

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{broadcast, oneshot, watch};

use self::gear::{Choices, Rig};
use crate::engine::image::UsImage;
use crate::engine::traits::{CancelToken, SystemClock};
use crate::engine::{
    CmdError, Command, Engine, EngineConfig, EngineEvent, Files, GearRole, Reply, Snapshot, StopRequest,
};
use crate::indi::client::IndiClient;
use crate::profile::ProfileStore;
use crate::server::rpc::INTERNAL_ERROR;

/// The engine thread's tick: the Guiding Assistant's countdown, the profile
/// written, the gear polled.
const TICK: Duration = Duration::from_secs(1);
/// Events kept for a subscriber that lags.
const EVENT_BACKLOG: usize = 4096;

/// What the daemon runs with.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    pub engine: EngineConfig,
    /// The gear simulator instead of INDI (`--sim`).
    pub sim: bool,
    /// The INDI server when the profile names none (`--indi-host`,
    /// `--indi-port`).
    pub indi_host: String,
    pub indi_port: u16,
}

/// Work for the engine thread, run on the engine and the rig; what it
/// returns sends the answer, once the snapshot is published.
type Task = Box<dyn FnOnce(&mut Engine, &mut Rig) -> Box<dyn FnOnce() + Send> + Send>;

/// `f` as a task answering on `tx`.
fn task<R: Send + 'static>(f: impl FnOnce(&mut Engine, &mut Rig) -> R + Send + 'static, tx: oneshot::Sender<R>) -> Task {
    Box::new(move |e, rig| {
        let r = f(e, rig);
        Box::new(move || {
            let _ = tx.send(r);
        })
    })
}

/// The event server's port, or its error.
#[derive(Debug, Default)]
struct ServerPort {
    port: Option<u16>,
    error: Option<String>,
}

enum Job {
    Command(Command, oneshot::Sender<Reply>),
    Run(Task),
    Exit,
}

struct Inner {
    opts: DaemonOptions,
    jobs: mpsc::Sender<Job>,
    events: broadcast::Sender<Arc<EngineEvent>>,
    snapshot: watch::Receiver<Arc<Snapshot>>,
    /// The latest frame, published after its snapshot.
    frames: watch::Receiver<Option<Arc<UsImage>>>,
    /// The engine thread's, shared for the event server's port.
    snapshot_tx: Arc<watch::Sender<Arc<Snapshot>>>,
    /// Held while a job's events and snapshot go out, so that a new
    /// client's catch-up and its events are taken at one point
    /// ([`Daemon::subscribe_with_snapshot`]); it holds the event server's
    /// port, which every snapshot carries.
    publishing: Arc<Mutex<ServerPort>>,
    stop: StopRequest,
    cancel: CancelToken,
    /// A `set_connected true` is running (PHD2's modal gear dialog).
    connecting: AtomicBool,
    /// Held by the connect running, so that connects run one at a time.
    connect_one: Mutex<()>,
    /// Cuts the running connect short (shutdown).
    connect_cancel: Mutex<CancelToken>,
    /// Counts the shutdowns: a connect begun before one is given up, and
    /// its gear is not attached.
    epoch: AtomicU64,
    /// The INDI connection of the gear last connected, for the device list.
    indi: Mutex<Option<IndiClient>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// The handle the event server and the relay use: commands with their
/// replies, the events, the snapshot. Clones share the daemon.
#[derive(Clone)]
pub struct Daemon(Arc<Inner>);

fn internal() -> CmdError {
    CmdError::new(INTERNAL_ERROR, "internal error")
}

/// Clears the `connecting` flags when the connect ends, however it ends
/// (a device that panicked included): the daemon's, and the engine's while
/// `engine` (told, and not yet cleared by the attach).
struct Connecting<'a> {
    d: &'a Daemon,
    engine: bool,
}

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        if self.engine {
            let (tx, _) = oneshot::channel();
            let _ = self.d.0.jobs.send(Job::Command(Command::Connecting(false), tx));
        }
        self.d.0.connecting.store(false, Ordering::SeqCst);
    }
}

fn cancelled() -> CmdError {
    CmdError::new(1, "equipment connection cancelled")
}

impl Daemon {
    /// Starts the engine thread with `store` (its current profile selected)
    /// and `files`.
    pub fn start(opts: DaemonOptions, store: ProfileStore, files: Box<dyn Files>) -> Daemon {
        let engine = Engine::new(opts.engine.clone(), store, Box::new(SystemClock::default()), files);
        let (jobs, rx) = mpsc::channel();
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        let (snap_tx, snapshot) = watch::channel(Arc::new(engine.snapshot()));
        let snap_tx = Arc::new(snap_tx);
        let (frame_tx, frames) = watch::channel(None);
        let publishing = Arc::new(Mutex::new(ServerPort::default()));
        let inner = Arc::new(Inner {
            opts,
            jobs,
            events: events.clone(),
            snapshot,
            frames,
            snapshot_tx: snap_tx.clone(),
            publishing: publishing.clone(),
            stop: engine.stop_request(),
            cancel: engine.cancel_token(),
            connecting: AtomicBool::new(false),
            connect_one: Mutex::default(),
            connect_cancel: Mutex::default(),
            epoch: AtomicU64::new(0),
            indi: Mutex::default(),
            thread: Mutex::new(None),
        });
        let daemon = Daemon(inner);
        let weak = Arc::downgrade(&daemon.0);
        let t = thread::Builder::new()
            .name("guider-engine".into())
            .spawn(move || run(engine, rx, Outputs { events, snapshot: snap_tx, frame: frame_tx, publishing }, weak))
            .expect("spawn the engine thread");
        *lock(&daemon.0.thread) = Some(t);
        daemon
    }

    pub fn options(&self) -> &DaemonOptions {
        &self.0.opts
    }

    /// The INDI connection of the gear last connected, `None` before a
    /// connect, after a disconnect and with `--sim`.
    pub fn indi_client(&self) -> Option<IndiClient> {
        lock(&self.0.indi).clone()
    }

    /// The events, from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<EngineEvent>> {
        self.0.events.subscribe()
    }

    /// The snapshot, as it changes.
    pub fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.0.snapshot.clone()
    }

    /// The latest frame, as frames come (`None` before the first).
    pub fn frames(&self) -> watch::Receiver<Option<Arc<UsImage>>> {
        self.0.frames.clone()
    }

    /// The latest snapshot.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.0.snapshot.borrow().clone()
    }

    /// The events from now on and the snapshot as it stands, taken
    /// together: every event before the snapshot is in it, every one after
    /// comes on the receiver (a client's catch-up, `send_catchup_events`).
    pub fn subscribe_with_snapshot(&self) -> (broadcast::Receiver<Arc<EngineEvent>>, Arc<Snapshot>) {
        let _publishing = lock(&self.0.publishing);
        (self.0.events.subscribe(), self.snapshot())
    }

    /// Resolves with the panic's text once the engine thread died of one.
    pub async fn failed(&self) -> String {
        let mut rx = self.snapshots();
        match rx.wait_for(|s| s.engine_error.is_some()).await {
            Ok(s) => s.engine_error.clone().unwrap_or_default(),
            // The sender lives as long as the daemon.
            Err(_) => std::future::pending().await,
        }
    }

    /// The event server's port once it listens (`port`), or why it does
    /// not (`error`), published at once in the snapshot.
    pub fn set_event_server(&self, port: Option<u16>, error: Option<String>) {
        let mut p = lock(&self.0.publishing);
        *p = ServerPort { port, error };
        self.0.snapshot_tx.send_modify(|s| {
            let s = Arc::make_mut(s);
            (s.port, s.port_error) = (p.port, p.error.clone());
        });
    }

    /// One command, answered once the engine handled it. `stop_capture`
    /// while capturing cuts the frame in progress short first.
    pub async fn command(&self, cmd: Command) -> Reply {
        if matches!(cmd, Command::StopCapture) && self.snapshot().capture_active {
            self.0.stop.request();
        }
        let (tx, rx) = oneshot::channel();
        self.0.jobs.send(Job::Command(cmd, tx)).map_err(|_| internal())?;
        rx.await.map_err(|_| internal())?
    }

    /// `f` on the engine thread, between frames (the profile methods);
    /// `None` when the engine thread is gone.
    pub async fn with_engine<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine) -> R + Send + 'static) -> Option<R> {
        self.run(move |e, _| f(e)).await
    }

    async fn run<R: Send + 'static>(&self, f: impl FnOnce(&mut Engine, &mut Rig) -> R + Send + 'static) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.0.jobs.send(Job::Run(task(f, tx))).ok()?;
        rx.await.ok()
    }

    /// [`run`](Self::run) from a thread outside the runtime (the connector).
    fn run_blocking<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Engine, &mut Rig) -> R + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        self.0.jobs.send(Job::Run(task(f, tx))).ok()?;
        rx.blocking_recv().ok()
    }

    /// `set_connected`: `GearDialog::ConnectAll` or `DisconnectAll`, with
    /// their error texts.
    pub async fn set_connected(&self, on: bool) -> Reply {
        if on {
            let d = self.clone();
            tokio::task::spawn_blocking(move || d.connect_all()).await.map_err(|_| internal())?
        } else {
            self.disconnect_all().await
        }
    }

    /// `ConnectAll`, on the caller's thread: the gear the profile chooses,
    /// connected camera → AO → mount → aux mount → rotator, then handed to
    /// the engine. Also the INDI reconnect's.
    fn connect_all(&self) -> Reply {
        let epoch = self.0.epoch.load(Ordering::SeqCst);
        let _one = lock(&self.0.connect_one);
        if self.0.epoch.load(Ordering::SeqCst) != epoch {
            return Err(cancelled());
        }
        self.0.connecting.store(true, Ordering::SeqCst);
        let mut connecting = Connecting { d: self, engine: false };
        let opts = self.0.opts.clone();
        let (choices, done, client) = self
            .run_blocking(move |e, rig| (Choices::read(e, &opts), rig.all_connected(), rig.client()))
            .ok_or_else(internal)?;
        if done {
            // Everything already connected.
            return Ok(json!(0));
        }
        let (tx, rx) = oneshot::channel();
        self.0.jobs.send(Job::Command(Command::Connecting(true), tx)).map_err(|_| internal())?;
        rx.blocking_recv().map_err(|_| internal())??;
        connecting.engine = true;

        let cancel = CancelToken::new();
        *lock(&self.0.connect_cancel) = cancel.clone();
        // A shutdown between the check above and the token stored.
        if self.0.epoch.load(Ordering::SeqCst) != epoch {
            cancel.cancel();
        }
        let (rig, gear, notes) = Rig::connect(&choices, client, &cancel);
        let fail = rig.failures();
        let d = self.clone();
        let reply = self.run_blocking(move |e, old| {
            for n in notes {
                n.apply(e);
            }
            let reply = if d.0.epoch.load(Ordering::SeqCst) != epoch {
                Err(cancelled())
            } else {
                e.handle(Command::Attach(gear))
            };
            if reply.is_ok() {
                *lock(&d.0.indi) = rig.client();
                *old = rig;
            } else {
                rig.discard(old, e);
            }
            let _ = e.handle(Command::Connecting(false));
            reply
        });
        connecting.engine = reply.is_none();
        reply.ok_or_else(internal)??;
        if fail.is_empty() { Ok(json!(0)) } else { Err(CmdError::new(1, format!("equipment failed to connect:{fail}"))) }
    }

    /// `connect_device`: one device's Connect or Disconnect button in the
    /// gear dialog (`OnButtonConnect<what>`, `OnButtonDisconnect<what>`),
    /// the rest of the gear left as it is; refused while capturing, as
    /// `set_connected` is. A device that fails to connect answers
    /// `ConnectAll`'s text for it.
    pub async fn connect_device(&self, role: GearRole, on: bool) -> Reply {
        if on {
            let d = self.clone();
            return tokio::task::spawn_blocking(move || d.connect_one(role)).await.map_err(|_| internal())?;
        }
        if self.0.connecting.load(Ordering::SeqCst) {
            return Err(CmdError::new(1, "cannot disconnect equipment when Connect Equipment dialog is open"));
        }
        self.run(move |e, rig| {
            let reply = e.handle(Command::DetachDevice(role));
            if reply.is_ok() {
                rig.disconnect(role, e);
            }
            reply
        })
        .await
        .ok_or_else(internal)?
    }

    /// [`connect_device`](Self::connect_device) on: the rig made from the
    /// profile's choices when nothing in it is connected, then the device
    /// connected on the caller's thread and handed to the engine.
    fn connect_one(&self, role: GearRole) -> Reply {
        let epoch = self.0.epoch.load(Ordering::SeqCst);
        let _one = lock(&self.0.connect_one);
        if self.0.epoch.load(Ordering::SeqCst) != epoch {
            return Err(cancelled());
        }
        self.0.connecting.store(true, Ordering::SeqCst);
        let mut connecting = Connecting { d: self, engine: false };
        let (tx, rx) = oneshot::channel();
        self.0.jobs.send(Job::Command(Command::Connecting(true), tx)).map_err(|_| internal())?;
        rx.blocking_recv().map_err(|_| internal())??;
        connecting.engine = true;
        let failed = || CmdError::new(1, format!("equipment failed to connect: {}", gear::failure_name(role)));
        let opts = self.0.opts.clone();
        let taken = self.run_blocking(move |e, rig| {
            if !rig.any_connected() {
                let new = Rig::build(&Choices::read(e, &opts), rig.client());
                std::mem::replace(rig, new).discard(rig, e);
            }
            rig.take(role)
        });
        let Some(mut dev) = taken.ok_or_else(internal)?.map_err(|()| failed())? else {
            // Connected already.
            return Ok(json!(0));
        };

        let cancel = CancelToken::new();
        *lock(&self.0.connect_cancel) = cancel.clone();
        if self.0.epoch.load(Ordering::SeqCst) != epoch {
            cancel.cancel();
        }
        let mut notes = Vec::new();
        gear::connect_device(role, &mut dev, &cancel, &mut notes);
        let d = self.clone();
        let reply = self.run_blocking(move |e, rig| {
            for n in notes {
                n.apply(e);
            }
            let reply = match rig.put(role, dev) {
                _ if d.0.epoch.load(Ordering::SeqCst) != epoch => Err(cancelled()),
                Some(gear) => e.handle(Command::AttachDevice(role, gear)).map(|_| true),
                None => Ok(false),
            };
            if !matches!(reply, Ok(true)) {
                rig.disconnect(role, e);
            }
            *lock(&d.0.indi) = rig.client();
            let _ = e.handle(Command::Connecting(false));
            reply
        });
        connecting.engine = reply.is_none();
        if reply.ok_or_else(internal)?? { Ok(json!(0)) } else { Err(failed()) }
    }

    /// `DisconnectAll`. Refused at once while capturing, before the engine
    /// thread is free again; otherwise a pulse in flight is cut short.
    async fn disconnect_all(&self) -> Reply {
        if self.0.connecting.load(Ordering::SeqCst) {
            return Err(CmdError::new(1, "cannot disconnect equipment when Connect Equipment dialog is open"));
        }
        // Nothing connected answers 0 before the capture check, as in PHD2.
        let snap = self.snapshot();
        let devices = [&snap.camera, &snap.mount, &snap.aux_mount, &snap.ao, &snap.rotator];
        if snap.capture_active && devices.iter().any(|d| d.as_ref().is_some_and(|d| d.connected)) {
            return Err(CmdError::new(1, "cannot disconnect equipment while capture active"));
        }
        self.0.cancel.cancel();
        *lock(&self.0.indi) = None;
        self.run(|e, rig| {
            if !rig.any_connected() {
                // Nothing connected (the gear dropped with the INDI
                // server): the connection closed all the same, so that it
                // does not bring the gear back.
                rig.disconnect_all(e);
                return Ok(json!(0));
            }
            let reply = e.handle(Command::Detach);
            if reply.is_ok() {
                rig.disconnect_all(e);
            }
            reply
        })
        .await
        .ok_or_else(internal)?
    }

    /// `set_profile`: `GearDialog::SetProfile`, with its error texts. The
    /// engine makes its gear anew from the new profile, as `LoadGearChoices`
    /// does; the rig and its INDI connection go, and the new profile's
    /// devices are connected at the next `set_connected`.
    pub async fn set_profile(&self, id: i32) -> Reply {
        let connecting = self.0.connecting.load(Ordering::SeqCst);
        let d = self.clone();
        self.run(move |e, rig| {
            if id == e.store().current_profile_id() {
                return Ok(json!(0));
            }
            if connecting {
                return Err(CmdError::new(1, "cannot set profile when Connect Equipment dialog is open"));
            }
            if rig.any_connected() {
                return Err(CmdError::new(1, "cannot set profile when equipment is connected"));
            }
            if !e.store().profile_exists(id) {
                return Err(CmdError::new(1, "invalid profile id"));
            }
            let name = e.store().profile_name(id);
            e.store().set_current_profile(&name);
            rig.disconnect_all(e);
            *rig = Rig::default();
            *lock(&d.0.indi) = None;
            e.handle(Command::ProfileChanged)
        })
        .await
        .ok_or_else(internal)?
    }

    /// PHD2's default file folder (`GetDefaultFileDir`): where
    /// `export_config_settings` writes.
    pub fn data_dir(&self) -> &str {
        &self.0.opts.engine.data_dir
    }

    /// The `shutdown` method without the exit (`MyFrame::OnClose`): capture
    /// stopped, a connect in progress given up, every device disconnected
    /// (`GearDialog::Shutdown`, no capture check left to fail).
    pub async fn shutdown(&self) {
        self.0.epoch.fetch_add(1, Ordering::SeqCst);
        lock(&self.0.connect_cancel).cancel();
        let _ = self.command(Command::StopCapture).await;
        self.0.cancel.cancel();
        // The connect given up has ended (and its gear was not attached).
        let d = self.clone();
        let _ = tokio::task::spawn_blocking(move || drop(lock(&d.0.connect_one))).await;
        *lock(&self.0.indi) = None;
        self.run(|e, rig| {
            e.debug("MyFrame::OnClose proceeding");
            e.debug("Shutdown: forced=0");
            if rig.any_connected() {
                let _ = e.handle(Command::Detach);
            }
            // The INDI connection closed too: it would bring the gear back.
            rig.disconnect_all(e);
            e.debug("Shutdown complete");
        })
        .await;
    }

    /// Ends the engine thread once its queue is done, writing the profile.
    pub fn close(&self) {
        let _ = self.0.jobs.send(Job::Exit);
        if let Some(t) = lock(&self.0.thread).take() {
            let _ = t.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the engine thread publishes.
struct Outputs {
    events: broadcast::Sender<Arc<EngineEvent>>,
    snapshot: Arc<watch::Sender<Arc<Snapshot>>>,
    frame: watch::Sender<Option<Arc<UsImage>>>,
    publishing: Arc<Mutex<ServerPort>>,
}

impl Outputs {
    /// The snapshot, then the frame, then the events, under `publishing`.
    fn publish(&self, engine: &mut Engine, evs: Vec<EngineEvent>) {
        let mut snap = engine.snapshot();
        let port = lock(&self.publishing);
        (snap.port, snap.port_error) = (port.port, port.error.clone());
        self.snapshot.send_replace(Arc::new(snap));
        for e in evs {
            if let EngineEvent::FrameReady(img) = e {
                self.frame.send_replace(Some(img));
            } else {
                // No subscriber is not an error.
                let _ = self.events.send(Arc::new(e));
            }
        }
    }
}

/// The engine thread: [`serve`], then the profile written and the guide
/// log closed, even after a panic (as far as they go).
fn run(mut engine: Engine, jobs: mpsc::Receiver<Job>, out: Outputs, daemon: std::sync::Weak<Inner>) {
    let served = panic::catch_unwind(AssertUnwindSafe(|| serve(&mut engine, &jobs, &out, &daemon)));
    let closed = panic::catch_unwind(AssertUnwindSafe(|| {
        persist(&mut engine);
        engine.close_logs();
    }));
    if let Err(p) = served.and(closed) {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        tracing::error!("the guider engine stopped: {msg}");
        out.snapshot.send_modify(|s| Arc::make_mut(s).engine_error = Some(msg));
    }
}

/// The engine thread's loop, until [`Job::Exit`].
fn serve(engine: &mut Engine, jobs: &mpsc::Receiver<Job>, out: &Outputs, daemon: &std::sync::Weak<Inner>) {
    let publish = |engine: &mut Engine, evs: Vec<EngineEvent>| out.publish(engine, evs);
    let mut rig = Rig::default();
    let mut last_tick = Instant::now();
    loop {
        let wait = if engine.has_work() { Duration::ZERO } else { TICK.saturating_sub(last_tick.elapsed()) };
        match jobs.recv_timeout(wait) {
            Ok(Job::Command(cmd, reply)) => {
                let r = engine.handle(cmd);
                let evs = engine.take_events();
                publish(engine, evs);
                let _ = reply.send(r);
            }
            Ok(Job::Run(task)) => {
                let reply = task(engine, &mut rig);
                engine.finish();
                let evs = engine.take_events();
                publish(engine, evs);
                reply();
            }
            Ok(Job::Exit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if engine.has_work() {
                    let evs = engine.step();
                    publish(engine, evs);
                }
            }
        }
        // PHD2's 1 s timer: as many ticks as seconds went by.
        let ticks = last_tick.elapsed().as_secs() as u32;
        if ticks > 0 {
            last_tick += TICK * ticks;
            for _ in 0..ticks {
                let _ = engine.handle(Command::Tick);
            }
            if let Some(d) = daemon.upgrade() {
                rig.poll(engine, &Daemon(d));
            }
            engine.finish();
            persist(engine);
            let evs = engine.take_events();
            publish(engine, evs);
        }
    }
}

/// The profile written when it changed, as PHD2's config is at its
/// flushes and exit (a daemon may be stopped any time).
fn persist(engine: &mut Engine) {
    if let Err(e) = engine.store().persist() {
        tracing::warn!("cannot write the config: {e}");
    }
}

#[cfg(test)]
pub(crate) mod tests;
