//! PHD2's event server: the JSON-RPC 2.0 socket (port 4400 + instance − 1)
//! that Ekos, NINA and every other PHD2 client drive the guider through.
//!
//! [`rpc`] holds the protocol itself, free of sockets: the line framing, the
//! request parsing, the method table, the parameter getters and the replies.
//! [`events`] writes the events and the catch-up; `methods` answers the 54
//! methods, from the engine or its snapshot.
//!
//! The server listens on every interface with `SO_REUSEADDR`, as PHD2's
//! `wxSocketServer` does. Each client gets a task that reads its requests
//! and a writer that sends, one JSON text per line ended by `\r\n`:
//!
//! 1. on connect, the catch-up (`Version`, the lock and the star, the
//!    calibrations, what is running, `AppState`), taken from one snapshot
//!    together with the event subscription, so no event falls between them;
//! 2. every engine event as it comes;
//! 3. the reply to each request line, after the events its command raised:
//!    PHD2 writes those while the handler runs, before its reply, and Ekos
//!    reads `SettleDone`, `Paused`, … in that order.
//!
//! Requests of one client run one line at a time, as PHD2's single event
//! loop runs them; a command waits for the engine, a read-only method
//! answers from the snapshot. Each request line gets its reply whatever
//! happens to its handler: a [`ReplyGuard`] answers the calls left without
//! an outcome `-32603 internal error` when it is dropped, so Ekos, which
//! keeps one call in flight and waits for its reply, never stalls.
//!
//! A client that does not read is not waited for: its events stay in a
//! bounded queue (the broadcast's backlog) and the ones it misses are
//! dropped with a warning, as PHD2's short writes are.
//!
//! Not ported: the legacy binary socket server on 4300 + instance − 1, and
//! the `evsrv:` request and response lines of the debug log.
//!
//! Ported from PHD2 src/event_server.cpp — BSD-3-Clause, see THIRD_PARTY.md

pub mod events;
mod methods;
pub mod rpc;

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, watch};

use self::events::Origin;
use self::rpc::{Call, LineReader, Parsed, RpcError};
use crate::daemon::Daemon;
use crate::engine::EngineEvent;

/// Replies waiting for a client's writer.
const REPLY_QUEUE: usize = 16;
/// One socket read: what PHD2 finds readable at once, which it throws away
/// whole when a line is too long.
const READ_CHUNK: usize = 16 * 1024;
/// The backlog of the listening socket.
const BACKLOG: u32 = 128;

/// Where and as what the server runs.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// 4400 + instance − 1, or `--server-port`; 0 takes a free port.
    pub port: u16,
    /// The events' `Inst`.
    pub instance: u32,
}

struct Shared {
    daemon: Daemon,
    origin: Origin,
    /// The address once listening.
    addr: watch::Sender<Option<SocketAddr>>,
}

/// The running server. Clones share it.
#[derive(Clone)]
pub struct EventServer(Arc<Shared>);

impl EventServer {
    /// Starts listening on `opts.port` and serving the daemon's clients.
    pub fn start(daemon: Daemon, opts: ServerOptions) -> EventServer {
        let shared = Arc::new(Shared { daemon, origin: Origin::new(opts.instance), addr: watch::Sender::new(None) });
        let s = shared.clone();
        tokio::spawn(async move {
            match bind(opts.port) {
                Ok(listener) => serve(listener, s).await,
                Err(e) => {
                    tracing::error!("Event server failed to start - Could not listen at port {}: {e}", opts.port)
                }
            }
        });
        EventServer(shared)
    }

    /// The address the server listens on, once it does.
    pub async fn local_addr(&self) -> SocketAddr {
        let mut rx = self.0.addr.subscribe();
        let addr = rx.wait_for(Option::is_some).await.map(|a| *a);
        // The sender lives as long as `self`.
        addr.ok().flatten().expect("the server's address")
    }
}

/// A listening socket on every interface, `SO_REUSEADDR` as PHD2's.
fn bind(port: u16) -> io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))?;
    socket.listen(BACKLOG)
}

