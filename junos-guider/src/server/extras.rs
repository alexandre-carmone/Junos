//! The junos UI's own methods (`ws_only` in [`super::rpc::METHODS`]),
//! reached through the relay's `guider_rpc` only: what PHD2's windows do
//! with clicks — the profiles, the equipment choices, the algorithms, the
//! Guiding Assistant, the darks, the polar drift tool, the logs. Each maps
//! onto an engine [`Command`] or the profile store; parameters are named
//! (or positional in the table's order). The tools the engine does not
//! have yet (drift align, pier-flip,
//! bad-pixel editing, a single device's connect) answer
//! code 1 "not available".
//!
//! The relay socket's own `set_frame_filter` and `get_history` are not
//! here: the socket answers them itself.

use serde_json::{Value, json};

use super::methods::command;
use super::rpc::{Call, METHOD_NOT_FOUND, Params, RpcError, bool_param, float_param, int_param};
use crate::daemon::Daemon;
use crate::engine::algo::{AlgoKind, Axis};
use crate::engine::assistant::{self, RecKind};
use crate::engine::calibration::Calibration;
use crate::engine::control::DitherMode;
use crate::engine::log::image_log::ImageLoggerSettings;
use crate::engine::traits::{PierSide, Point};
use crate::engine::{BookmarkOp, CalAssistCmd, Command, StaticPaSet, DarksBuildKind, Engine, ManualPointing, Which};
use crate::indi::client::Role;

/// PHD2's `pConfig->Global` keys the UI edits through `set_profile_value`
/// (the rest are the profile's); `guider_settings` carries them too.
pub const GLOBAL_KEYS: [&str; 2] = ["/StickyLockPosition", "/server/guide_allow_recalibrate"];

fn e1(msg: &str) -> RpcError {
    RpcError::new(1, msg)
}

fn need<T>(v: Option<T>, name: &str) -> Result<T, RpcError> {
    v.ok_or_else(|| RpcError::invalid_params(format!("expected {name} param")))
}

fn num(p: &Params, name: &str) -> Result<f64, RpcError> {
    need(p.param(name).and_then(float_param), name)
}

fn int(p: &Params, name: &str) -> Result<i32, RpcError> {
    need(p.param(name).and_then(int_param), name)
}

fn flag(p: &Params, name: &str) -> Result<bool, RpcError> {
    need(p.param(name).and_then(bool_param), name)
}

fn text<'a>(p: &Params<'a>, name: &str) -> Result<&'a str, RpcError> {
    need(p.param(name).and_then(Value::as_str), name)
}

fn axis(p: &Params) -> Result<Axis, RpcError> {
    match text(p, "axis")?.to_ascii_lowercase().as_str() {
        "ra" | "x" => Ok(Axis::X),
        "dec" | "y" => Ok(Axis::Y),
        _ => Err(RpcError::invalid_params("expected axis param")),
    }
}

/// `f` on the engine thread, its `Err` passed on.
async fn engine<R: Send + 'static>(
    d: &Daemon,
    f: impl FnOnce(&mut Engine) -> Result<R, RpcError> + Send + 'static,
) -> Result<R, RpcError> {
    d.with_engine(f).await.unwrap_or_else(|| Err(RpcError::new(super::rpc::INTERNAL_ERROR, "internal error")))
}

/// Any device connected: PHD2's gear dialog locks its choices then.
fn any_connected(e: &Engine) -> bool {
    let s = e.snapshot();
    [&s.camera, &s.mount, &s.aux_mount, &s.ao, &s.rotator].into_iter().flatten().any(|v| v.connected)
}

/// The profile changed under the engine: its settings read again.
fn reload(e: &mut Engine) -> Result<Value, RpcError> {
    e.handle(Command::ProfileChanged).map_err(|e| RpcError::new(e.code, e.message))
}

