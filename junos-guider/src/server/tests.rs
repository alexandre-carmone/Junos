//! The event server over TCP, against the daemon on the gear simulator.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use super::{EventServer, ServerOptions};
use crate::daemon::files::DiskFiles;
use crate::daemon::{Daemon, DaemonOptions};
use crate::engine::calibration::ProfileAccess;
use crate::engine::{Command, EngineConfig};
use crate::profile::ProfileStore;

/// The daemon on the simulator in a fresh folder under `target/`, short
/// exposures, and its server on a free port.
pub(super) async fn start(name: &str) -> (Daemon, EventServer, PathBuf) {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-server").join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let mut store = ProfileStore::open(&d.join("config"), 1).unwrap();
    store.initialize_profile();
    {
        let mut p = store.profile();
        // The simulator's stars measure about 1.6 px (see the engine tests).
        p.set_f64("/guider/StarMinHFD", 1.0);
        p.set_i32("/ExposureDurationMs", 100);
    }
    let s = |p: PathBuf| p.to_str().unwrap().to_string();
    let opts = DaemonOptions {
        engine: EngineConfig {
            instance: 1,
            log_dir: s(d.join("logs")),
            darks_dir: s(d.join("darks")),
            data_dir: s(d.clone()),
        },
        sim: true,
        indi_host: "localhost".into(),
        indi_port: 7624,
    };
    let daemon = Daemon::start(opts, store, Box::new(DiskFiles));
    let server = EventServer::start(daemon.clone(), ServerOptions { port: 0, instance: 1 });
    (daemon, server, d)
}

/// A client reading the server's lines.
pub(super) struct Client {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
}

impl Client {
    pub(super) async fn connect(server: &EventServer) -> Client {
        let addr = server.local_addr().await;
        let stream = TcpStream::connect(("127.0.0.1", addr.port())).await.unwrap();
        let (rd, wr) = stream.into_split();
        Client { rd: BufReader::new(rd), wr }
    }

    pub(super) async fn send(&mut self, text: &str) {
        self.wr.write_all(format!("{text}\r\n").as_bytes()).await.unwrap();
    }

    /// The next line, as sent: one JSON text ended by `\r\n`.
    pub(super) async fn text(&mut self) -> String {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(10), self.rd.read_line(&mut line))
            .await
            .expect("a line within 10 s")
            .unwrap();
        assert!(n > 0, "the server closed the connection");
        assert!(line.ends_with("\r\n"), "{line:?}");
        line.truncate(line.len() - 2);
        line
    }

    pub(super) async fn line(&mut self) -> Value {
        let t = self.text().await;
        serde_json::from_str(&t).unwrap_or_else(|e| panic!("{e}: {t}"))
    }

    /// The events until `name`, which is the last.
    pub(super) async fn until(&mut self, name: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let v = self.line().await;
            assert!(v.get("jsonrpc").is_none(), "unexpected reply {v}");
            let hit = v["Event"] == name;
            seen.push(v);
            if hit {
                return seen;
            }
        }
    }
}

pub(super) fn names(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["Event"].as_str().unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn catch_up_events_and_one_reply_per_request() {
    let (d, server, _dir) = start("basic").await;
    let mut c = Client::connect(&server).await;

    // The catch-up of a guider at rest: the version, the state.
    let v = c.text().await;
    assert!(v.starts_with(r#"{"Event":"Version","Timestamp":"#), "{v}");
    let v: Value = serde_json::from_str(&v).unwrap();
    assert_eq!(v["PHDVersion"], "2.6.14");
    assert_eq!(v["PHDSubver"], "");
    assert_eq!(v["OverlapSupport"], true);
    assert_eq!(v["MsgVersion"], 1);
    assert_eq!(v["Inst"], 1);
    assert!(v["Host"].is_string() && v["Timestamp"].as_f64().unwrap() > 1.7e9);
    let v = c.line().await;
    assert_eq!((v["Event"].as_str(), v["State"].as_str()), (Some("AppState"), Some("Stopped")));

    // Bad lines, a method only the relay may call, a notification: each
    // request with an id gets one reply, the notification none.
    c.send("{oops").await;
    let r = c.line().await;
    assert_eq!(r["error"]["code"], -32700);
    assert_eq!(r["id"], Value::Null);
    c.send(r#"{"method":"select_star","params":[1,2],"id":"a"}"#).await;
    assert_eq!(c.text().await, r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"method not found"},"id":"a"}"#);
    c.send(r#"{"method":"get_exposure"}"#).await;
    c.send(r#"{"method":"get_exposure","id":7}"#).await;
    let r = c.line().await;
    assert_eq!(r["id"], 7);
    assert!(r.get("result").is_some() || r.get("error").is_some());
    c.send(r#"[{"method":"get_exposure","id":8},{"method":"nope","id":9},{"method":"get_exposure"}]"#).await;
    let r = c.line().await;
    let ids: Vec<&Value> = r.as_array().unwrap().iter().map(|x| &x["id"]).collect();
    assert_eq!(ids, [&json!(8), &json!(9)]);

    // The engine's events, as they come.
    d.set_connected(true).await.unwrap();
    d.command(Command::Loop).await.unwrap();
    let seen = c.until("LoopingExposures").await;
    let e = seen.last().unwrap();
    assert!(e["Frame"].as_u64().unwrap() >= 1);
    // Under a heavy load the simulator's first frames can hold no star
    // AutoFind takes: try again on the next frame, as a user would.
    let mut tries = 0;
    while let Err(e) = d.command(Command::FindStar { roi: None }).await {
        tries += 1;
        assert!(tries < 20, "{e}");
        c.until("LoopingExposures").await;
    }
    let seen = c.until("StarSelected").await;
    assert!(names(&seen).contains(&"LockPositionSet"), "{seen:?}");

    // A second client's catch-up: the lock and the star, then the state.
    let mut c2 = Client::connect(&server).await;
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(c2.line().await);
    }
    assert_eq!(names(&got), ["Version", "LockPositionSet", "StarSelected", "AppState"]);
    assert_eq!(got[3]["State"], "Selected");
    let lock = d.snapshot().lock_position.unwrap();
    assert!((got[1]["X"].as_f64().unwrap() - lock.x).abs() < 1e-3);

    d.command(Command::StopCapture).await.unwrap();
    c.until("LoopingExposuresStopped").await;
    c2.until("LoopingExposuresStopped").await;
    d.shutdown().await;
    d.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_line_too_long_is_refused() {
    let (d, server, _dir) = start("too-big").await;
    let mut c = Client::connect(&server).await;
    c.until("AppState").await;
    c.wr.write_all(&[b' '; 3000]).await.unwrap();
    assert_eq!(c.text().await, r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"too big"},"id":null}"#);
    d.close();
}