async fn serve(listener: TcpListener, s: Arc<Shared>) {
    let addr = listener.local_addr().ok();
    if let Some(a) = addr {
        tracing::info!("event server started, listening on port {}", a.port());
    }
    s.addr.send_replace(addr);
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(client(stream, peer, s.clone()));
            }
            Err(e) => {
                // Out of descriptors, mostly: wait a little rather than spin.
                tracing::warn!("event server: accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

/// A reply for the writer.
struct Outgoing {
    text: String,
}

/// One client (`OnEventServerEvent`, `OnEventServerClientEvent`).
async fn client(stream: TcpStream, peer: SocketAddr, s: Arc<Shared>) {
    tracing::debug!(%peer, "evsrv: cli connect");
    let _ = stream.set_nodelay(true);
    let (rd, wr) = stream.into_split();
    let (events, snap) = s.daemon.subscribe_with_snapshot();
    let catch_up = events::catch_up(&snap, &s.origin, events::now());
    let (tx, rx) = mpsc::channel(REPLY_QUEUE);
    let writer = tokio::spawn(write_loop(wr, catch_up, events, rx, s.origin.clone()));
    read_loop(rd, tx, &s).await;
    // The reader is done; the writer ends once the replies queued are out.
    let _ = writer.await;
    tracing::debug!(%peer, "evsrv: cli disconnect");
}

/// One message on the wire.
async fn send(wr: &mut OwnedWriteHalf, text: &str) -> io::Result<()> {
    let mut line = String::with_capacity(text.len() + 2);
    line.push_str(text);
    line.push_str("\r\n");
    wr.write_all(line.as_bytes()).await
}

/// The client's writer: the catch-up, then the events and the replies.
async fn write_loop(
    mut wr: OwnedWriteHalf,
    catch_up: Vec<String>,
    mut events: broadcast::Receiver<Arc<EngineEvent>>,
    mut replies: mpsc::Receiver<Outgoing>,
    origin: Origin,
) {
    for text in catch_up {
        if send(&mut wr, &text).await.is_err() {
            return;
        }
    }
    let mut events_open = true;
    loop {
        tokio::select! {
            biased;
            out = replies.recv() => {
                let Some(out) = out else { break };
                // The events the command raised went out before its reply
                // was handed over: they are waiting here already.
                while events_open {
                    match events.try_recv() {
                        Ok(e) => {
                            if write_event(&mut wr, &e, &origin).await.is_err() {
                                return;
                            }
                        }
                        Err(TryRecvError::Lagged(n)) => lagged(n),
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Closed) => events_open = false,
                    }
                }
                if send(&mut wr, &out.text).await.is_err() {
                    return;
                }
            }
            e = events.recv(), if events_open => match e {
                Ok(e) => {
                    if write_event(&mut wr, &e, &origin).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(n)) => lagged(n),
                Err(RecvError::Closed) => events_open = false,
            },
        }
    }
}

/// An event, stamped as it is written: the client's writer takes it as it
/// comes, so the stamp is the time PHD2 would have written.
async fn write_event(wr: &mut OwnedWriteHalf, e: &EngineEvent, origin: &Origin) -> io::Result<()> {
    match events::event(e, origin, events::now()) {
        Some(text) => send(wr, &text).await,
        None => Ok(()),
    }
}

fn lagged(n: u64) {
    tracing::warn!("evsrv: a client did not read; {n} events dropped");
}

/// The client's requests (`handle_cli_input`), one line at a time.
async fn read_loop(mut rd: OwnedReadHalf, tx: mpsc::Sender<Outgoing>, s: &Shared) {
    let mut lines = LineReader::default();
    let mut buf = vec![0; READ_CHUNK];
    loop {
        let n = match rd.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        for input in lines.feed(&buf[..n]) {
            let mut guard = ReplyGuard::new(rpc::parse(&input), tx.clone());
            for call in guard.calls() {
                let outcome = methods::run(s, &call).await;
                guard.outcomes.push(outcome);
            }
            guard.send().await;
        }
    }
}

/// One request line's reply, sent however its handlers end: dropped
/// before [`send`](ReplyGuard::send) (a handler that panicked, a client
/// task cancelled), it answers the calls left without an outcome
/// `-32603 internal error`.
struct ReplyGuard {
    parsed: Option<Parsed>,
    outcomes: Vec<Result<Value, RpcError>>,
    tx: mpsc::Sender<Outgoing>,
}

impl ReplyGuard {
    fn new(parsed: Parsed, tx: mpsc::Sender<Outgoing>) -> ReplyGuard {
        ReplyGuard { parsed: Some(parsed), outcomes: Vec::new(), tx }
    }

    /// The calls to run, in order.
    fn calls(&self) -> Vec<Call> {
        self.parsed.as_ref().map(|p| p.calls().cloned().collect()).unwrap_or_default()
    }

    fn text(&mut self) -> Option<String> {
        self.parsed.take()?.reply(std::mem::take(&mut self.outcomes))
    }

    /// The reply, to the writer.
    async fn send(mut self) {
        if let Some(text) = self.text() {
            let _ = self.tx.send(Outgoing { text }).await;
        }
    }
}

impl Drop for ReplyGuard {
    fn drop(&mut self) {
        if let Some(text) = self.text() {
            let _ = self.tx.try_send(Outgoing { text });
        }
    }
}

#[cfg(test)]
mod tests;