/// One of the UI's methods.
pub async fn dispatch(d: &Daemon, call: &Call) -> Result<Value, RpcError> {
    let p = call.params();
    let p = &p;
    match call.method.name {
        "select_star" => command(d, Command::SelectStar { x: num(p, "x")?, y: num(p, "y")? }).await,
        "nudge_lock" => command(d, Command::NudgeLock { dx: num(p, "dx")?, dy: num(p, "dy")? }).await,
        "save_lock" => command(d, Command::SaveLock).await,
        "restore_lock" => command(d, Command::RestoreLock).await,
        "bookmark_add" => {
            let op = match p.param("at").and_then(Value::as_str) {
                None | Some("lock") => BookmarkOp::Lock,
                Some("star") => BookmarkOp::Star,
                Some(_) => return Err(RpcError::invalid_params("expected at param")),
            };
            command(d, Command::Bookmark(op)).await
        }
        "bookmark_toggle" => {
            command(d, Command::Bookmark(BookmarkOp::Toggle(Point::new(num(p, "x")?, num(p, "y")?)))).await
        }
        "bookmark_delete" => {
            command(d, Command::Bookmark(BookmarkOp::Delete(Point::new(num(p, "x")?, num(p, "y")?)))).await
        }
        "bookmarks_clear" => command(d, Command::Bookmark(BookmarkOp::Clear)).await,
        "set_profile_value" => {
            let key = text(p, "key")?.to_string();
            let value = need(p.param("value").cloned(), "value")?;
            if !key.starts_with('/') {
                return Err(RpcError::invalid_params("expected key param"));
            }
            engine(d, move |e| {
                use crate::engine::calibration::ProfileAccess;
                let global = GLOBAL_KEYS.contains(&key.as_str());
                let mut prof = if global { e.store().global() } else { e.store().profile() };
                match &value {
                    Value::Bool(b) => prof.set_bool(&key, *b),
                    Value::Number(n) if n.is_i64() => prof.set_i64(&key, n.as_i64().unwrap_or(0)),
                    Value::Number(n) => prof.set_f64(&key, n.as_f64().unwrap_or(0.0)),
                    Value::String(s) => prof.set_str(&key, s),
                    _ => return Err(RpcError::invalid_params("expected value param")),
                }
                drop(prof);
                reload(e)
            })
            .await
        }
        "new_profile" => {
            let name = text(p, "name")?.to_string();
            engine(d, move |e| match e.store().create_profile(&name) {
                true => Err(e1("a profile of that name exists")),
                false => Ok(json!(e.store().profile_id(&name))),
            })
            .await
        }
        "rename_profile" => {
            let (id, name) = (int(p, "id")?, text(p, "name")?.to_string());
            engine(d, move |e| {
                let old = e.store().profile_name(id);
                match e.store().profile_exists(id) && !e.store().rename_profile(&old, &name) {
                    true => Ok(json!(0)),
                    false => Err(e1("could not rename the profile")),
                }
            })
            .await
        }
        "delete_profile" => {
            let id = int(p, "id")?;
            engine(d, move |e| {
                if !e.store().profile_exists(id) {
                    return Err(e1("invalid profile id"));
                }
                let current = e.store().current_profile_id() == id;
                if current && any_connected(e) {
                    return Err(e1("cannot delete the current profile when equipment is connected"));
                }
                let name = e.store().profile_name(id);
                e.store().delete_profile(&name);
                if current { reload(e) } else { Ok(json!(0)) }
            })
            .await
        }
        "import_profile" => {
            let text = text(p, "text")?.to_string();
            let name = p.param("name").and_then(Value::as_str).unwrap_or("Imported profile").to_string();
            engine(d, move |e| {
                if any_connected(e) {
                    return Err(e1("cannot set profile when equipment is connected"));
                }
                // `ReadProfile` names the profile after the file.
                if e.store().read_profile(&format!("{name}.phd"), &text) {
                    return Err(e1("invalid profile file"));
                }
                reload(e)?;
                Ok(json!(e.store().current_profile_id()))
            })
            .await
        }
        "export_profile" => {
            let id = p.param("id").and_then(int_param);
            engine(d, move |e| {
                if id.is_some_and(|id| id != e.store().current_profile_id()) {
                    return Err(e1("only the current profile can be exported"));
                }
                Ok(json!({"name": e.store().current_profile(), "text": e.store().write_profile()}))
            })
            .await
        }
        "export_settings" => engine(d, |e| Ok(json!({"text": e.store().save_all()}))).await,
        "set_equipment" => set_equipment(d, p).await,
        "list_devices" => Ok(list_devices(d)),
        "set_algorithm" => {
            let kind = AlgoKind::from_i32(int(p, "algorithm")?).ok_or_else(|| e1("unknown algorithm"))?;
            command(d, Command::SetAlgorithm { axis: axis(p)?, kind }).await
        }
        "reset_algo_params" => command(d, Command::ResetAlgoParams { axis: axis(p)? }).await,
        "set_manual_pointing" => {
            let pointing = match p.param("ra") {
                None | Some(Value::Null) => None,
                Some(_) => {
                    let side = match p.param("pier_side").and_then(Value::as_str) {
                        Some("East") => PierSide::East,
                        Some("West") => PierSide::West,
                        _ => PierSide::Unknown,
                    };
                    let lst = local_sidereal_hours(num(p, "longitude")?, super::events::now());
                    Some(ManualPointing { ra_hours: num(p, "ra")?, dec_deg: num(p, "dec")?, pier_side: side, lst_hours: lst })
                }
            };
            command(d, Command::SetManualPointing(pointing)).await
        }
        "set_calibration_data" => {
            // The units `get_calibration_data` answers in: degrees, px/s.
            let which = match p.param("which").and_then(Value::as_str) {
                Some("ao") => Which::Ao,
                _ => Which::Mount,
            };
            let cal = Calibration {
                x_rate: num(p, "xRate")? / 1000.0,
                y_rate: num(p, "yRate")? / 1000.0,
                x_angle: num(p, "xAngle")?.to_radians(),
                y_angle: num(p, "yAngle")?.to_radians(),
                declination: num(p, "declination")?.to_radians(),
                binning: p.param("binning").and_then(int_param).unwrap_or(1) as u16,
                is_valid: true,
                ..Calibration::default()
            };
            command(d, Command::SetCalibrationData { which, cal }).await
        }
        "set_dither_mode" => {
            let mode = if text(p, "mode")?.eq_ignore_ascii_case("spiral") { DitherMode::Spiral } else { DitherMode::Random };
            command(d, Command::SetDitherMode { mode, ra_only: flag(p, "ra_only")?, scale: num(p, "scale")? }).await
        }
        "set_auto_exposure" => {
            let (min_ms, max_ms, target_snr) = (int(p, "min")?, int(p, "max")?, num(p, "target_snr")?);
            command(d, Command::SetAutoExposure { min_ms, max_ms, target_snr }).await
        }
        "set_image_logger" => {
            // The dialog's fields; one left out takes the default.
            let b = |n: &str, v: bool| p.param(n).and_then(bool_param).unwrap_or(v);
            let f = |n: &str, v: f64| p.param(n).and_then(float_param).unwrap_or(v);
            let base = ImageLoggerSettings::default();
            let ui = ImageLoggerSettings {
                logging_enabled: flag(p, "enabled")?,
                log_frames_over_thresh_rel: b("over_rel", base.log_frames_over_thresh_rel),
                log_frames_over_thresh_px: b("over_px", base.log_frames_over_thresh_px),
                log_frames_dropped: b("dropped", base.log_frames_dropped),
                log_auto_select_frames: b("auto_select", base.log_auto_select_frames),
                log_next_n_frames: b("next_n", base.log_next_n_frames),
                guide_error_thresh_rel: f("thresh_rel", base.guide_error_thresh_rel),
                guide_error_thresh_px: f("thresh_px", base.guide_error_thresh_px),
                log_next_n_frames_count: p
                    .param("next_n_count")
                    .and_then(int_param)
                    .map_or(base.log_next_n_frames_count, |n| n.max(0) as u32),
            };
            command(d, Command::SetImageLogger(ui)).await
        }
        "ga_open" => command(d, Command::GaOpen).await,
        "ga_close" => command(d, Command::GaClose).await,
        "ga_start" => command(d, Command::GaStart).await,
        "ga_stop" => command(d, Command::GaStop).await,
        "ga_end_wait" => {
            let accept = p.param("accept").and_then(bool_param).unwrap_or(false);
            command(d, Command::GaEndWait { accept }).await
        }
        "ga_set_measure_backlash" => command(d, Command::GaSetMeasureBacklash(flag(p, "on")?)).await,
        "ga_apply" => {
            let tag = text(p, "kind")?;
            let kind = RecKind::ALL.into_iter().find(|k| k.tag() == tag).ok_or_else(|| e1("unknown recommendation"))?;
            command(d, Command::GaApply(kind)).await
        }
        "ga_history" => engine(d, |e| Ok(json!(assistant::history(&e.store().profile())))).await,
        "ga_review" => command(d, Command::GaReview(text(p, "timestamp")?.to_string())).await,
        "darks_build" => {
            let (min, max, count) = (int(p, "min_ms")?, int(p, "max_ms")?, int(p, "count")?);
            let exposures_ms = d.snapshot().exposure_durations.iter().copied().filter(|&ms| ms >= min && ms <= max).collect();
            let new_library = p.param("new_library").and_then(bool_param).unwrap_or(false);
            let note = p.param("note").and_then(Value::as_str).unwrap_or("").to_string();
            command(d, Command::DarksBuild(DarksBuildKind::Library { exposures_ms, count, new_library, note })).await
        }
        "darks_stop" => command(d, Command::DarksStop).await,
        "darks_use" => {
            let mut r = json!({});
            if let Some(on) = p.param("dark").and_then(bool_param) {
                r["dark"] = command(d, Command::DarksUse(on)).await?;
            }
            if let Some(on) = p.param("defect").and_then(bool_param) {
                r["defect"] = command(d, Command::DefectMapUse(on)).await?;
            }
            Ok(r)
        }
        "bpm_build" => {
            let (exp_ms, count) = (int(p, "exposure_ms")?, int(p, "count")?);
            // The Darks dialog's note goes into the defect map's master dark too.
            let note = p.param("note").and_then(Value::as_str).unwrap_or("").to_string();
            command(d, Command::DarksBuild(DarksBuildKind::DefectMap { exp_s: exp_ms / 1000, count, note })).await
        }
        "pa_drift_open" => command(d, Command::PolarDriftOpen).await,
        "pa_drift_start" => command(d, Command::PolarDriftStart).await,
        "pa_drift_stop" => command(d, Command::PolarDriftStop).await,
        "pa_drift_close" => command(d, Command::PolarDriftClose).await,
        "starcross_start" => {
            let pulse_count = p.param("pulse_count").and_then(int_param);
            let pulse_size = p.param("pulse_size").and_then(int_param);
            command(d, Command::StarCrossStart { pulse_count, pulse_size }).await
        }
        "starcross_stop" => command(d, Command::StarCrossStop).await,
        "pa_static_start" => command(d, Command::StaticPaOpen).await,
        "pa_static_rotate" => command(d, Command::StaticPaRotate).await,
        "pa_static_mark" => command(d, Command::StaticPaMark(int(p, "position")?)).await,
        "pa_static_clear" => command(d, Command::StaticPaClear).await,
        "pa_static_set" => {
            let b = |n: &str| p.param(n).and_then(bool_param);
            let set = StaticPaSet {
                manual: b("manual"),
                north: b("north"),
                hour_angle: p.param("hour_angle").and_then(float_param),
                flip: b("flip"),
                ref_star: p.param("ref_star").and_then(int_param).and_then(|i| usize::try_from(i).ok()),
            };
            command(d, Command::StaticPaSet(set)).await
        }
        "pa_static_stop" => command(d, Command::StaticPaClose).await,
        "comet_train_start" => command(d, Command::CometTrainStart).await,
        "cal_assistant_start" => command(d, Command::CalAssistant(CalAssistCmd::Open)).await,
        "cal_assistant_set" => {
            let offset = p.param("offset").and_then(int_param);
            let dec = p.param("dec").and_then(int_param);
            let east = p.param("east").and_then(bool_param);
            command(d, Command::CalAssistant(CalAssistCmd::Set { offset, dec, east })).await
        }
        "cal_assistant_location" => {
            let c = match text(p, "action")? {
                "save" => {
                    let single_side = p.param("single_side").and_then(bool_param).unwrap_or(false);
                    CalAssistCmd::SaveCustom { single_side }
                }
                "load" => CalAssistCmd::LoadCustom,
                "restore" => CalAssistCmd::Restore,
                _ => return Err(RpcError::invalid_params("expected action param")),
            };
            command(d, Command::CalAssistant(c)).await
        }
        "cal_assistant_slew" => command(d, Command::CalAssistant(CalAssistCmd::Slew)).await,
        "cal_assistant_calibrate" => command(d, Command::CalAssistant(CalAssistCmd::Calibrate)).await,
        "cal_assistant_cancel" => command(d, Command::CalAssistant(CalAssistCmd::Stop)).await,
        "cal_assistant_stop" => command(d, Command::CalAssistant(CalAssistCmd::Close)).await,
        "comet_train_stop" => command(d, Command::CometTrainStop).await,
        "list_logs" => list_logs(&d.options().engine.log_dir),
        "read_log" => {
            let tail = p.param("tail").and_then(Value::as_u64);
            read_log(&d.options().engine.log_dir, text(p, "name")?, tail)
        }
        name if super::rpc::method(name, true).is_some() => Err(e1("not available")),
        _ => Err(RpcError::new(METHOD_NOT_FOUND, "method not found")),
    }
}

