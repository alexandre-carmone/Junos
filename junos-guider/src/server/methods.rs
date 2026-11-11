//! The 54 methods of PHD2's event server (`event_server.cpp:2383`): each
//! call's parameters checked as PHD2's handler checks them (its `-32602`
//! and code-1 texts), then a [`Command`] for the engine, or the answer read
//! from the snapshot for the read-only ones — Ekos polls `get_app_state`
//! and `get_connected` every few seconds, mid-exposure too. The engine's
//! refusals carry PHD2's codes and texts already.
//!
//! Kept as PHD2 does it:
//! - `set_lock_position` refused is `-32600`; `set_connected` takes a JSON
//!   bool only; `clear_calibration` with no params at all clears both, with
//!   params but no `which` the mount.
//! - `get_pixel_scale` is `null` when the scale is 1.0 (unknown focal length
//!   or pixel size); `get_exposure_durations` is the list as PHD2 has it.
//! - The algorithm methods act on pMount: the AO when there is one.
//! - `get_star_image` errors only before a star is selected or without a
//!   frame, never on a star lost while guiding; it sends at most 63 × 63
//!   pixels, as native-endian u16 in base64.
//! - `shutdown` answers before anything stops.
//!
//! The junos UI's own methods (`ws_only`, which the TCP server never looks
//! up) go to [`super::extras`]: one dispatcher for the event server and the
//! relay's `guider_rpc`.
//!
//! Fixed (a crash in PHD2): `set_algo_param` without `value` answers
//! "expected param value param" where PHD2 dereferences null.
//!
//! Ported from PHD2 src/event_server.cpp — BSD-3-Clause, see THIRD_PARTY.md

use std::f64::consts::PI;
use std::path::Path;

use serde_json::{Map, Value, json};

use super::events::mounts;
use super::rpc::{
    Call, INTERNAL_ERROR, METHOD_NOT_FOUND, Params, RpcError, bool_param, bool_value, float_param, int_param, num_f,
    num_g, num_point, point_param, rect_param, roi_param, settle_param, string_param,
};
use crate::daemon::Daemon;
use crate::engine::algo::Axis;
use crate::engine::guider::{GuiderState, PauseType, ShiftUnits};
use crate::engine::scope::DecGuideMode;
use crate::engine::traits::{Dir, Parity, Point, Rect};
use crate::engine::{AlgoView, Command, Reply, SingleFrame, Snapshot, Which};

type Outcome = Result<Value, RpcError>;

fn outcome(r: Reply) -> Outcome {
    r.map_err(|e| RpcError::new(e.code, e.message))
}

/// `jrpc_error(code, msg)`.
fn err(code: i32, msg: impl Into<String>) -> Outcome {
    Err(RpcError::new(code, msg))
}

/// `-32602`.
fn bad(msg: impl Into<String>) -> RpcError {
    RpcError::invalid_params(msg)
}

pub(super) async fn command(d: &Daemon, cmd: Command) -> Outcome {
    outcome(d.command(cmd).await)
}

