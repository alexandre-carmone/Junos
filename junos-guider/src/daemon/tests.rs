//! The daemon on the gear simulator, in real time: short exposures and a
//! short calibration (RA only) keep each run to a few seconds.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::broadcast::{self, error::RecvError};

use super::files::DiskFiles;
use super::{Daemon, DaemonOptions};
use crate::engine::calibration::ProfileAccess;
use crate::engine::control::SettleParams;
use crate::engine::{CmdError, Command, EngineConfig, EngineEvent};
use crate::profile::{ProfileStore, Section};

/// A fresh folder under `target/`.
fn dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-daemon").join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The daemon on the simulator, `profile` set before it starts.
fn start(d: &Path, profile: impl FnOnce(&mut Section)) -> Daemon {
    let mut store = ProfileStore::open(&d.join("config"), 1).unwrap();
    store.initialize_profile();
    {
        let mut p = store.profile();
        // The simulator's stars measure about 1.6 px (see the engine tests).
        p.set_f64("/guider/StarMinHFD", 1.0);
        p.set_i32("/ExposureDurationMs", 100);
        // RA only, 200 ms steps over 10 px: a calibration of a few frames.
        p.set_i32("/scope/DecGuideMode", 0);
        p.set_i32("/scope/CalibrationDuration", 200);
        p.set_i32("/scope/CalibrationDistance", 10);
        profile(&mut p);
    }
    let s = |p: PathBuf| p.to_str().unwrap().to_string();
    let opts = DaemonOptions {
        engine: EngineConfig {
            instance: 1,
            log_dir: s(d.join("logs")),
            darks_dir: s(d.join("darks")),
            data_dir: s(d.to_path_buf()),
        },
        sim: true,
        indi_host: "localhost".into(),
        indi_port: 7624,
    };
    Daemon::start(opts, store, Box::new(DiskFiles))
}

/// The events until one matches, within `secs`.
async fn wait_for(
    rx: &mut broadcast::Receiver<Arc<EngineEvent>>,
    secs: u64,
    f: impl Fn(&EngineEvent) -> bool,
) -> Vec<Arc<EngineEvent>> {
    let mut seen = Vec::new();
    let found = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            match rx.recv().await {
                Ok(e) => {
                    let hit = f(&e);
                    seen.push(e);
                    if hit {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => panic!("the engine thread ended"),
            }
        }
    })
    .await;
    assert!(found.is_ok(), "no matching event in {secs} s; saw {:?}", seen.iter().map(|e| name(e)).collect::<Vec<_>>());
    seen
}

fn name(e: &EngineEvent) -> String {
    format!("{e:?}").split([' ', '(', '{']).next().unwrap_or_default().to_string()
}

fn settle(px: f64, time: i32, timeout: i32) -> SettleParams {
    SettleParams { tolerance_px: px, settle_time_s: time, timeout_s: timeout, frames: 99999 }
}