/// The gear dialog's choices and the INDI device names, in the profile;
/// refused while anything is connected, as the dialog locks them.
async fn set_equipment(d: &Daemon, p: &Params<'_>) -> Result<Value, RpcError> {
    use crate::engine::calibration::ProfileAccess;
    const KEYS: [(&str, &str); 10] = [
        ("camera", "/camera/LastMenuChoice"),
        ("mount", "/scope/LastMenuChoice"),
        ("aux_mount", "/scope/LastAuxMenuChoice"),
        ("ao", "/stepguider/LastMenuChoice"),
        ("rotator", "/rotator/LastMenuChoice"),
        ("camera_device", "/indi/INDIcam"),
        ("mount_device", "/indi/INDImount"),
        ("ao_device", "/indi/INDIao"),
        ("rotator_device", "/indi/INDIrotator"),
        ("indi_host", "/indi/INDIhost"),
    ];
    let strings: Vec<(&str, String)> =
        KEYS.iter().filter_map(|(n, k)| p.param(n).and_then(Value::as_str).map(|v| (*k, v.to_string()))).collect();
    let ccd = p.param("camera_ccd").and_then(int_param);
    let port = p.param("indi_port").and_then(int_param);
    engine(d, move |e| {
        if any_connected(e) {
            return Err(e1("cannot change equipment when equipment is connected"));
        }
        let mut prof = e.store().profile();
        for (key, v) in &strings {
            prof.set_str(key, v);
        }
        if let Some(c) = ccd {
            prof.set_i32("/indi/INDIcam_ccd", c);
        }
        if let Some(port) = port {
            prof.set_i32("/indi/INDIport", port);
        }
        drop(prof);
        reload(e)
    })
    .await
}