/// One call, answered.
pub async fn run(d: &Daemon, call: &Call) -> Outcome {
    if call.method.ws_only {
        return super::extras::dispatch(d, call).await;
    }
    let p = call.params();
    let snap = d.snapshot();
    let camera_connected = snap.camera.as_ref().is_some_and(|c| c.connected);
    match call.method.name {
        "clear_calibration" => {
            let which = match call.params {
                None => Which::Both,
                Some(_) => which_mount(p.param("which"))
                    .ok_or_else(|| bad("expected param \"mount\", \"ao\", or \"both\""))?,
            };
            command(d, Command::ClearCalibration(which)).await
        }
        "deselect_star" => command(d, Command::DeselectStar).await,
        "get_exposure" => Ok(json!(snap.exposure_ms)),
        "set_exposure" => {
            let ms = p.param("exposure").and_then(int_param).ok_or_else(|| bad("expected exposure param"))?;
            command(d, Command::SetExposure(ms)).await
        }
        "get_exposure_durations" => Ok(json!(snap.exposure_durations)),
        "get_profiles" => {
            let profiles = d.with_engine(|e| e.store().profiles()).await.ok_or_else(internal)?;
            let list = profiles.into_iter().map(|(id, name, selected)| {
                let mut t = json!({ "id": id, "name": name });
                if selected {
                    t["selected"] = json!(true);
                }
                t
            });
            Ok(Value::Array(list.collect()))
        }
        "get_profile" => Ok(json!({ "id": snap.profile_id, "name": snap.profile_name })),
        "set_profile" => {
            let id = p.param("id").and_then(int_param).ok_or_else(|| bad("expected profile id param"))?;
            outcome(d.set_profile(id).await)
        }
        "get_connected" => Ok(json!(snap.connected)),
        "set_connected" => match p.param("connected").and_then(bool_value) {
            Some(on) => outcome(d.set_connected(on).await),
            None => Err(bad("expected connected boolean param")),
        },
        "get_calibrated" => Ok(json!(snap.calibrated)),
        "get_paused" => Ok(json!(snap.paused != PauseType::None)),
        "set_paused" => {
            let paused = p.param("paused").and_then(bool_param).ok_or_else(|| bad("expected bool param at index 0"))?;
            let mut full = false;
            if paused && let Some(t) = p.param("type") {
                // Anything but "full" (exact case) pauses the guiding only.
                full = string_param(t).ok_or_else(|| bad("expected string param at index 1"))? == "full";
            }
            command(d, Command::SetPaused { paused, full }).await
        }
        "get_lock_position" => Ok(snap.lock_position.map_or(Value::Null, point)),
        "set_lock_position" => {
            let (x, y) = match (p.param("x").and_then(float_param), p.param("y").and_then(float_param)) {
                (Some(x), Some(y)) => (x, y),
                _ => return Err(bad("expected lock position x, y params")),
            };
            let exact = match p.param("exact") {
                None => true,
                Some(v) => bool_param(v).ok_or_else(|| bad("expected boolean param at index 2"))?,
            };
            command(d, Command::SetLockPosition { x, y, exact }).await
        }
        "loop" => command(d, Command::Loop).await,
        "stop_capture" => command(d, Command::StopCapture).await,
        "guide" => {
            let settle = settle_param(p.param("settle"))?;
            let recalibrate = match p.param("recalibrate") {
                None => false,
                Some(v) => bool_param(v).ok_or_else(|| bad("expected bool value for recalibrate"))?,
            };
            let roi = roi_param(p.param("roi"))?;
            command(d, Command::Guide { settle, recalibrate, roi }).await
        }
        "dither" => {
            let amount = p.param("amount").and_then(float_param).ok_or_else(|| bad("expected dither amount param"))?;
            let ra_only = match p.param("raOnly") {
                None => false,
                Some(v) => bool_param(v).ok_or_else(|| bad("expected dither raOnly param"))?,
            };
            let settle = settle_param(p.param("settle"))?;
            command(d, Command::Dither { amount, ra_only, settle }).await
        }
        "find_star" => {
            let roi = roi_param(p.param("roi"))?;
            // The lock position, as a `PHD_Point`.
            let lock = command(d, Command::FindStar { roi }).await?;
            match lock.as_array().map(|a| a.iter().map(Value::as_f64).collect::<Vec<_>>()).as_deref() {
                Some([Some(x), Some(y)]) => Ok(num_point(*x, *y)),
                _ => Ok(lock),
            }
        }
        "get_pixel_scale" => Ok(if snap.pixel_scale == 1.0 { Value::Null } else { num_g(snap.pixel_scale) }),
        "get_app_state" => Ok(json!(snap.app_state.map_or("Stopped", |a| a.as_str()))),
        "flip_calibration" => command(d, Command::FlipCalibration).await,
        "get_lock_shift_enabled" => Ok(json!(snap.lock_shift.enabled)),
        "set_lock_shift_enabled" => {
            let on = p.param("enabled").and_then(bool_param).ok_or_else(|| bad("expected enabled boolean param"))?;
            command(d, Command::SetLockShiftEnabled(on)).await
        }
        "get_lock_shift_params" => Ok(lock_shift(&snap, p.param("axes"))),
        "set_lock_shift_params" => {
            let (rate, units, mount_coords) = parse_lock_shift(call.params.as_ref())?;
            command(d, Command::SetLockShiftParams { rate, units, mount_coords }).await
        }
        "save_image" => command(d, Command::SaveImage).await,
        "get_star_image" => {
            let size = match p.param("size") {
                None => 15,
                Some(v) => int_param(v).filter(|&n| n >= 15).ok_or_else(|| bad("invalid image size param"))?,
            };
            star_image(&snap, size)
        }
        "get_use_subframes" => Ok(json!(snap.camera.is_some() && snap.use_subframes)),
        "get_search_region" => Ok(json!(snap.search_region)),
        // `TerminateApp` only posts the close: the caller runs it once the
        // reply is out (the TCP reader's `Shared::shutdown`, the relay
        // socket's `Daemon::shutdown`).
        "shutdown" => Ok(json!(0)),
        "get_camera_binning" if camera_connected => Ok(json!(snap.binning)),
        "get_camera_frame_size" if camera_connected => Ok(json!([snap.frame_size.0, snap.frame_size.1])),
        "get_camera_binning" | "get_camera_frame_size" => err(1, "camera not connected"),
        "get_current_equipment" => {
            let mut t = Map::new();
            let devices = [
                ("camera", &snap.camera),
                ("mount", &snap.mount),
                ("aux_mount", &snap.aux_mount),
                ("AO", &snap.ao),
                ("rotator", &snap.rotator),
            ];
            for (key, dev) in devices {
                if let Some(dev) = dev {
                    t.insert(key.into(), json!({ "name": dev.name, "connected": dev.connected }));
                }
            }
            Ok(Value::Object(t))
        }
        "get_guide_output_enabled" => match mounts(&snap).0 {
            Some(m) => Ok(json!(m.guiding_enabled)),
            None => err(1, "mount not defined"),
        },
        "set_guide_output_enabled" => {
            let on = p.param("enabled").and_then(bool_param).ok_or_else(|| bad("expected enabled boolean param"))?;
            command(d, Command::SetGuideOutputEnabled(on)).await
        }
        "get_algo_param_names" => {
            let Some(axis) = axis_param(p.param("axis")) else { return err(1, "expected axis name param") };
            let mut names = vec![json!("algorithmName")];
            if let Some(a) = algo(&snap, axis) {
                names.extend(a.params.iter().map(|(n, _)| json!(n)));
            }
            Ok(Value::Array(names))
        }
        "get_algo_param" => {
            let Some(axis) = axis_param(p.param("axis")) else { return err(1, "expected axis name param") };
            let Some(name) = p.param("name").and_then(string_param) else {
                return err(1, "expected param name param");
            };
            match algo(&snap, axis) {
                Some(a) if name == "algorithmName" => Ok(json!(a.class_name)),
                Some(a) => match a.params.iter().find(|(n, _)| n == name) {
                    Some((_, v)) => Ok(num_g(*v)),
                    None => err(1, "could not get param"),
                },
                None => err(1, "could not get param"),
            }
        }
        "set_algo_param" => {
            let Some(axis) = axis_param(p.param("axis")) else { return err(1, "expected axis name param") };
            let Some(name) = p.param("name").and_then(string_param) else {
                return err(1, "expected param name param");
            };
            let Some(value) = p.param("value").and_then(float_param) else {
                return err(1, "expected param value param");
            };
            command(d, Command::SetAlgoParam { axis, name: name.to_string(), value }).await
        }
        "get_dec_guide_mode" => Ok(json!(snap.dec_guide_mode.unwrap_or(DecGuideMode::None).label())),
        "set_dec_guide_mode" => {
            let Some(mode) = p.param("mode").and_then(string_param) else { return err(1, "expected mode param") };
            let modes = [DecGuideMode::None, DecGuideMode::Auto, DecGuideMode::North, DecGuideMode::South];
            match modes.into_iter().find(|m| m.label().eq_ignore_ascii_case(mode)) {
                Some(m) => command(d, Command::SetDecGuideMode(m)).await,
                None => err(1, "invalid dec guide mode param"),
            }
        }
        "get_settling" => Ok(json!(snap.settling)),
        "guide_pulse" => {
            let Some(amount) = p.param("amount").and_then(int_param) else { return err(1, "expected amount param") };
            let Some(dir) = direction(p.param("direction")) else { return err(1, "expected direction param") };
            let which = match which_mount(p.param("which")) {
                Some(w @ (Which::Mount | Which::Ao)) => w,
                _ => return err(1, "invalid 'which' param"),
            };
            command(d, Command::GuidePulse { amount, dir, which }).await
        }
        "get_calibration_data" => {
            let m = match which_mount(p.param("which")) {
                Some(Which::Mount) => &snap.mount_calibration,
                Some(Which::Ao) => &snap.ao_calibration,
                _ => return err(1, "invalid 'which' param"),
            };
            if !m.present || !m.connected {
                return err(1, "device not connected");
            }
            let mut r = Map::new();
            r.insert("calibrated".into(), json!(m.calibrated));
            if m.calibrated {
                let deg = |rad: f64| rad * 180.0 / PI;
                r.insert("xAngle".into(), num_f(deg(m.x_angle), 1));
                r.insert("xRate".into(), num_f(m.x_rate * 1000.0, 3));
                r.insert("xParity".into(), json!(parity(m.x_parity)));
                r.insert("yAngle".into(), num_f(deg(m.y_angle), 1));
                r.insert("yRate".into(), num_f(m.y_rate * 1000.0, 3));
                r.insert("yParity".into(), json!(parity(m.y_parity)));
                r.insert("declination".into(), num_g(deg(m.declination)));
            }
            Ok(Value::Object(r))
        }
        "capture_single_frame" => {
            if snap.capture_active {
                return err(1, "cannot capture single frame when capture is currently active");
            }
            // Fix: PHD2 goes on without a camera (event_server.cpp:1389).
            if !camera_connected {
                return err(1, "cannot capture single frame when camera is not connected");
            }
            let req = single_frame(&p, &snap)?;
            command(d, Command::CaptureSingleFrame(req)).await
        }
        "get_cooler_status" => {
            if !camera_connected {
                return err(1, "camera not connected");
            }
            let Some(c) = snap.cooler else { return err(1, "failed to get cooler status") };
            let mut r = Map::new();
            r.insert("coolerOn".into(), json!(c.on));
            r.insert("temperature".into(), num_f(c.temperature, 1));
            if c.on {
                r.insert("setpoint".into(), num_f(c.setpoint, 1));
                r.insert("power".into(), num_f(c.power, 1));
            }
            Ok(Value::Object(r))
        }
        "set_cooler_state" => {
            let on = p.param("enabled").and_then(bool_param).ok_or_else(|| bad("expected enabled boolean param"))?;
            command(d, Command::SetCoolerState(on)).await
        }
        "get_ccd_temperature" => {
            if !camera_connected {
                return err(1, "camera not connected");
            }
            match snap.sensor_temperature {
                Some(t) => Ok(json!({ "temperature": num_f(t, 1) })),
                None => err(1, "failed to get sensor temperature"),
            }
        }
        "export_config_settings" => {
            let text = d.with_engine(|e| e.store().save_all()).await.ok_or_else(internal)?;
            let dir = d.data_dir();
            let file = format!("{dir}/phd2_settings.txt");
            // `GetDefaultFileDir` makes the folder when it is missing.
            if std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&file, text)).is_err() {
                return err(1, "export settings failed");
            }
            Ok(json!({ "filename": file }))
        }
        "get_variable_delay_settings" => {
            let (enabled, short_ms, long_ms) = snap.var_delay;
            Ok(json!({
                "Enabled": enabled,
                "ShortDelaySeconds": short_ms / 1000,
                "LongDelaySeconds": long_ms / 1000,
            }))
        }
        "set_variable_delay_settings" => {
            let enabled = p.param("Enabled").and_then(bool_param);
            let short = p.param("ShortDelaySeconds").and_then(float_param);
            let long = p.param("LongDelaySeconds").and_then(float_param);
            let (Some(enabled), Some(short), Some(long)) = (enabled, short, long) else {
                return Err(bad("expected Enabled, ShortDelaySeconds, LongDelaySeconds params)"));
            };
            // `(int) shortDelaySec * 1000`: the seconds truncated first.
            let (short_ms, long_ms) = ((short as i32).wrapping_mul(1000), (long as i32).wrapping_mul(1000));
            command(d, Command::SetVariableDelay { enabled, short_ms, long_ms }).await
        }
        // Neither the INDI camera nor the simulator limits frames.
        "get_limit_frame" => Ok(json!({ "roi": null })),
        "set_limit_frame" => {
            let roi = match p.param("roi") {
                None => return Err(bad("missing required param `roi`")),
                Some(Value::Null) => None,
                Some(v) => Some(rect_param(v).ok_or_else(|| bad("invalid ROI param"))?),
            };
            command(d, Command::SetLimitFrame(roi)).await
        }
        // Every PHD2 name is above; the UI's went to `extras`.
        _ => err(METHOD_NOT_FOUND, "method not found"),
    }
}

