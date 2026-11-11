//! The daemon around the engine: the engine's thread, its command queue,
//! its events and snapshots, the gear it connects, its files.
//!
//! The [`Engine`] runs on one `std::thread` (PHD2's worker thread and main
//! thread in one): it takes the queued jobs first, then steps while it has
//! work ([`Engine::has_work`]), and otherwise waits for a job at most until
//! the next 1 s tick. Every command comes with a reply channel and is
//! answered after its events went out on the broadcast and the snapshot
//! was published, so a client that drains the events before writing the
//! reply keeps PHD2's order. The snapshot ([`Snapshot`], in a `watch`) is
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
//! `loop` and `guide` answer "all equipment must be connected first".
//!
//! [`files`] puts the engine's files on the disk.
//!
//! Ported from PHD2 src/gear_dialog.cpp (ConnectAll, DisconnectAll, Shutdown), src/myframe.cpp (OnClose) — BSD-3-Clause, see THIRD_PARTY.md

pub mod files;
mod gear;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::{broadcast, oneshot, watch};

use self::gear::{Choices, Rig};
use crate::engine::traits::{CancelToken, SystemClock};
use crate::engine::{CmdError, Command, Engine, EngineConfig, EngineEvent, Files, Reply, Snapshot, StopRequest};
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
    /// Cuts the running connect short (shutdown).
    connect_cancel: Mutex<CancelToken>,
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

/// Clears the `connecting` flag when the connect ends, however it ends.
struct Connecting<'a>(&'a AtomicBool);

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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
        let publishing = Arc::new(Mutex::new(ServerPort::default()));
        let inner = Arc::new(Inner {
            opts,
            jobs,
            events: events.clone(),
            snapshot,
            snapshot_tx: snap_tx.clone(),
            publishing: publishing.clone(),
            stop: engine.stop_request(),
            cancel: engine.cancel_token(),
            connecting: AtomicBool::new(false),
            connect_cancel: Mutex::default(),
            indi: Mutex::default(),
            thread: Mutex::new(None),
        });
        let daemon = Daemon(inner);
        let weak = Arc::downgrade(&daemon.0);
        let t = thread::Builder::new()
            .name("guider-engine".into())
            .spawn(move || run(engine, rx, events, snap_tx, publishing, weak))
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
        if self.0.connecting.swap(true, Ordering::SeqCst) {
            return Err(CmdError::new(1, "cannot connect equipment when Connect Equipment dialog is open"));
        }
        let _connecting = Connecting(&self.0.connecting);
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

        let cancel = CancelToken::new();
        *lock(&self.0.connect_cancel) = cancel.clone();
        let (rig, gear, notes) = Rig::connect(&choices, client, &cancel);
        let fail = rig.failures();
        *lock(&self.0.indi) = rig.client();
        self.run_blocking(move |e, old| {
            for n in notes {
                n.apply(e);
            }
            let reply = e.handle(Command::Attach(gear));
            let mut rig = rig;
            if reply.is_ok() {
                *old = rig;
            } else {
                rig.disconnect_all(e);
            }
            let _ = e.handle(Command::Connecting(false));
            reply
        })
        .ok_or_else(internal)??;
        if fail.is_empty() { Ok(json!(0)) } else { Err(CmdError::new(1, format!("equipment failed to connect:{fail}"))) }
    }

    /// `DisconnectAll`. Refused at once while capturing, before the engine
    /// thread is free again; otherwise a pulse in flight is cut short.
    async fn disconnect_all(&self) -> Reply {
        if self.0.connecting.load(Ordering::SeqCst) {
            return Err(CmdError::new(1, "cannot disconnect equipment when Connect Equipment dialog is open"));
        }
        if self.snapshot().capture_active {
            return Err(CmdError::new(1, "cannot disconnect equipment while capture active"));
        }
        self.0.cancel.cancel();
        *lock(&self.0.indi) = None;
        self.run(|e, rig| {
            if !rig.any_connected() {
                // Nothing connected.
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
    /// gear choices are read again at the next connect.
    pub async fn set_profile(&self, id: i32) -> Reply {
        let connecting = self.0.connecting.load(Ordering::SeqCst);
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
        lock(&self.0.connect_cancel).cancel();
        let _ = self.command(Command::StopCapture).await;
        self.0.cancel.cancel();
        self.run(|e, rig| {
            e.debug("MyFrame::OnClose proceeding");
            e.debug("Shutdown: forced=0");
            if rig.any_connected() {
                let _ = e.handle(Command::Detach);
                rig.disconnect_all(e);
            }
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

/// The engine thread.
fn run(
    mut engine: Engine,
    jobs: mpsc::Receiver<Job>,
    events: broadcast::Sender<Arc<EngineEvent>>,
    snapshot: Arc<watch::Sender<Arc<Snapshot>>>,
    publishing: Arc<Mutex<ServerPort>>,
    daemon: std::sync::Weak<Inner>,
) {
    let publish = |engine: &mut Engine, evs: Vec<EngineEvent>| {
        let mut snap = engine.snapshot();
        let port = lock(&publishing);
        (snap.port, snap.port_error) = (port.port, port.error.clone());
        for e in evs {
            // No subscriber is not an error.
            let _ = events.send(Arc::new(e));
        }
        snapshot.send_replace(Arc::new(snap));
    };
    let mut rig = Rig::default();
    let mut last_tick = Instant::now();
    loop {
        let wait = if engine.has_work() { Duration::ZERO } else { TICK.saturating_sub(last_tick.elapsed()) };
        match jobs.recv_timeout(wait) {
            Ok(Job::Command(cmd, reply)) => {
                let r = engine.handle(cmd);
                let evs = engine.take_events();
                publish(&mut engine, evs);
                let _ = reply.send(r);
            }
            Ok(Job::Run(task)) => {
                let reply = task(&mut engine, &mut rig);
                engine.finish();
                let evs = engine.take_events();
                publish(&mut engine, evs);
                reply();
            }
            Ok(Job::Exit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if engine.has_work() {
                    let evs = engine.step();
                    publish(&mut engine, evs);
                }
            }
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            let _ = engine.handle(Command::Tick);
            if let Some(d) = daemon.upgrade() {
                rig.poll(&mut engine, &Daemon(d));
            }
            engine.finish();
            persist(&mut engine);
            let evs = engine.take_events();
            publish(&mut engine, evs);
        }
    }
    persist(&mut engine);
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