/// `guider_devices`: the INDI server of the gear last connected and every
/// device it described, with the roles the chooser offers it for; empty
/// before a connect (and with `--sim`).
pub fn list_devices(d: &Daemon) -> Value {
    let Some(c) = d.indi_client() else {
        return json!({"host": null, "port": null, "connected": false, "devices": []});
    };
    // The client asks indiserver for its watched devices only (PHD2's
    // watchDevice), so the list holds those and whatever else it sent.
    let link = c.lock();
    let roles = [
        (Role::Camera, "camera"),
        (Role::Mount, "mount"),
        (Role::AuxMount, "aux_mount"),
        (Role::Ao, "ao"),
        (Role::Rotator, "rotator"),
    ];
    let devices: Vec<Value> = link
        .store
        .devices()
        .iter()
        .map(|dev| {
            let i = dev.driver_interface();
            let fits: Vec<&str> = roles.iter().filter(|(r, _)| i == 0 || r.includes(i)).map(|(_, n)| *n).collect();
            json!({
                "name": dev.name,
                "interfaces": i,
                "roles": fits,
                "connected": link.device_connected(&dev.name) == Some(true),
            })
        })
        .collect();
    json!({"host": c.host(), "port": c.port(), "connected": link.is_up(), "devices": devices})
}