fn internal() -> RpcError {
    RpcError::new(INTERNAL_ERROR, "internal error")
}

/// `NV(PHD_Point)`: `[%.2f,%.2f]`.
fn point(p: Point) -> Value {
    num_point(p.x, p.y)
}

/// `which_mount`: no param is the mount; `None` for anything but "mount",
/// "ao" or "both" (any case).
fn which_mount(v: Option<&Value>) -> Option<Which> {
    let Some(v) = v else { return Some(Which::Mount) };
    let s = string_param(v)?;
    [("ao", Which::Ao), ("mount", Which::Mount), ("both", Which::Both)]
        .into_iter()
        .find_map(|(n, w)| s.eq_ignore_ascii_case(n).then_some(w))
}

/// `axis_param`: "ra" or "x", "dec" or "y", any case.
fn axis_param(v: Option<&Value>) -> Option<Axis> {
    let s = v.and_then(string_param)?;
    let is = |n: &str| s.eq_ignore_ascii_case(n);
    if is("ra") || is("x") {
        Some(Axis::X)
    } else if is("dec") || is("y") {
        Some(Axis::Y)
    } else {
        None
    }
}

/// `dir_param`: PHD2's twelve spellings, any case.
fn direction(v: Option<&Value>) -> Option<Dir> {
    let s = v.and_then(string_param)?.to_ascii_lowercase();
    Some(match s.as_str() {
        "n" | "north" | "up" => Dir::North,
        "s" | "south" | "down" => Dir::South,
        "e" | "east" | "right" => Dir::East,
        "w" | "west" | "left" => Dir::West,
        _ => return None,
    })
}

