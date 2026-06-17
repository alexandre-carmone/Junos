//! Retry/refresh helpers for INDI property fetches.
//!
//! `processDeviceCommands` in kstars/ekos/ekoslive/message.cpp:1664 silently
//! drops `device_property_get` / `_subscribe` when the INDI driver isn't
//! registered yet. Drivers may take seconds to come up after profile start,
//! so we retry on a short cadence until data lands, then switch to the
//! long-lived refresh loop to keep things current.

use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

use super::{DeviceStore, SendCmd};

/// Repeatedly subscribe+get an INDI property on a device until `done_pred`
/// is satisfied (i.e. the expected data has landed in the store), or we
/// give up after a reasonable timeout. Needed because `processDeviceCommands`
/// in KStars silently drops commands while the INDI driver isn't registered
/// yet (`message.cpp:1664`), and drivers may take seconds to come up.
pub(super) fn spawn_retry_property<T, F>(
    send: SendCmd,
    device: String,
    property: &'static str,
    signal: RwSignal<T>,
    done_pred: F,
)
where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> bool + 'static,
{
    spawn_retry_property_with(send, device, property, true, signal, done_pred);
}

/// Like `spawn_retry_property`, but lets the caller request `compact: false`
/// so switch/text element labels come back. Needed for properties whose
/// option list uses human labels (CCD_CAPTURE_FORMAT, CCD_TRANSFER_FORMAT,
/// CCD_ISO, FILTER_NAME).
pub(super) fn spawn_retry_property_with<T, F>(
    send: SendCmd,
    device: String,
    property: &'static str,
    compact: bool,
    signal: RwSignal<T>,
    done_pred: F,
)
where
    T: Clone + Send + Sync + 'static,
    F: Fn(&T) -> bool + 'static,
{
    use gloo_timers::future::TimeoutFuture;
    spawn_local(async move {
        leptos::logging::log!("[ws] retry_property start device={} prop={}", device, property);
        // First shot — subscribe (persistent push) + get (fast path).
        let sub = serde_json::json!({
            "type":"device_property_subscribe",
            "payload":{ "device": device, "properties":[property] }
        }).to_string();
        let get = serde_json::json!({
            "type":"device_property_get",
            "payload":{ "device": device, "property": property, "compact": compact }
        }).to_string();
        send(sub.clone());
        send(get.clone());
        leptos::logging::log!("[ws] retry_property sent subscribe+get for {}", property);

        // Retry budget: 60 attempts × 1s = 1 minute.
        for i in 0..60 {
            TimeoutFuture::new(1_000).await;
            if done_pred(&signal.get_untracked()) {
                leptos::logging::log!("[ws] retry_property done for {} after {}s", property, i + 1);
                return;
            }
            send(sub.clone());
            send(get.clone());
        }
        leptos::logging::log!("[ws] retry_property giving up on {} after 60s", property);
    });
}

