//! Ekos' PHD2 client against the event server, line for line: the
//! requests `kstars/ekos/guide/externalguide/phd2.cpp` sends (Qt's compact
//! JSON, keys sorted, one call in flight, `\r\n`), its connect handshake,
//! its 2 s `get_app_state` and 3 s `get_connected` polls, guiding with a
//! settle, a dither, a suspend and resume, an abort, a calibration reset
//! after a slew and the star image it asks for on each `GuideStep`.
//!
//! The daemon runs on the gear simulator in real time: 100 ms exposures and
//! an RA-only calibration of 200 ms steps over 10 px keep it to seconds.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use junos_guider::daemon::files::DiskFiles;
use junos_guider::daemon::{Daemon, DaemonOptions};
use junos_guider::engine::EngineConfig;
use junos_guider::engine::calibration::ProfileAccess;
use junos_guider::profile::ProfileStore;
use junos_guider::server::{EventServer, ServerOptions};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

fn start(name: &str) -> (Daemon, EventServer) {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-ekos").join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let mut store = ProfileStore::open(&d.join("config"), 1).unwrap();
    store.initialize_profile();
    {
        let mut p = store.profile();
        // The simulator's stars measure about 1.6 px (see the engine tests).
        p.set_f64("/guider/StarMinHFD", 1.0);
        p.set_i32("/ExposureDurationMs", 100);
        p.set_i32("/scope/DecGuideMode", 0);
        p.set_i32("/scope/CalibrationDuration", 200);
        p.set_i32("/scope/CalibrationDistance", 10);
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
    (daemon, server)
}

/// Ekos' side of the socket.
struct Ekos {
    lines: Lines<BufReader<OwnedReadHalf>>,
    wr: OwnedWriteHalf,
    next_id: i64,
    /// Every event read, in order, and how far `wait` has looked.
    events: Vec<Value>,
    seen: usize,
    /// Replies per id.
    replies: HashMap<i64, usize>,
    /// The states `get_app_state` answered.
    states: Vec<String>,
    last_app_poll: Instant,
    last_conn_poll: Instant,
}

impl Ekos {
    async fn connect(server: &EventServer) -> Ekos {
        let port = server.local_addr().await.port();
        let (rd, wr) = TcpStream::connect(("127.0.0.1", port)).await.unwrap().into_split();
        Ekos {
            lines: BufReader::new(rd).lines(),
            wr,
            next_id: 1,
            events: Vec::new(),
            seen: 0,
            replies: HashMap::new(),
            states: Vec::new(),
            last_app_poll: Instant::now(),
            last_conn_poll: Instant::now(),
        }
    }

    /// The next line, `None` after `ms` without one.
    async fn read(&mut self, ms: u64) -> Option<Value> {
        let line = tokio::time::timeout(Duration::from_millis(ms), self.lines.next_line()).await.ok()?;
        let line = line.unwrap().expect("the server closed the connection");
        let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        if let Some(id) = v.get("id") {
            *self.replies.entry(id.as_i64().unwrap_or(-1)).or_default() += 1;
        }
        Some(v)
    }

    /// `sendPHD2Request`: `{"id","jsonrpc","method"[,"params"]}`, then the
    /// reply, reading the events that come first.
    async fn call(&mut self, method: &str, params: Option<Value>) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut req = json!({ "id": id, "jsonrpc": "2.0", "method": method });
        if let Some(p) = params {
            req["params"] = p;
        }
        self.wr.write_all(format!("{req}\r\n").as_bytes()).await.unwrap();
        let t = Instant::now();
        loop {
            assert!(t.elapsed() < Duration::from_secs(10), "no reply to {method}");
            let Some(v) = self.read(500).await else { continue };
            if v.get("Event").is_some() {
                self.events.push(v);
                continue;
            }
            assert_eq!(v["jsonrpc"], "2.0", "{v}");
            // Ekos drops a reply whose id is not the call in flight.
            assert_eq!(v["id"], id, "{method}: {v}");
            return v;
        }
    }

    async fn result(&mut self, method: &str, params: Option<Value>) -> Value {
        let r = self.call(method, params).await;
        assert!(r.get("error").is_none(), "{method}: {r}");
        r["result"].clone()
    }

    async fn error(&mut self, method: &str, params: Option<Value>) -> (i64, String) {
        let r = self.call(method, params).await;
        let e = &r["error"];
        assert!(r.get("result").is_none(), "{method}: {r}");
        (e["code"].as_i64().unwrap(), e["message"].as_str().unwrap().to_string())
    }

    async fn app_state(&mut self) -> String {
        let s = self.result("get_app_state", None).await.as_str().unwrap().to_string();
        self.states.push(s.clone());
        s
    }

    /// The events until `name` (the index of the last read), polling as
    /// Ekos does meanwhile.
    async fn wait(&mut self, name: &str, secs: u64) -> usize {
        let t = Instant::now();
        loop {
            if let Some(i) = self.events[self.seen..].iter().position(|e| e["Event"] == name) {
                self.seen += i + 1;
                return self.seen - 1;
            }
            assert!(t.elapsed() < Duration::from_secs(secs), "no {name} in {secs} s: {:?}", names(&self.events));
            if self.last_app_poll.elapsed() >= Duration::from_secs(2) {
                self.last_app_poll = Instant::now();
                self.app_state().await;
            } else if self.last_conn_poll.elapsed() >= Duration::from_secs(3) {
                self.last_conn_poll = Instant::now();
                assert_eq!(self.result("get_connected", None).await, true);
            } else if let Some(v) = self.read(200).await {
                assert!(v.get("Event").is_some(), "a reply nobody asked for: {v}");
                self.events.push(v);
            }
        }
    }

    /// Where `name` is among the events read, from `from` on.
    fn index(&self, name: &str, from: usize) -> usize {
        let i = self.events[from..].iter().position(|e| e["Event"] == name);
        from + i.unwrap_or_else(|| panic!("no {name} after {from}: {:?}", names(&self.events)))
    }
}