/// The log folder's PHD2 logs, newest first.
fn list_logs(dir: &str) -> Result<Value, RpcError> {
    let mut logs: Vec<(String, u64, u64)> = std::fs::read_dir(dir)
        .map_err(|_| e1("cannot read the log folder"))?
        .flatten()
        .filter_map(|f| {
            let name = f.file_name().into_string().ok()?;
            let meta = f.metadata().ok().filter(|m| m.is_file())?;
            let mtime = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            (name.starts_with("PHD2_") && name.ends_with(".txt")).then_some((name, meta.len(), mtime))
        })
        .collect();
    logs.sort_by(|a, b| b.2.cmp(&a.2).then(b.0.cmp(&a.0)));
    Ok(Value::Array(logs.into_iter().map(|(n, size, t)| json!({"name": n, "size": size, "modified": t})).collect()))
}

/// The log called `name` in the log folder, or its last `tail` bytes.
fn read_log(dir: &str, name: &str, tail: Option<u64>) -> Result<Value, RpcError> {
    if name.contains('/') || !name.starts_with("PHD2_") || !name.ends_with(".txt") {
        return Err(RpcError::invalid_params("expected name param"));
    }
    let bytes = std::fs::read(std::path::Path::new(dir).join(name)).map_err(|_| e1("cannot read the log"))?;
    let from = tail.map_or(0, |t| bytes.len().saturating_sub(t as usize));
    Ok(json!({"name": name, "size": bytes.len(), "text": String::from_utf8_lossy(&bytes[from..])}))
}

