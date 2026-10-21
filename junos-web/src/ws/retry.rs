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
        debug_log!("[ws] retry_property start device={} prop={}", device, property);
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
        debug_log!("[ws] retry_property sent subscribe+get for {}", property);

        // Retry budget: 60 attempts × 1s = 1 minute.
        for i in 0..60 {
            TimeoutFuture::new(1_000).await;
            if done_pred(&signal.get_untracked()) {
                debug_log!("[ws] retry_property done for {} after {}s", property, i + 1);
                return;
            }
            send(sub.clone());
            send(get.clone());
        }
        debug_log!("[ws] retry_property giving up on {} after 60s", property);
    });
}

/// `document.hidden`: the page is in a background tab or the phone's screen
/// is off. Nothing is drawn, so the refresh loops skip their sweep rather
/// than keep the wifi busy with settings snapshots nobody sees.
fn page_hidden() -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .map(|d| d.hidden())
        .unwrap_or(false)
}

/// Long-lived periodic refresh: re-requests key INDI properties every 20 s
/// so the UI stays current even if KStars drops a push subscription.
/// Complements `spawn_retry_property` (which handles fast bootstrap then stops).
///
/// 20 s, not 5: every sweep is ~30 requests each answered by a full settings
/// snapshot, and KStars pushes most of this state on change anyway. Over a
/// weak wifi link the sweeps competed with the preview frames.
pub(super) fn spawn_refresh_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    let online = store.online;

    spawn_local(async move {
        loop {
            TimeoutFuture::new(20_000).await;

            if !online.get_untracked() || page_hidden() {
                continue;
            }
            if store.optical_trains.with_untracked(|t| t.is_empty()) { continue; }
            // Each module is polled on the device from its own assigned train:
            // camera + filter wheel from Capture ("1"), focuser from Focus ("2").
            let cap_train   = store.module_train_untracked("1");
            let focus_train = store.module_train_untracked("2");

            // ── Ekos module-level state ─────────────────────────────
            for cmd in super::BOOTSTRAP_CMDS {
                send(cmd.to_string());
            }

            // ── Camera INDI properties (Capture train) ───────────────
            if let Some(cam) = cap_train.as_ref().map(|t| t.camera.as_str())
                .filter(|c| !c.is_empty() && *c != "--")
            {
                for prop in ["CCD_INFO", "CCD_BINNING", "CCD_TEMPERATURE", "CCD_COOLER"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": cam, "property": prop, "compact": true }
                    }).to_string());
                }
                // Combo option lists need labels — compact:false.
                for prop in ["CCD_CAPTURE_FORMAT", "CCD_TRANSFER_FORMAT", "CCD_FRAME_TYPE", "CCD_ISO"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": cam, "property": prop, "compact": false }
                    }).to_string());
                }
            }

            // ── Filter wheel INDI properties (Capture train) ─────────
            if let Some(fw) = cap_train.as_ref().map(|t| t.filterwheel.as_str())
                .filter(|f| !f.is_empty() && *f != "--")
            {
                send(serde_json::json!({
                    "type": "device_property_get",
                    "payload": { "device": fw, "property": "FILTER_NAME", "compact": false }
                }).to_string());
                send(serde_json::json!({
                    "type": "device_property_get",
                    "payload": { "device": fw, "property": "FILTER_SLOT", "compact": true }
                }).to_string());
            }

            // ── Mount INDI properties ────────────────────────────────
            // EQUATORIAL_EOD_COORD is owned by `spawn_mount_coord_loop`
            // (faster cadence + re-subscribe); intentionally not polled here.

            // ── Focuser INDI properties (Focus train) ────────────────
            if let Some(foc) = focus_train.as_ref().map(|t| t.focuser.as_str())
                .filter(|f| !f.is_empty() && *f != "--")
            {
                for prop in ["ABS_FOCUS_POSITION", "FOCUS_TEMPERATURE"] {
                    send(serde_json::json!({
                        "type": "device_property_get",
                        "payload": { "device": foc, "property": prop, "compact": true }
                    }).to_string());
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
/// This loop owns EQUATORIAL_EOD_COORD: it re-polls every 1.5 s while a tab
/// that draws the mount is active (`store.mount_fast_poll`, set in `main.rs`:
/// Sky, Mount, Polar Align), every 6 s otherwise, and re-subscribes every
/// ~12 s (so a dropped push self-heals). It resolves the mount device from the
/// active optical train, falling back to the device name learned from prior
/// `new_mount_state`/coord pushes (`mount_status.device`) when the train has no
/// mount set.
pub(super) fn spawn_mount_coord_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    let online = store.online;
    let mount_status = store.mount_status;
    let fast = store.mount_fast_poll;

    spawn_local(async move {
        let mut tick: u32 = 0;
        loop {
            let fast_now = fast.get_untracked();
            TimeoutFuture::new(if fast_now { 1_500 } else { 6_000 }).await;

            if !online.get_untracked() || page_hidden() {
                continue;
            }

            // Prefer the Mount module's assigned train ("3"); fall back to the
            // device name we already learned from coord pushes when it has none.
            let device = store
                .module_train_untracked("3")
                .map(|t| t.mount)
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
            // recovers on its own; the poll guarantees freshness meanwhile.
            if tick % (if fast_now { 8 } else { 2 }) == 0 {
                send(serde_json::json!({
                    "type": "device_property_subscribe",
                    "payload": { "device": device, "properties": ["EQUATORIAL_EOD_COORD"] }
                }).to_string());
            }
            tick = tick.wrapping_add(1);
        }
    });
}

/// Build Filter Offsets follows: KStars only answers
/// `filter_offset_get_all_settings` (message.cpp:6065), the pushes carry the
/// progress but not the table, so while a run is on (`filter_offsets_phase`
/// Running) the table is asked for every 3 s — app-level, so a run started
/// from the sheet still ends in the store once the sheet is closed.
pub(super) fn spawn_filter_offsets_loop(send: SendCmd, store: DeviceStore) {
    use gloo_timers::future::TimeoutFuture;

    spawn_local(async move {
        loop {
            TimeoutFuture::new(3_000).await;
            if !store.online.get_untracked() || page_hidden() {
                continue;
            }
            if store.filter_offsets_phase.get_untracked() == super::BfoPhase::Running {
                send(r#"{"type":"filter_offset_get_all_settings","payload":{}}"#.to_string());
            }
        }
    });
}