fn names(events: &[Value]) -> Vec<&str> {
    events.iter().map(|e| e["Event"].as_str().unwrap_or("?")).collect()
}

fn settle() -> Value {
    // Ekos' Options: ditherThreshold (px), ditherSettle (s), ditherTimeout.
    json!({ "pixels": 1.5, "time": 0, "timeout": 45 })
}

#[tokio::test(flavor = "multi_thread")]
async fn ekos_drives_the_guider() {
    let (daemon, server) = start("sequence");
    let mut ekos = Ekos::connect(&server).await;

    // Connect: PHD2 says who it is and where it stands; Ekos connects the
    // equipment, checks it, then reads the guider's parameters.
    let v = ekos.read(5000).await.unwrap();
    assert_eq!((v["Event"].as_str(), v["PHDVersion"].as_str()), (Some("Version"), Some("2.6.14")));
    let v = ekos.read(5000).await.unwrap();
    assert_eq!((v["Event"].as_str(), v["State"].as_str()), (Some("AppState"), Some("Stopped")));
    assert_eq!(ekos.result("set_connected", Some(json!({ "connected": true }))).await, 0);
    assert_eq!(ekos.result("get_connected", None).await, true);
    let scale = ekos.result("get_pixel_scale", None).await;
    assert!(scale.is_null() || scale.as_f64().unwrap() > 0.0, "{scale}");
    assert_eq!(ekos.result("get_exposure", None).await, 100);
    assert_eq!(ekos.result("get_dec_guide_mode", None).await, "Off");
    let durations = ekos.result("get_exposure_durations", None).await;
    assert!(durations.as_array().is_some_and(|d| d.len() > 2 && d.iter().all(Value::is_i64)), "{durations}");
    let gear = ekos.result("get_current_equipment", None).await;
    assert_eq!(gear["camera"], json!({ "name": "Simulator", "connected": true }));
    assert_eq!(gear["mount"], json!({ "name": "On Camera", "connected": true }));
    assert_eq!(ekos.app_state().await, "Stopped");

    // Guide: PHD2 calibrates, guides and settles; Ekos reports Guiding
    // from SettleDone only.
    let from = ekos.events.len();
    let guide = json!({ "recalibrate": false, "settle": settle() });
    assert_eq!(ekos.result("guide", Some(guide)).await, 0);
    let done = ekos.wait("SettleDone", 90).await;
    assert_eq!(ekos.events[done]["Status"], 0, "{}", ekos.events[done]);
    let at = |name: &str| ekos.index(name, from);
    assert!(at("StartCalibration") < at("Calibrating"));
    assert!(at("Calibrating") < at("CalibrationComplete"));
    assert!(at("CalibrationComplete") < at("StartGuiding"));
    assert!(at("StartGuiding") < at("SettleBegin") && at("SettleBegin") < done);
    assert_eq!(ekos.events[at("CalibrationComplete")]["Mount"], "On Camera");
    assert_eq!(ekos.app_state().await, "Guiding");

    // On each GuideStep Ekos asks for the star.
    let step = ekos.wait("GuideStep", 10).await;
    let s = ekos.events[step].clone();
    assert_eq!(s["Mount"], "On Camera");
    assert!(s["Frame"].as_i64().unwrap() > 0 && s.get("dx").is_some() && s.get("SNR").is_some(), "{s}");
    // A 31 × 31 box around the star, cut by the guiding subframe.
    let img = ekos.result("get_star_image", Some(json!({ "size": 32 }))).await;
    let (w, h) = (img["width"].as_u64().unwrap() as usize, img["height"].as_u64().unwrap() as usize);
    assert!((16..=31).contains(&w) && (16..=31).contains(&h), "{w} × {h}");
    assert!(img["frame"].as_u64().unwrap() >= 1);
    assert_eq!(img["pixels"].as_str().unwrap().len(), (w * h * 2).div_ceil(3) * 4);
    let pos: Vec<f64> = img["star_pos"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert!((0.0..w as f64).contains(&pos[0]) && (0.0..h as f64).contains(&pos[1]), "{pos:?}");

    // Dither, as Capture asks between frames: Ekos waits for SettleDone.
    let from = ekos.events.len();
    let dither = json!({ "amount": 2, "raOnly": false, "settle": settle() });
    assert_eq!(ekos.result("dither", Some(dither)).await, 0);
    let done = ekos.wait("SettleDone", 60).await;
    assert_eq!(ekos.events[done]["Status"], 0, "{}", ekos.events[done]);
    assert!(ekos.index("GuidingDithered", from) < done);
    assert_eq!(ekos.app_state().await, "Guiding");

    // Suspend and resume (Focus, Capture).
    let paused = json!({ "paused": true, "type": "full" });
    assert_eq!(ekos.result("set_paused", Some(paused.clone())).await, 0);
    ekos.wait("Paused", 5).await;
    assert_eq!(ekos.app_state().await, "Paused");
    assert_eq!(ekos.result("get_paused", None).await, true);
    assert_eq!(ekos.result("set_paused", Some(json!({ "paused": false }))).await, 0);
    ekos.wait("Resumed", 5).await;
    ekos.wait("GuideStep", 10).await;
    assert_eq!(ekos.app_state().await, "Guiding");

    // Abort while suspended: Ekos unpauses first so the stop works; no
    // exposure starts after it.
    assert_eq!(ekos.result("set_paused", Some(paused)).await, 0);
    ekos.wait("Paused", 5).await;
    let from = ekos.events.len();
    assert_eq!(ekos.result("set_paused", Some(json!({ "paused": false }))).await, 0);
    assert_eq!(ekos.result("stop_capture", None).await, 0);
    let stopped = ekos.wait("LoopingExposuresStopped", 10).await;
    assert!(ekos.index("Resumed", from) < ekos.index("GuidingStopped", from));
    assert!(ekos.index("GuidingStopped", from) < stopped);
    assert_eq!(ekos.app_state().await, "Stopped");
    tokio::time::sleep(Duration::from_millis(600)).await;
    while let Some(v) = ekos.read(100).await {
        ekos.events.push(v);
    }
    let after: Vec<&str> = names(&ekos.events[stopped + 1..]);
    assert!(!after.iter().any(|n| matches!(*n, "LoopingExposures" | "GuideStep" | "StarLost")), "{after:?}");
    assert!(!daemon.snapshot().capture_active);

    // A slew: Ekos resets the calibration.
    assert_eq!(ekos.result("get_calibrated", None).await, true);
    assert_eq!(ekos.result("clear_calibration", Some(json!({ "which": "mount" }))).await, 0);
    assert_eq!(ekos.result("get_calibrated", None).await, false);

    // Refusals: one reply each, with PHD2's codes.
    assert_eq!(ekos.error("get_star_image", Some(json!({ "size": 10 }))).await, (-32602, "invalid image size param".into()));
    let e = ekos.error("set_connected", Some(json!({ "connected": 1 }))).await;
    assert_eq!(e, (-32602, "expected connected boolean param".into()));
    assert_eq!(ekos.error("guide", Some(json!({ "recalibrate": false }))).await, (-32602, "expected settle object param".into()));
    let e = ekos.error("set_lock_position", Some(json!({ "exact": true, "x": 1e6, "y": 1e6 }))).await;
    assert_eq!(e, (-32600, "could not set lock position".into()));
    assert_eq!(ekos.error("select_star", Some(json!({ "x": 1, "y": 2 }))).await, (-32601, "method not found".into()));
    assert_eq!(ekos.error("set_dec_guide_mode", Some(json!({ "mode": "Both" }))).await.0, 1);
    assert_eq!(ekos.result("set_dec_guide_mode", Some(json!({ "mode": "auto" }))).await, 0);
    assert_eq!(ekos.result("get_dec_guide_mode", None).await, "Auto");
    assert_eq!(ekos.result("set_exposure", Some(json!({ "exposure": 200 }))).await, 0);
    assert_eq!(ekos.result("get_exposure", None).await, 200);

    // Disconnect: Ekos stops, then disconnects the equipment.
    assert_eq!(ekos.result("set_connected", Some(json!({ "connected": false }))).await, 0);
    assert_eq!(ekos.result("get_connected", None).await, false);

    // Every call got exactly one reply.
    for id in 1..ekos.next_id {
        assert_eq!(ekos.replies.get(&id), Some(&1), "id {id}");
    }
    assert_eq!(ekos.replies.len() as i64, ekos.next_id - 1, "{:?}", ekos.replies);
    let allowed = ["Stopped", "Looping", "Selected", "Calibrating", "Guiding", "LostLock", "Paused"];
    assert!(ekos.states.iter().all(|s| allowed.contains(&s.as_str())), "{:?}", ekos.states);

    daemon.shutdown().await;
    daemon.close();
}
