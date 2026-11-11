//! Small helpers shared by the tab modules. Centralises three patterns that
//! were previously copy-pasted across `focus.rs`, `imaging.rs`,
//! `polar_align.rs`, `scheduler.rs` and `guide/mod.rs`:
//!
//!   * `send_cmd` — wrap `{type, payload}` JSON and push it to `SendCmd`.
//!   * `dispatch_setting` — single-key `*_set_all_settings` update, with or
//!     without a `{settings:{…}}` wrapper (every module takes the flat map
//!     today — see `message.cpp:673`; the wrapper is kept for callers that
//!     need one).
//!   * `option_set` / `option_get` — one global KStars `Options::` entry,
//!     by its Q_PROPERTY name (`ws/mod.rs::BOOTSTRAP_CMDS` says how it is
//!     spelled). `option_set` saves and answers nothing, so a caller re-asks
//!     with `option_get`, whose reply lands in `DeviceStore::options`.
//!   * `send_device_property_set` — INDI `device_property_set` with the
//!     `elements` array shaped by the caller.
//!
//! Plus `extract_indi_number` for reading a named element out of a
//! `device_property_get`/`set` payload's `numbers` array, `file_command`
//! for the KStars file loads/saves whose answer the caller waits for, and
//! `guider_rpc` for a junos-guider method call and its reply.

use leptos::prelude::*;

use crate::ws::{FileReply, GuiderPending, GuiderReply, GuiderRpcError, SendCmd};
use serde_json::{json, Map, Value};

pub fn send_cmd(send: &SendCmd, ty: &str, payload: Value) {
    send(json!({ "type": ty, "payload": payload }).to_string());
}

/// Send a file command (`scheduler_load_file`, `scheduler_save_file`,
/// `capture_load_sequence_file`) and wait up to `timeout_ms` for its answer in
/// `DeviceStore::file_reply`. `None` when KStars stays silent — Ekos not
/// started, or a save that wrote nothing.
pub async fn file_command(
    send: &SendCmd,
    reply: RwSignal<Option<FileReply>>,
    ty: &str,
    payload: Value,
    timeout_ms: u32,
) -> Option<FileReply> {
    const STEP_MS: u32 = 100;
    reply.set(None);
    send_cmd(send, ty, payload);
    for _ in 0..timeout_ms.div_ceil(STEP_MS) {
        gloo_timers::future::TimeoutFuture::new(STEP_MS).await;
        if let Some(r) = reply.try_get_untracked().flatten().filter(|r| r.cmd == ty) {
            return Some(r);
        }
    }
    None
}

/// How long a `guider_rpc` waits for its `guider_rpc_result`.
const GUIDER_RPC_TIMEOUT_MS: u32 = 10_000;

/// Call a junos-guider method (PHD2's, or one of the UI's extras) through the
/// relay: `guider_rpc {method, params, id}`. The returned signal turns
/// `Some` once with the `guider_rpc_result` (`DeviceStore::guider_pending`),
/// a timeout after 10 s, or [`GuiderRpcError::LOST`] when the socket or the
/// daemon goes first. Ids are unique to this page, since the relay hands
/// every browser every reply.
pub fn guider_rpc(
    send: &SendCmd,
    pending: GuiderPending,
    method: &str,
    params: Value,
) -> RwSignal<Option<GuiderReply>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static PAGE: OnceLock<u32> = OnceLock::new();
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let page = PAGE.get_or_init(|| (js_sys::Math::random() * f64::from(u32::MAX)) as u32);
    let id = format!("{page:08x}-{}", SEQ.fetch_add(1, Ordering::Relaxed));

    let reply = RwSignal::new(None);
    pending.update_value(|m| {
        m.insert(id.clone(), reply);
    });
    send_cmd(send, "guider_rpc", json!({ "method": method, "params": params, "id": id }));
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(GUIDER_RPC_TIMEOUT_MS).await;
        if let Some(r) = pending.try_update_value(|m| m.remove(&id)).flatten() {
            r.set(Some(Err(GuiderRpcError {
                code: GuiderRpcError::TIMEOUT,
                message: "no reply from the guider".into(),
            })));
        }
    });
    reply
}

/// `option_set {options:[{name, value}]}` — message.cpp:1654. No reply.
pub fn option_set(send: &SendCmd, name: &str, value: Value) {
    send_cmd(send, "option_set", json!({ "options": [{ "name": name, "value": value }] }));
}

/// `option_get {options:[{name}, …]}` — answered under the same type with
/// `[{name, value}, …]` (message.cpp:1663).
pub fn option_get(send: &SendCmd, names: &[&str]) {
    let options: Vec<Value> = names.iter().map(|n| json!({ "name": n })).collect();
    send_cmd(send, "option_get", json!({ "options": options }));
}

pub fn dispatch_setting(
    send: &SendCmd,
    cmd: &str,
    wrapper_key: Option<&str>,
    key: &str,
    value: Value,
) {
    let mut map = Map::new();
    map.insert(key.to_string(), value);
    let payload = match wrapper_key {
        Some(w) => json!({ w: Value::Object(map) }),
        None    => Value::Object(map),
    };
    send_cmd(send, cmd, payload);
}

pub fn send_device_property_set(send: &SendCmd, device: &str, property: &str, elements: Value) {
    send_cmd(send, "device_property_set", json!({
        "device": device,
        "property": property,
        "elements": elements,
    }));
}

/// Push a new observer location into KStars' live `geo()` truth. KStars exposes
/// no direct "set location" command; the only path is via a connected INDI
/// device: point KStars' `locationSource` at the mount, then write
/// `GEOGRAPHIC_COORD` to it. The driver echoes it back `IPS_OK` and KStars'
/// `GenericDevice::processNumber` (indistd.cpp:361) calls `setLocation()`.
/// INDI `LONG` is 0..360° East, so a Western (negative) longitude is wrapped.
/// `ELEV` is intentionally omitted so KStars keeps its current elevation.
pub fn push_site_to_kstars(send: &SendCmd, device: &str, lat: f64, lon: f64) {
    // "Set & leave": make the mount KStars' location master (idempotent).
    send_cmd(send, "option_set", json!({
        "options": [ { "name": "locationSource", "value": device } ],
    }));
    let lon_east = if lon < 0.0 { lon + 360.0 } else { lon };
    send_device_property_set(send, device, "GEOGRAPHIC_COORD", json!([
        { "name": "LAT",  "value": lat },
        { "name": "LONG", "value": lon_east },
    ]));
}

pub fn extract_indi_number(payload: &Value, name: &str) -> Option<f64> {
    payload["numbers"].as_array()?
        .iter()
        .find(|el| el["name"].as_str() == Some(name))
        .and_then(|el| el["value"].as_f64())
}