/// The local sidereal time at `longitude_deg` (east positive), hours, at
/// `utc` seconds since 1970 (the IAU 1982 GMST to first order).
fn local_sidereal_hours(longitude_deg: f64, utc: f64) -> f64 {
    let days = utc / 86400.0 - 10957.5; // since J2000.0
    (18.697374558 + 24.06570982441908 * days + longitude_deg / 15.0).rem_euclid(24.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidereal_time_and_logs() {
        // 2000-01-01 12:00 UT at Greenwich: GMST 18.697 h.
        assert!((local_sidereal_hours(0.0, 946_728_000.0) - 18.697374558).abs() < 1e-9);
        assert!((local_sidereal_hours(15.0, 946_728_000.0) - 19.697374558).abs() < 1e-9);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-extras");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PHD2_GuideLog_a.txt"), "line 1\nline 2\n").unwrap();
        std::fs::write(dir.join("other.txt"), "x").unwrap();
        let d = dir.to_str().unwrap();
        let logs = list_logs(d).unwrap();
        assert_eq!(logs.as_array().unwrap().len(), 1);
        assert_eq!(logs[0]["name"], "PHD2_GuideLog_a.txt");
        assert_eq!(read_log(d, "PHD2_GuideLog_a.txt", Some(7)).unwrap()["text"], "line 2\n");
        assert!(read_log(d, "../PHD2_x.txt", None).is_err());
        assert!(read_log(d, "other.txt", None).is_err());
    }

    use crate::daemon::tests::{dir, start};
    use crate::server::methods::run;

    async fn call(d: &Daemon, name: &str, params: Value) -> Result<Value, RpcError> {
        let method = crate::server::rpc::method(name, true).expect("known");
        run(d, &Call { method, params: Some(params), id: Some(json!(1)) }).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn profiles_settings_and_tools_on_the_simulator() {
        let d = start(&dir("extras"), |_| {});
        assert_eq!(call(&d, "new_profile", json!({"name": "Refractor"})).await, Ok(json!(2)));
        assert_eq!(call(&d, "new_profile", json!(["Refractor"])).await, Err(e1("a profile of that name exists")));
        assert_eq!(call(&d, "rename_profile", json!({"id": 2, "name": "RC8"})).await, Ok(json!(0)));
        let exported = call(&d, "export_profile", json!({})).await.unwrap();
        assert_eq!(exported["name"], "My Equipment");
        assert!(exported["text"].as_str().unwrap().starts_with("PHD Profile 1\n"));
        assert_eq!(call(&d, "export_profile", json!({"id": 2})).await, Err(e1("only the current profile can be exported")));
        // A setting written and read again by the engine.
        let r = call(&d, "set_profile_value", json!({"key": "/ExposureDurationMs", "value": 2000})).await;
        assert_eq!(r, Ok(json!(0)));
        assert_eq!(d.snapshot().exposure_setting_ms, 2000);
        assert_eq!(call(&d, "set_profile_value", json!({"key": "x", "value": 1})).await.unwrap_err().code, -32602);
        // A global goes to PHD2's Global section, not the profile.
        let r = call(&d, "set_profile_value", json!({"key": "/server/guide_allow_recalibrate", "value": false})).await;
        assert_eq!(r, Ok(json!(0)));
        let (global, profile) = d
            .with_engine(|e| {
                let key = "/server/guide_allow_recalibrate";
                let g = e.store().global_entries().any(|(k, v)| k == key && *v == json!(false));
                let p = e.store().profile_entries().any(|(k, _)| k == key);
                (g, p)
            })
            .await
            .unwrap();
        assert!(global && !profile);
        // The current profile goes; the first one left is current.
        assert_eq!(call(&d, "delete_profile", json!({"id": 1})).await, Ok(json!(0)));
        assert_eq!(d.snapshot().profile_name, "RC8");
        let r = call(&d, "import_profile", json!({"text": "PHD Profile 1\n/ExposureDurationMs\t1\t1500\n", "name": "Mak"})).await;
        assert_eq!(r, Ok(json!(1)));
        assert_eq!((d.snapshot().profile_name.as_str(), d.snapshot().exposure_setting_ms), ("Mak", 1500));
        assert_eq!(call(&d, "ga_history", json!({})).await, Ok(json!([])));
        assert_eq!(call(&d, "list_devices", json!({})).await.unwrap()["devices"], json!([]));
        assert_eq!(call(&d, "drift_align_start", json!({})).await, Err(e1("not available")));
        // The image logger's dialog: the thresholds held to 1–10.
        let r = call(&d, "set_image_logger", json!({"enabled": true, "dropped": true, "thresh_px": 20.0})).await;
        assert_eq!(r, Ok(json!(0)));
        let px = engine(&d, |e| {
            use crate::engine::calibration::ProfileAccess;
            Ok(e.store().profile().get_f64("/ImageLogger/ErrorThreshPx", 0.0))
        });
        assert_eq!(px.await, Ok(10.0));
        assert_eq!(call(&d, "set_image_logger", json!({})).await.unwrap_err().message, "expected enabled param");
        assert_eq!(call(&d, "select_star", json!({"x": 1})).await.unwrap_err().message, "expected y param");
        d.shutdown().await;
        d.close();
    }
}