fn app_state(d: &Daemon) -> &'static str {
    d.snapshot().app_state.unwrap().as_str()
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_loops_guides_and_stops_on_the_simulator() {
    let path = dir("guide");
    let d = start(&path, |_| {});
    let mut ev = d.subscribe();
    let snap = d.snapshot();
    assert!(!snap.connected && snap.camera.is_none());
    assert_eq!(app_state(&d), "Stopped");

    // While the gear connects, loop and guide wait for it.
    d.command(Command::Connecting(true)).await.unwrap();
    assert!(d.snapshot().connecting);
    let refused = Err(CmdError::new(1, "all equipment must be connected first"));
    assert_eq!(d.command(Command::Loop).await, refused);
    let guide = || Command::Guide { settle: settle(1.5, 1, 30), recalibrate: false, roi: None };
    assert_eq!(d.command(guide()).await, refused);
    d.command(Command::Connecting(false)).await.unwrap();

    assert_eq!(d.set_connected(true).await, Ok(json!(0)));
    let snap = d.snapshot();
    assert!(snap.connected && !snap.connecting);
    assert_eq!(snap.camera.as_ref().map(|c| c.name.as_str()), Some("Simulator"));
    assert_eq!(snap.mount.as_ref().map(|m| m.name.as_str()), Some("On Camera"));
    // Everything is connected already.
    assert_eq!(d.set_connected(true).await, Ok(json!(0)));

    d.command(Command::Loop).await.unwrap();
    wait_for(&mut ev, 5, |e| matches!(e, EngineEvent::LoopingExposures { .. })).await;
    assert_eq!(app_state(&d), "Looping");
    let lock = d.command(Command::FindStar { roi: None }).await.unwrap();
    assert_eq!(app_state(&d), "Selected");
    assert!(lock.as_array().is_some_and(|a| a.len() == 2), "{lock}");

    let t = Instant::now();
    d.command(guide()).await.unwrap();
    let seen = wait_for(&mut ev, 60, |e| matches!(e, EngineEvent::SettleDone { .. })).await;
    let names: Vec<String> = seen.iter().map(|e| name(e)).collect();
    let at = |n: &str| names.iter().position(|x| x == n).unwrap_or_else(|| panic!("no {n} in {names:?}"));
    assert!(at("StartCalibration") < at("CalibrationComplete"));
    assert!(at("CalibrationComplete") < at("StartGuiding") && at("StartGuiding") < at("SettleDone"));
    let Some(EngineEvent::SettleDone { error, .. }) = seen.last().map(|e| &**e) else { unreachable!() };
    assert_eq!(error.as_deref(), None);
    // Real time: the exposures, pulses and the 1 s settle took their time.
    assert!(t.elapsed() >= Duration::from_secs(1));
    assert_eq!(app_state(&d), "Guiding");
    wait_for(&mut ev, 5, |e| matches!(e, EngineEvent::GuideStep { .. })).await;
    assert!(d.snapshot().calibrated);

    // A setting changed through a command is written at close.
    d.command(Command::SetExposure(200)).await.unwrap();

    d.command(Command::StopCapture).await.unwrap();
    let seen = wait_for(&mut ev, 5, |e| matches!(e, EngineEvent::LoopingExposuresStopped)).await;
    assert!(seen.iter().any(|e| matches!(**e, EngineEvent::GuidingStopped)));
    assert!(!d.snapshot().capture_active);
    assert_eq!(app_state(&d), "Stopped");

    assert_eq!(d.set_connected(false).await, Ok(json!(0)));
    let snap = d.snapshot();
    assert!(!snap.connected && !snap.camera.as_ref().unwrap().connected);
    // Nothing left to disconnect.
    assert_eq!(d.set_connected(false).await, Ok(json!(0)));
    d.close();

    let mut store = ProfileStore::open(&path.join("config"), 1).unwrap();
    store.initialize_profile();
    assert_eq!(store.profile().get_i32("/ExposureDurationMs", 0), 200);
    let logs: Vec<String> =
        fs::read_dir(path.join("logs")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert!(logs.iter().any(|n| n.starts_with("PHD2_GuideLog_")), "{logs:?}");
    assert!(logs.iter().any(|n| n.starts_with("PHD2_DebugLog_")), "{logs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_exposure_does_not_hold_up_a_stop_or_a_refusal() {
    let path = dir("stop");
    let d = start(&path, |p| p.set_i32("/ExposureDurationMs", 5000));
    let mut ev = d.subscribe();
    d.set_connected(true).await.unwrap();
    d.command(Command::Loop).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // PHD2 refuses to disconnect while capturing, and says so at once.
    let t = Instant::now();
    let refused = d.set_connected(false).await;
    assert_eq!(refused, Err(CmdError::new(1, "cannot disconnect equipment while capture active")));
    assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());

    // A stop cuts the 5 s exposure short; the loop ends as PHD2's does when
    // a stop interrupts the exposure.
    let t = Instant::now();
    d.command(Command::StopCapture).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    let seen = wait_for(&mut ev, 2, |e| matches!(e, EngineEvent::LoopingExposuresStopped)).await;
    assert!(!seen.iter().any(|e| matches!(**e, EngineEvent::Alert { .. })), "an interrupt is no camera error");
    assert!(!d.snapshot().capture_active);

    let t = Instant::now();
    assert_eq!(d.set_connected(false).await, Ok(json!(0)));
    assert!(t.elapsed() < Duration::from_millis(500));
    assert!(!d.snapshot().connected);
    d.shutdown().await;
    d.close();

    let debug = fs::read_dir(path.join("logs"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("PHD2_DebugLog_"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .unwrap();
    // The stop ran while the exposure was out, then the interrupted
    // exposure ended the loop as a stop (not as a camera error).
    let stop = debug.find("StopCapturing CaptureActive=1 continueCapturing=1 exposurePending=1").expect("stop");
    let error = debug.find("OnExposeComplete: Capture Error reported").expect("error path");
    assert!(stop < error);
    assert!(debug.contains("MyFrame::OnClose proceeding"));
}