/// `parity_str`.
fn parity(p: Parity) -> &'static str {
    match p {
        Parity::Even => "+",
        Parity::Odd => "-",
        Parity::Unknown => "?",
    }
}

/// pMount's algorithm on an axis.
fn algo(snap: &Snapshot, axis: Axis) -> Option<&AlgoView> {
    match axis {
        Axis::X => snap.algo_x.as_ref(),
        Axis::Y => snap.algo_y.as_ref(),
    }
}

/// `get_lock_shift_params`: the shift as set, or with `axes` "x/y" or
/// "camera" the lock's rate in camera px/hr. A rate never worked out is
/// read as zero (PHD2 multiplies an unset point).
fn lock_shift(snap: &Snapshot, axes: Option<&Value>) -> Value {
    let camera = axes.map(|v| string_param(v).unwrap_or("")).is_some_and(|a| {
        a.eq_ignore_ascii_case("x/y") || a.eq_ignore_ascii_case("camera")
    });
    let l = snap.lock_shift;
    let (rate, units, mount_coords) = if camera {
        let r = snap.lock_shift_rate.unwrap_or_default();
        (Point::new(r.x * 3600.0, r.y * 3600.0), ShiftUnits::Pixels, false)
    } else {
        (l.rate, l.units, l.mount_coords)
    };
    json!({
        "enabled": l.enabled,
        "rate": point(rate),
        "units": if matches!(units, ShiftUnits::Arcsec) { "arcsec/hr" } else { "pixels/hr" },
        "axes": if mount_coords { "RA/Dec" } else { "X/Y" },
    })
}

