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
    start_with(name, ServerOptions { port: 0, instance: 1, shutdown_exits: false }).await
}

/// [`start`] with the server's options.
pub(super) async fn start_with(name: &str, server: ServerOptions) -> (Daemon, EventServer, PathBuf) {
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
    let server = EventServer::start(daemon.clone(), server);
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

#[tokio::test(flavor = "multi_thread")]
async fn methods_answer_as_phd2() {
    let (d, server, dir) = start("methods").await;
    let mut c = Client::connect(&server).await;
    c.until("AppState").await;
    let mut id = 0;
    let mut call = async |c: &mut Client, method: &str, params: Value| {
        id += 1;
        let req = json!({ "method": method, "params": params, "id": id });
        c.send(&req.to_string()).await;
        loop {
            let v = c.line().await;
            if v.get("jsonrpc").is_some() {
                assert_eq!(v["id"], id, "{v}");
                return v.get("result").cloned().unwrap_or_else(|| v["error"].clone());
            }
        }
    };

    // Nothing connected yet.
    assert_eq!(call(&mut c, "get_profile", json!([])).await, json!({ "id": 1, "name": "My Equipment" }));
    assert_eq!(call(&mut c, "get_profiles", json!([])).await, json!([{ "id": 1, "name": "My Equipment", "selected": true }]));
    assert_eq!(call(&mut c, "set_profile", json!([1])).await, 0);
    assert_eq!(call(&mut c, "set_profile", json!([2])).await["message"], "invalid profile id");
    assert_eq!(call(&mut c, "set_profile", json!(["1"])).await["code"], -32602);
    assert_eq!(call(&mut c, "get_camera_binning", json!([])).await["message"], "camera not connected");
    assert_eq!(call(&mut c, "get_guide_output_enabled", json!([])).await["message"], "mount not defined");
    assert_eq!(call(&mut c, "get_algo_param_names", json!(["ra"])).await, json!(["algorithmName"]));
    assert_eq!(call(&mut c, "get_algo_param", json!(["ra", "minMove"])).await["message"], "could not get param");
    assert_eq!(call(&mut c, "get_algo_param_names", json!(["z"])).await["message"], "expected axis name param");
    assert_eq!(call(&mut c, "get_lock_position", json!([])).await, Value::Null);
    assert_eq!(call(&mut c, "get_limit_frame", json!([])).await, json!({ "roi": null }));
    assert_eq!(call(&mut c, "set_limit_frame", json!({})).await["message"], "missing required param `roi`");
    let e = call(&mut c, "capture_single_frame", json!({})).await;
    assert_eq!(e["message"], "cannot capture single frame when camera is not connected");

    d.set_connected(true).await.unwrap();
    assert_eq!(call(&mut c, "set_profile", json!({ "id": 2 })).await["message"], "cannot set profile when equipment is connected");
    assert_eq!(call(&mut c, "get_camera_binning", json!([])).await, 1);
    assert_eq!(call(&mut c, "get_guide_output_enabled", json!([])).await, true);
    let names = call(&mut c, "get_algo_param_names", json!({ "axis": "RA" })).await;
    assert_eq!(names, json!(["algorithmName", "minMove", "hysteresis", "aggression"]));
    assert_eq!(call(&mut c, "get_algo_param", json!(["x", "algorithmName"])).await, "Hysteresis");
    assert_eq!(call(&mut c, "set_algo_param", json!(["ra", "aggression", 0.5])).await, 0);
    assert_eq!(call(&mut c, "get_algo_param", json!(["ra", "aggression"])).await, 0.5);
    assert_eq!(call(&mut c, "set_algo_param", json!(["ra", "aggression"])).await["message"], "expected param value param");
    assert_eq!(call(&mut c, "get_calibration_data", json!(["mount"])).await, json!({ "calibrated": false }));
    assert_eq!(call(&mut c, "get_calibration_data", json!(["ao"])).await["message"], "device not connected");
    assert_eq!(call(&mut c, "get_calibration_data", json!(["both"])).await["message"], "invalid 'which' param");
    let shift = call(&mut c, "get_lock_shift_params", json!([])).await;
    assert_eq!(shift, json!({ "enabled": false, "rate": [0.0, 0.0], "units": "arcsec/hr", "axes": "RA/Dec" }));
    let set = json!([{ "rate": [3.3, 1.1], "units": "pixels/hr", "axes": "X/Y" }]);
    assert_eq!(call(&mut c, "set_lock_shift_params", set).await, 0);
    let shift = call(&mut c, "get_lock_shift_params", json!([])).await;
    assert_eq!(shift, json!({ "enabled": false, "rate": [3.3, 1.1], "units": "pixels/hr", "axes": "X/Y" }));
    assert_eq!(call(&mut c, "set_variable_delay_settings", json!([true, 5.9, 20])).await, 0);
    let delay = call(&mut c, "get_variable_delay_settings", json!([])).await;
    assert_eq!(delay, json!({ "Enabled": true, "ShortDelaySeconds": 5, "LongDelaySeconds": 20 }));
    let e = call(&mut c, "capture_single_frame", json!({ "exposure": 0 })).await;
    assert_eq!((e["code"].as_i64(), e["message"].as_str()), (Some(-32602), Some("expected exposure param")));
    let e = call(&mut c, "capture_single_frame", json!({ "path": "rel.fit" })).await;
    assert_eq!(e["message"], "path param must be an absolute path");
    assert_eq!(call(&mut c, "get_star_image", json!([])).await["message"], "no star selected");
    let file = call(&mut c, "export_config_settings", json!([])).await;
    let file = file["filename"].as_str().unwrap().to_string();
    assert_eq!(Path::new(&file), dir.join("phd2_settings.txt"));
    assert!(fs::read_to_string(&file).unwrap().starts_with("PHD Config 1"));

    d.shutdown().await;
    d.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_taken_port_is_tried_again() {
    let blocker = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = blocker.local_addr().unwrap().port();
    let (d, server, _dir) = start_with("retry", ServerOptions { port, instance: 1, shutdown_exits: false }).await;
    let t = std::time::Instant::now();
    while d.snapshot().port_error.is_none() {
        assert!(t.elapsed() < Duration::from_secs(5), "no port error");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let snap = d.snapshot();
    assert_eq!(snap.port, None);
    let error = snap.port_error.clone().unwrap();
    assert!(error.starts_with(&format!("Could not listen at port {port}: ")), "{error}");

    // The port freed, the server takes it at its next try.
    drop(blocker);
    let addr = tokio::time::timeout(Duration::from_secs(10), server.local_addr()).await.unwrap();
    assert_eq!(addr.port(), port);
    let snap = d.snapshot();
    assert_eq!((snap.port, snap.port_error.as_deref()), (Some(port), None));
    let mut c = Client::connect(&server).await;
    c.until("AppState").await;
    d.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_answers_then_stops() {
    let (d, server, _dir) = start("shutdown").await;
    let mut c = Client::connect(&server).await;
    c.until("AppState").await;
    d.set_connected(true).await.unwrap();
    d.command(Command::Loop).await.unwrap();
    c.until("LoopingExposures").await;

    // The reply comes before the stop; the gear is disconnected and the
    // server goes on serving.
    c.send(r#"{"method":"shutdown","id":1}"#).await;
    loop {
        let v = c.line().await;
        if v.get("jsonrpc").is_some() {
            assert_eq!((&v["result"], &v["id"]), (&json!(0), &json!(1)));
            break;
        }
        assert_ne!(v["Event"], "LoopingExposuresStopped");
    }
    c.until("LoopingExposuresStopped").await;
    c.send(r#"{"method":"get_connected","id":2}"#).await;
    loop {
        let v = c.line().await;
        if v.get("jsonrpc").is_some() {
            assert_eq!(v["result"], false);
            break;
        }
    }
    assert!(!d.snapshot().capture_active);

    // With --shutdown-exits the process is asked to end.
    let opts = ServerOptions { port: 0, instance: 1, shutdown_exits: true };
    let exiting = EventServer::start(d.clone(), opts);
    let mut c = Client::connect(&exiting).await;
    c.until("AppState").await;
    c.send(r#"{"method":"shutdown","id":3}"#).await;
    assert_eq!(c.text().await, r#"{"jsonrpc":"2.0","result":0,"id":3}"#);
    tokio::time::timeout(Duration::from_secs(5), exiting.exit_requested()).await.unwrap();
    d.close();
}