/// Long-lived periodic refresh: re-requests key INDI properties every few
/// seconds so the UI stays current even if KStars drops a push subscription.
/// Complements `spawn_retry_property` (which handles fast bootstrap then stops).
pub(super) fn spawn_refresh_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    let online   = store.online;
    let trains   = store.optical_trains;
    let dustcap  = store.dustcap_status;

    spawn_local(async move {
        loop {
            TimeoutFuture::new(5_000).await;

            if !online.get_untracked() {
                continue;
            }
            let train_list = trains.get_untracked();
            let Some(train) = train_list.first() else { continue };

            // ── Ekos module-level state ─────────────────────────────
            send(r#"{"type":"get_devices","payload":{}}"#.to_string());
            send(r#"{"type":"get_states","payload":{}}"#.to_string());
            send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
            send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
            send(r#"{"type":"train_get_profiles","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
            send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"align_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"guide_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"scheduler_get_all_settings","payload":{}}"#.to_string());
            send(r#"{"type":"scheduler_get_jobs","payload":{}}"#.to_string());
            send(r#"{"type":"option_get","payload":{"options":[{"name":"GuiderType"},{"name":"PHD2Host"},{"name":"PHD2Port"},{"name":"LinGuiderHost"},{"name":"LinGuiderPort"}]}}"#.to_string());

            // ── Camera INDI properties ───────────────────────────────
            if !train.camera.is_empty() && train.camera != "--" {
                for prop in ["CCD_INFO", "CCD_TEMPERATURE", "CCD_COOLER"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": train.camera, "property": prop, "compact": true }
                    }).to_string());
                }
                // Combo option lists need labels — compact:false.
                for prop in ["CCD_CAPTURE_FORMAT", "CCD_TRANSFER_FORMAT", "CCD_FRAME_TYPE", "CCD_ISO"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": train.camera, "property": prop, "compact": false }
                    }).to_string());
                }
            }

            // ── Filter wheel INDI properties ─────────────────────────
            if !train.filterwheel.is_empty() && train.filterwheel != "--" {
                send(serde_json::json!({
                    "type": "device_property_get",
                    "payload": { "device": train.filterwheel, "property": "FILTER_NAME", "compact": false }
                }).to_string());
                send(serde_json::json!({
                    "type": "device_property_get",
                    "payload": { "device": train.filterwheel, "property": "FILTER_SLOT", "compact": true }
                }).to_string());
            }

            // ── Mount INDI properties ────────────────────────────────
            // EQUATORIAL_EOD_COORD is owned by `spawn_mount_coord_loop`
            // (faster cadence + re-subscribe); intentionally not polled here.

            // ── Focuser INDI properties ──────────────────────────────
            if !train.focuser.is_empty() && train.focuser != "--" {
                for prop in ["ABS_FOCUS_POSITION", "FOCUS_TEMPERATURE"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": train.focuser, "property": prop, "compact": true }
                    }).to_string());
                }
            }

            // ── Dust-cap / flat-panel INDI properties ────────────────
            if let Some(cap) = dustcap.get_untracked() {
                if !cap.device.is_empty() {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": cap.device, "property": "CAP_PARK", "compact": true }
                    }).to_string());
                    if cap.has_light_panel {
                        for prop in ["FLAT_LIGHT_CONTROL", "FLAT_LIGHT_INTENSITY"] {
                            send(serde_json::json!({
                                "type": "device_property_get",
                                "payload": { "device": cap.device, "property": prop, "compact": true }
                            }).to_string());
                        }
                    }
                }
            }
        }
    });
}

/// Dedicated, self-healing freshness loop for the mount's RA/Dec.
///
/// `spawn_retry_property` only bootstraps EQUATORIAL_EOD_COORD then stops once
/// the first value lands, and a one-shot `device_property_subscribe` can be
/// silently dropped by KStars (driver not registered / settleStatus not yet
/// Success — message.cpp:1664, pitfalls #1/#3) and is never renewed. That left
/// the planetarium mount reticle showing a stale position that never recovered
/// — visibly wrong on-sky and still wrong after a plate-solve + sync.
///
/// This loop owns EQUATORIAL_EOD_COORD: it re-polls every 1.5 s (so the marker
/// tracks slews/syncs without the 5 s lag) and re-subscribes every ~12 s (so a
/// dropped push self-heals). It resolves the mount device from the active
/// optical train, falling back to the device name learned from prior
/// `new_mount_state`/coord pushes (`mount_status.device`) when the train has no
/// mount set.
pub(super) fn spawn_mount_coord_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    let online = store.online;
    let trains = store.optical_trains;
    let mount_status = store.mount_status;

    spawn_local(async move {
        let mut tick: u32 = 0;
        loop {
            TimeoutFuture::new(1_500).await;

            if !online.get_untracked() {
                continue;
            }

            // Prefer the active train's mount; fall back to the device name we
            // already learned from coord pushes when the train has none.
            let device = trains
                .get_untracked()
                .first()
                .map(|t| t.mount.clone())
                .filter(|m| !m.is_empty() && m != "--")
                .or_else(|| {
                    mount_status
                        .get_untracked()
                        .map(|m| m.device)
                        .filter(|d| !d.is_empty() && d != "--")
                });
            let Some(device) = device else { continue };

            send(serde_json::json!({
                "type": "device_property_get",
                "payload": { "device": device, "property": "EQUATORIAL_EOD_COORD", "compact": true }
            }).to_string());

            // Re-subscribe every ~12 s so a silently-dropped push subscription
            // recovers on its own; the 1.5 s poll guarantees freshness meanwhile.
            if tick % 8 == 0 {
                send(serde_json::json!({
                    "type": "device_property_subscribe",
                    "payload": { "device": device, "properties": ["EQUATORIAL_EOD_COORD"] }
                }).to_string());
            }
            tick = tick.wrapping_add(1);
        }
    });
}