/// `parse_lock_shift_params`: an object, or an array holding one.
fn parse_lock_shift(params: Option<&Value>) -> Result<(Point, ShiftUnits, bool), RpcError> {
    let params = match params {
        Some(Value::Array(a)) => a.first(),
        p => p,
    };
    let p = Params::new(&["rate", "units", "axes"], params);
    let rate = p.param("rate").and_then(point_param).ok_or_else(|| bad("expected rate value array"))?;
    let text = |name| p.param(name).map_or("", |v| string_param(v).unwrap_or(""));
    let units = match text("units") {
        u if u.eq_ignore_ascii_case("arcsec/hr") || u.eq_ignore_ascii_case("arc-sec/hr") => ShiftUnits::Arcsec,
        u if u.eq_ignore_ascii_case("pixels/hr") => ShiftUnits::Pixels,
        _ => return Err(bad("expected units 'arcsec/hr' or 'pixels/hr'")),
    };
    let mount_coords = match text("axes") {
        a if a.eq_ignore_ascii_case("RA/Dec") => true,
        a if a.eq_ignore_ascii_case("X/Y") => false,
        _ => return Err(bad("expected axes 'RA/Dec' or 'X/Y'")),
    };
    Ok((rate, units, mount_coords))
}

/// `capture_single_frame`'s parameters, checked in PHD2's order; the
/// engine checks what needs the camera again.
fn single_frame(p: &Params, snap: &Snapshot) -> Result<SingleFrame, RpcError> {
    let mut req = SingleFrame::default();
    if let Some(v) = p.param("exposure") {
        let ms = int_param(v).filter(|n| (1..=600_000).contains(n));
        req.exposure_ms = Some(ms.ok_or_else(|| bad("expected exposure param"))?);
    }
    if let Some(v) = p.param("binning") {
        let max = snap.max_binning as i32;
        let bin = int_param(v).filter(|n| (1..=max).contains(n));
        req.binning = Some(bin.ok_or_else(|| bad(format!("invalid binning value: min=1, max={max}")))?);
    }
    if let Some(v) = p.param("gain") {
        let gain = int_param(v).filter(|n| (0..=100).contains(n));
        req.gain = Some(gain.ok_or_else(|| bad("invalid gain value: must be between 0 and 100"))?);
    }
    if let Some(v) = p.param("subframe") {
        req.subframe = rect_param(v).ok_or_else(|| bad("invalid subframe param"))?;
    }
    if let Some(v) = p.param("path") {
        let path = string_param(v).ok_or_else(|| bad("invalid path param: string expected"))?;
        if !Path::new(path).is_absolute() {
            return Err(bad("path param must be an absolute path"));
        }
        if Path::new(path).exists() {
            return Err(bad("destination file already exists"));
        }
        req.path = Some(path.to_string());
    }
    req.save = req.path.is_some();
    if let Some(v) = p.param("save") {
        req.save = bool_param(v).ok_or_else(|| bad("save param must be a boolean"))?;
    }
    if !req.save && req.path.is_some() {
        return Err(bad("path param not allowed when save = false"));
    }
    Ok(req)
}

