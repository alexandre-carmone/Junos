//! The daemon on the gear simulator, in real time: short exposures and a
//! short calibration (RA only) keep each run to a few seconds.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::sync::broadcast::{self, error::RecvError};

use super::files::DiskFiles;
use super::gear::{Choices, Rig};
use super::{Daemon, DaemonOptions};
use crate::engine::calibration::ProfileAccess;
use crate::engine::control::SettleParams;
use crate::engine::traits::CancelToken;
use crate::engine::{CmdError, Command, EngineConfig, EngineEvent};
use crate::profile::{ProfileStore, Section};

/// A fresh folder under `target/`.
pub(crate) fn dir(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-daemon").join(name);
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The daemon on the simulator, `profile` set before it starts.
pub(crate) fn start(d: &Path, profile: impl FnOnce(&mut Section)) -> Daemon {
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
pub(crate) async fn wait_for(
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

/// `find_star`, tried again on the next frames: under a heavy load (the
/// whole suite at once) the simulator's first frame can hold no star
/// AutoFind takes.
pub(crate) async fn find_star(d: &Daemon, ev: &mut broadcast::Receiver<Arc<EngineEvent>>) -> serde_json::Value {
    for _ in 0..20 {
        if let Ok(lock) = d.command(Command::FindStar { roi: None }).await {
            return lock;
        }
        wait_for(ev, 5, |e| matches!(e, EngineEvent::LoopingExposures { .. })).await;
    }
    panic!("no star found in 20 frames");
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
    let seen = wait_for(&mut ev, 5, |e| matches!(e, EngineEvent::LoopingExposures { .. })).await;
    assert_eq!(app_state(&d), "Looping");
    // The frames go to their own `watch`, never to the broadcast.
    assert!(d.frames().borrow().is_some());
    assert!(!seen.iter().any(|e| matches!(**e, EngineEvent::FrameReady(_))));
    let lock = find_star(&d, &mut ev).await;
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
    assert!(logs.iter().any(|n| n.starts_with("PHD2_DebugLog_")), "{logs:?}");
    // `OnClose` closed the guide log: its summary and closing line.
    let guide = logs.iter().find(|n| n.starts_with("PHD2_GuideLog_")).expect("a guide log");
    let guide = fs::read_to_string(path.join("logs").join(guide)).unwrap();
    assert!(guide.contains("Log Summary: calcnt:"), "{guide}");
    assert!(guide.trim_end().lines().last().is_some_and(|l| l.starts_with("Log closed at ")), "{guide}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_panic_is_published_not_hidden() {
    let path = dir("panic");
    let d = start(&path, |_| {});
    assert_eq!(d.with_engine(|_| -> () { panic!("boom") }).await, None);
    let msg = tokio::time::timeout(Duration::from_secs(5), d.failed()).await.unwrap();
    assert_eq!(msg, "boom");
    assert_eq!(d.snapshot().engine_error.as_deref(), Some("boom"));
    let internal = Err(CmdError::new(crate::server::rpc::INTERNAL_ERROR, "internal error"));
    assert_eq!(d.command(Command::Loop).await, internal);
    d.close();
    // The guide log was closed all the same.
    let guide = fs::read_dir(path.join("logs"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("PHD2_GuideLog_"))
        .map(|e| fs::read_to_string(e.path()).unwrap())
        .unwrap();
    assert!(guide.trim_end().lines().last().is_some_and(|l| l.starts_with("Log closed at ")), "{guide}");
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

/// A scripted INDI telescope with timed guiding, as the mount tests'.
fn fake_mount(dev: &'static str) -> crate::indi::fake::Fake {
    use crate::indi::fake;
    fake::Fake::start(move |e, peer| match (e.name.as_str(), e.attr("name").unwrap_or_default()) {
        ("getProperties", _) => peer.send(&fake::driver(dev, 5, false)),
        ("newSwitchVector", "CONNECTION") => {
            peer.send(&fake::connection_reply(e).unwrap());
            let coord = [("RA", 5.5, 0.0, 24.0), ("DEC", 20.0, -90.0, 90.0)];
            let mut s = fake::def_number(dev, "EQUATORIAL_EOD_COORD", "Ok", &coord);
            for (p, a, b) in [
                ("TELESCOPE_TIMED_GUIDE_NS", "TIMED_GUIDE_N", "TIMED_GUIDE_S"),
                ("TELESCOPE_TIMED_GUIDE_WE", "TIMED_GUIDE_W", "TIMED_GUIDE_E"),
            ] {
                s += &fake::def_number(dev, p, "Idle", &[(a, 0.0, 0.0, 6e4), (b, 0.0, 0.0, 6e4)]);
            }
            peer.send(&s);
        }
        _ => {}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn an_indi_mount_connects_drops_and_comes_back() {
    const DEV: &str = "Telescope Simulator";
    let fake = fake_mount(DEV);
    let path = dir("indi");
    let mut store = ProfileStore::open(&path.join("config"), 1).unwrap();
    store.initialize_profile();
    {
        let mut p = store.profile();
        p.set_str("/scope/LastMenuChoice", "INDI Mount [Telescope Simulator]");
        p.set_str("/indi/INDImount", DEV);
        p.set_str("/indi/INDIhost", "127.0.0.1");
        p.set_i32("/indi/INDIport", fake.port as i32);
    }
    let s = |p: PathBuf| p.to_str().unwrap().to_string();
    let opts = DaemonOptions {
        engine: EngineConfig {
            instance: 1,
            log_dir: s(path.join("logs")),
            darks_dir: s(path.join("darks")),
            data_dir: s(path.clone()),
        },
        sim: false,
        indi_host: "localhost".into(),
        indi_port: 1,
    };
    let d = Daemon::start(opts, store, Box::new(DiskFiles));
    let mut ev = d.subscribe();

    // No camera chosen: PHD2's error, the mount connected all the same.
    let r = d.set_connected(true).await;
    assert_eq!(r, Err(CmdError::new(1, "equipment failed to connect: camera")));
    let mount = || d.snapshot().mount.clone().unwrap();
    assert_eq!(mount().name, "INDI Mount [Telescope Simulator]");
    assert!(mount().connected);
    assert!(!d.snapshot().connected);

    // Two connects at once: the second waits for the first, then answers
    // from its own try, never with the gear dialog's error.
    let failed = Err(CmdError::new(1, "equipment failed to connect: camera"));
    let (a, b) = tokio::join!(d.set_connected(true), d.set_connected(true));
    assert_eq!((a, b), (failed.clone(), failed));
    assert!(mount().connected);

    // A connect the engine refuses lets go of its own devices only: the
    // live rig keeps its INDI connection and its connected mount.
    let opts = d.options().clone();
    let kept = d
        .run(move |e, rig| {
            let choices = Choices::read(e, &opts);
            let (new, _gear, _) = Rig::connect(&choices, rig.client(), &CancelToken::new());
            new.discard(rig, e);
            rig.client().is_some_and(|c| !c.is_stopped()) && rig.any_connected()
        })
        .await;
    assert_eq!(kept, Some(true));

    // The server goes: PHD2's alert, the mount disconnected; then the
    // client redials and the daemon connects the mount again.
    fake.drop_conns();
    let seen = wait_for(&mut ev, 30, |e| matches!(e, EngineEvent::Alert { .. })).await;
    let Some(EngineEvent::Alert { msg, .. }) = seen.last().map(|e| &**e) else { unreachable!() };
    assert_eq!(msg, "INDI server disconnected");
    // The snapshot went out before the alert. The mount shows connected
    // from the reconnect's attach on, before that connect lets go of the
    // gear dialog: wait for all of it, generously (the whole suite at once
    // slows the redial).
    assert!(!mount().connected);
    let t = Instant::now();
    while !mount().connected || d.0.connecting.load(std::sync::atomic::Ordering::SeqCst) || fake.accepted() < 2 {
        assert!(t.elapsed() < Duration::from_secs(30), "the mount did not come back");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(d.set_connected(false).await, Ok(json!(0)));
    assert!(!mount().connected);
    // The INDI connection went with the gear, so it brings nothing back.
    assert_eq!(d.run(|_, rig| rig.client().is_none()).await, Some(true));
    assert!(d.indi_client().is_none());
    d.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_profile_makes_the_gear_anew() {
    let path = dir("profile");
    let d = start(&path, |_| {});
    let id = d
        .with_engine(|e| {
            let cur = e.store().current_profile();
            // True is an error.
            assert!(!e.store().clone_profile("Other", &cur));
            e.store().profile_id("Other")
        })
        .await
        .unwrap();
    d.set_connected(true).await.unwrap();
    let refused = Err(CmdError::new(1, "cannot set profile when equipment is connected"));
    assert_eq!(d.set_profile(id).await, refused);

    assert_eq!(d.set_connected(false).await, Ok(json!(0)));
    assert_eq!(d.set_profile(id).await, Ok(json!(0)));
    // `LoadGearChoices`: the engine's gear and the rig are gone, so the
    // next connect builds the new profile's devices and mounts.
    let snap = d.snapshot();
    assert!(snap.camera.is_none() && snap.mount.is_none() && snap.ao.is_none());
    assert_eq!(d.run(|_, rig| rig.client().is_none() && !rig.any_connected()).await, Some(true));
    assert_eq!(d.set_connected(true).await, Ok(json!(0)));
    assert_eq!(d.snapshot().profile_id, id);
    d.shutdown().await;
    d.close();
}