/// `get_star_image`: up to 63 × 63 pixels around the star, inside the
/// subframe or the frame.
pub fn star_image(snap: &Snapshot, size: i32) -> Outcome {
    let selected = snap.guider_state.is_some_and(|st| st as i32 >= GuiderState::Selected as i32);
    let (Some(img), Some(star), true) = (&snap.image, snap.star_position, selected) else {
        return err(2, "no star selected");
    };
    let halfw = ((size - 1) / 2).min(31);
    let fullw = 2 * halfw + 1;
    // `rint`: half to even.
    let (sx, sy) = (star.x.round_ties_even() as i32, star.y.round_ties_even() as i32);
    let frame = match img.subframe {
        Some(sf) if !sf.is_empty() => sf,
        _ => Rect::new(0, 0, img.w, img.h),
    };
    // A subframe may stick out of the frame.
    let rect = Rect::new(sx - halfw, sy - halfw, fullw, fullw).intersect(&frame).intersect(&Rect::new(0, 0, img.w, img.h));
    let mut bytes = Vec::with_capacity((rect.w.max(0) * rect.h.max(0) * 2) as usize);
    for y in rect.y..=rect.bottom() {
        let row = (y * img.w + rect.x) as usize;
        for v in &img.data[row..row + rect.w as usize] {
            bytes.extend_from_slice(&v.to_ne_bytes());
        }
    }
    Ok(json!({
        "frame": img.frame_num,
        "width": rect.w,
        "height": rect.h,
        "star_pos": num_point(star.x - rect.x as f64, star.y - rect.y as f64),
        "pixels": base64(&bytes),
    }))
}

/// `B64Encode`: the standard alphabet, `=` padded.
pub fn base64(data: &[u8]) -> String {
    const E: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().fold(0u32, |t, &b| t << 8 | b as u32) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(E[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_as_phd2() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0x00, 0x01]), "//4AAQ==");
    }

    #[test]
    fn parameter_spellings() {
        assert_eq!(which_mount(None), Some(Which::Mount));
        assert_eq!(which_mount(Some(&json!("AO"))), Some(Which::Ao));
        assert_eq!(which_mount(Some(&json!("Both"))), Some(Which::Both));
        assert_eq!(which_mount(Some(&json!("scope"))), None);
        assert_eq!(which_mount(Some(&json!(1))), None);
        assert_eq!(axis_param(Some(&json!("RA"))), Some(Axis::X));
        assert_eq!(axis_param(Some(&json!("y"))), Some(Axis::Y));
        assert_eq!(axis_param(Some(&json!("z"))), None);
        assert_eq!(direction(Some(&json!("Up"))), Some(Dir::North));
        assert_eq!(direction(Some(&json!("w"))), Some(Dir::West));
        // Dir::parse's one-letter u/d/l/r are not PHD2's.
        assert_eq!(direction(Some(&json!("u"))), None);
        assert_eq!(direction(None), None);
    }

    #[test]
    fn lock_shift_params_both_shapes() {
        let o = json!({"rate": [3.3, 1.1], "units": "Arc-Sec/hr", "axes": "ra/dec"});
        assert_eq!(parse_lock_shift(Some(&o)).unwrap(), (Point::new(3.3, 1.1), ShiftUnits::Arcsec, true));
        let a = json!([{"rate": [1, 2], "units": "pixels/hr", "axes": "X/Y"}]);
        assert_eq!(parse_lock_shift(Some(&a)).unwrap(), (Point::new(1.0, 2.0), ShiftUnits::Pixels, false));
        let msg = |v: Value| parse_lock_shift(Some(&v)).unwrap_err().message;
        assert_eq!(msg(json!([[1, 2], "pixels/hr", "X/Y"])), "expected rate value array");
        assert_eq!(msg(json!({"rate": [1, 2]})), "expected units 'arcsec/hr' or 'pixels/hr'");
        assert_eq!(msg(json!({"rate": [1, 2], "units": "pixels/hr", "axes": 3})), "expected axes 'RA/Dec' or 'X/Y'");
    }

    #[test]
    fn star_image_box() {
        use crate::engine::image::UsImage;
        let mut img = UsImage::new(100, 80);
        for (k, v) in img.data.iter_mut().enumerate() {
            *v = k as u16;
        }
        img.frame_num = 9;
        let mut snap = Snapshot {
            image: Some(std::sync::Arc::new(img)),
            star_position: Some(Point::new(2.5, 40.4)),
            guider_state: Some(GuiderState::Selecting),
            ..Default::default()
        };
        assert_eq!(star_image(&snap, 32).unwrap_err().code, 2);
        snap.guider_state = Some(GuiderState::Guiding);
        // A 31 × 31 box around (2, 40), cut by the frame's left edge.
        let r = star_image(&snap, 32).unwrap();
        assert_eq!((r["frame"].as_u64(), r["width"].as_i64(), r["height"].as_i64()), (Some(9), Some(18), Some(31)));
        assert_eq!(r["star_pos"], json!([2.5, 15.4]));
        let px = r["pixels"].as_str().unwrap();
        assert_eq!(px.len(), (18 * 31 * 2usize).div_ceil(3) * 4);
        // The first pixel is (0, 25): 2500, native-endian.
        assert_eq!(&px[..2], &base64(&2500u16.to_ne_bytes())[..2]);
        // The cap: 63 × 63 at most.
        snap.star_position = Some(Point::new(50.0, 40.0));
        let r = star_image(&snap, 500).unwrap();
        assert_eq!((r["width"].as_i64(), r["height"].as_i64()), (Some(63), Some(63)));
    }
}
