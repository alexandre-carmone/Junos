//! Minimal WebSocket + DeviceStore for milestone 1.
//!
//! Scope: attach to the junos-server `/ws` endpoint and decode just the
//! Ekos Live messages the planetarium needs for the FOV reticle.
//!
//! # Where FOV inputs actually come from
//!
//! The FOV reticle needs focal length, aperture, pixel size and sensor
//! dimensions. These live in three different places in KStars:
//!
//! 1. **Scope focal length + aperture** — `get_scopes` returns the OAL
//!    scope DB as `[{name, focal_length, aperture, …}]`. We match the
//!    active train's `scope` field against this list by name.
//! 2. **Active train's scope/camera names** — `train_get_all` returns
//!    records `[{id, name, scope, camera, mount, …}]`. Take the first
//!    entry as the active train.
//! 3. **Camera pixel size + sensor dimensions** — these come from the
//!    INDI `CCD_INFO` number property. Fetch via
//!    `device_property_get {device: <camera name>, property: "CCD_INFO"}`.
//!    The reply comes back as a `device_property_get` event whose payload
//!    is `{device, name, state, numbers:[{name, value}, …]}` (compact
//!    form from `kstars/indi/indistd.cpp::numberToJson`). Relevant element
//!    names: `CCD_MAX_X`, `CCD_MAX_Y`, `CCD_PIXEL_SIZE_X`, `CCD_PIXEL_SIZE_Y`,
//!    or the fallback `CCD_PIXEL_SIZE`.
//!
//! Note: `train_settings_get` is NOT a source for FOV data — it returns
//! `OpticalTrainSettings`, a map of module-enum IDs (`"0"`, `"1"`, …) to
//! per-module configs (Capture/Focus/Guide/Align settings), not hardware.

mod retry;
mod store;
mod types;

use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use leptos::prelude::*;
use wasm_bindgen_futures::spawn_local;

pub use store::DeviceStore;
pub use types::*;

use retry::{spawn_mount_coord_loop, spawn_refresh_loop, spawn_retry_property, spawn_retry_property_with};

/// Type-erased command sink. Components dispatch raw Ekos Live JSON strings.
pub type SendCmd = Arc<dyn Fn(String) + Send + Sync>;

pub fn use_junos_ws() -> (DeviceStore, SendCmd) {
    let store = DeviceStore::new();

    let ws_url = {
        let window   = web_sys::window().unwrap();
        let location = window.location();
        let host     = location.host().unwrap_or_else(|_| "localhost:8080".into());
        let proto    = location.protocol().unwrap_or_else(|_| "http:".into());
        let ws_proto = if proto == "https:" { "wss:" } else { "ws:" };
        format!("{}//{}/ws", ws_proto, host)
    };

    let (cmd_tx, mut cmd_rx) = futures::channel::mpsc::unbounded::<String>();

    let send_fn: SendCmd = Arc::new(move |json: String| {
        let _ = cmd_tx.unbounded_send(json);
    });

    // Prime: once Ekos is online, fetch scope DB, active train list, and the
    // debounced focus settings snapshot. All bypass the Ekos::Success gate.
    let online_sig = store.online;
    {
        let prime_send = send_fn.clone();
        Effect::new(move |_| {
            if online_sig.get() {
                prime_send(r#"{"type":"file_default_path","payload":{"type":8}}"#.to_string());
                prime_send(r#"{"type":"get_devices","payload":{}}"#.to_string());
                prime_send(r#"{"type":"get_states","payload":{}}"#.to_string());
                prime_send(r#"{"type":"get_scopes","payload":{}}"#.to_string());
                prime_send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
                prime_send(r#"{"type":"train_get_profiles","payload":{}}"#.to_string());
                prime_send(r#"{"type":"focus_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"capture_get_sequences","payload":{}}"#.to_string());
                prime_send(r#"{"type":"align_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"guide_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"mount_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"scheduler_get_all_settings","payload":{}}"#.to_string());
                prime_send(r#"{"type":"scheduler_get_jobs","payload":{}}"#.to_string());
                prime_send(r#"{"type":"livestacker_get_all_settings","payload":{}}"#.to_string());
                // Guider-backend settings live in global KStars Options::,
                // not in guide_get_all_settings. See message.cpp:1418.
                prime_send(r#"{"type":"option_get","payload":{"options":[{"name":"GuiderType"},{"name":"PHD2Host"},{"name":"PHD2Port"},{"name":"LinGuiderHost"},{"name":"LinGuiderPort"}]}}"#.to_string());
            }
        });
    }

    // Profile list: not gated on `online`. KStars dispatches profile_*
    // commands before the Ekos-startup gate (message.cpp:249), so the
    // browser can list/CRUD/start profiles while Ekos is offline.
    // KStars also pushes `get_profiles` unsolicited on connect
    // (message.cpp:93), but if the browser opens after KStars is already
    // up that initial push is missed — re-fetch on every WS connect.
    let connected_sig = store.connected;
    {
        let prime_send = send_fn.clone();
        Effect::new(move |_| {
            if connected_sig.get() {
                prime_send(r#"{"type":"get_profiles","payload":{}}"#.to_string());
                prime_send(r#"{"type":"get_drivers","payload":{}}"#.to_string());
                // Note: the observer site (astro_get_location) is primed and
                // cached by junos-server on KStars attach, then replayed to each
                // browser on connect (proxy.rs) — no per-browser request needed.
                // The store arm for "astro_get_location" consumes that replay.
            }
        });
    }

    // Bind each Ekos module to its *saved* optical train. Without an explicit
    // train_set, Guide::m_Camera stays null and the first guide_start silently
    // no-ops via Guide::calibrate() → KSNotification::error (does not propagate
    // over Ekos Live). Focus/Capture happen to win the OpticalTrainManager::
    // updated race; Guide does not (guide.cpp:3479-3499 has no init-time call
    // to refreshOpticalTrain, unlike focus.cpp:8032-8055).
    //
    // We bind to the per-module assignment from `module_trains`
    // (train_get_profiles) — NOT blindly to the first train — otherwise every
    // fresh load would overwrite the user's rig-manager choices: TRAIN_SET →
    // setOpticalTrain(name) → combo currentIndexChanged → ProfileSettings::
    // setOneSetting persists it back (mount.cpp:1384, focus.cpp:8025, …).
    //
    // Gate on a loaded, non-empty profiles map so we never bind before the
    // saved assignments arrive. KStars sets all module keys on first train
    // creation (opticaltrainmanager.cpp:437-442), so the populated map is the
    // normal state; first-train is only a fallback for an unset key.
    //
    // (module, ProfileSettings enum key): 1=Capture 2=Focus 3=Mount 4=Guide
    // 5=Align — same mapping as the rig-manager ModuleAssign dropdowns.
    {
        let trains_sig = store.optical_trains;
        let profiles_sig = store.module_trains;
        let last_sig = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let send_for_effect = send_fn.clone();
        Effect::new(move |_| {
            let trains = trains_sig.get();
            let profiles = profiles_sig.get();
            let Some(first) = trains.first() else { return };
            if first.name.is_empty() { return; }
            // Wait for the saved per-module map before binding anything.
            if !profiles.as_object().map_or(false, |o| !o.is_empty()) {
                return;
            }
            // Resolve each module to its saved train name, falling back to the
            // first train only when the key is absent/unresolved.
            let resolve = |key: &str| -> String {
                profiles
                    .get(key)
                    .and_then(|v| v.as_i64())
                    .and_then(|id| trains.iter().find(|t| t.id == id))
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| first.name.clone())
            };
            let bindings: Vec<(&str, String)> = [
                ("capture", "1"),
                ("focus", "2"),
                ("mount", "3"),
                ("guide", "4"),
                ("align", "5"),
            ]
            .into_iter()
            .map(|(module, key)| (module, resolve(key)))
            .collect();
            // Only (re)send when a resolution actually changed, so the 5 s
            // refresh loop's train_get_all/profiles re-fetches don't re-spam.
            let signature = bindings
                .iter()
                .map(|(m, n)| format!("{m}={n}"))
                .collect::<Vec<_>>()
                .join(";");
            if signature == *last_sig.borrow() {
                return;
            }
            *last_sig.borrow_mut() = signature;
            for (module, name) in bindings {
                let msg = serde_json::json!({
                    "type": "train_set",
                    "payload": { "module": module, "name": name }
                })
                .to_string();
                send_for_effect(msg);
            }
        });
    }

    // Cross-reference: when both the scopes list and the capture train are
    // known, match that train's scope name against the scope DB and write
    // focal length + aperture into telescope_settings. The FOV reticle /
    // mosaic preview frame the *imaging* optics, so use the Capture-module
    // train (ProfileSettings key "1"), not blindly the first train.
    let trains_sig = store.optical_trains;
    let scopes_sig = store.scopes;
    let profiles_sig_fl = store.module_trains;
    let telescope_sig = store.telescope_settings;
    Effect::new(move |_| {
        let trains = trains_sig.get();
        let scopes = scopes_sig.get();
        let profiles = profiles_sig_fl.get();
        let cap_id = profiles.get("1").and_then(|v| v.as_i64());
        let train = match cap_id.and_then(|id| trains.iter().find(|t| t.id == id)) {
            Some(t) => t,
            None => match trains.first() {
                Some(t) => t,
                None => return,
            },
        };
        if train.scope.is_empty() || scopes.is_empty() { return; }
        if let Some(s) = scopes.iter().find(|s| s.name == train.scope) {
            // Apply the per-train focal reducer, mirroring KStars' framing
            // assistant (`framingassistantui.cpp:422`:
            // `reducedFocalLength = focalLen * focalReducer`). Without this,
            // the FOV preview overlay (mosaic + center reticle) would show
            // a smaller field than the actual capture / scheduler layout.
            let effective_focal = s.focal_length_mm * train.reducer;
            telescope_sig.update(|t| {
                t.focal_length_mm = Some(effective_focal);
                t.aperture_mm     = Some(s.aperture_mm);
            });
        }
    });

    // Watch for a dust-cap / flat-panel device coming online. The cap device
    // isn't part of the optical train, so we discover it via `get_devices`
    // and subscribe to its INDI properties directly.
    {
        let cap_sig = store.dustcap_status;
        let last_cap = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let send_for_effect = send_fn.clone();
        Effect::new(move |_| {
            let Some(cap) = cap_sig.get() else { return };
            if cap.device.is_empty() || cap.device == *last_cap.borrow() { return; }
            *last_cap.borrow_mut() = cap.device.clone();
            spawn_retry_property(
                send_for_effect.clone(),
                cap.device.clone(),
                "CAP_PARK",
                cap_sig,
                |opt| opt.as_ref().map_or(false, |c| c.park_state != crate::ws::DustCapParkState::Unknown),
            );
            if cap.has_light_panel {
                spawn_retry_property(
                    send_for_effect.clone(),
                    cap.device.clone(),
                    "FLAT_LIGHT_CONTROL",
                    cap_sig,
                    |opt| opt.as_ref().map_or(false, |c| c.light_on.is_some()),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    cap.device.clone(),
                    "FLAT_LIGHT_INTENSITY",
                    cap_sig,
                    |opt| opt.as_ref().map_or(false, |c| c.brightness.is_some()),
                );
            }
        });
    }

    // Subscribe + get the INDI properties we care about on the active train's
    // camera (CCD_INFO) and mount (EQUATORIAL_EOD_COORD).
    //
    // Pitfall: `processDeviceCommands` in kstars/ekos/ekoslive/message.cpp:1664
    // drops the command silently if `INDIListener::findDevice` fails — and the
    // INDI driver may not be registered yet right after profile start. So
    // fire-and-forget isn't enough: we retry on a timer until the property
    // actually comes back, then stop.
    {
        let trains_sig2 = store.optical_trains;
        let profiles_sig = store.module_trains;
        let camera_sig  = store.camera_status;
        let mount_sig   = store.mount_status;
        let focus_sig   = store.focus_status;
        let filter_sig  = store.filter_wheel_status;
        let guide_sig   = store.guide_status;
        let last_cam     = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_mount   = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_focuser = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_filter  = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let last_guider  = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let send_for_effect = send_fn.clone();
        Effect::new(move |_| {
            let trains = trains_sig2.get();
            let profiles = profiles_sig.get();
            if trains.is_empty() { return; }
            // Resolve each module to its assigned train (ProfileSettings key:
            // 1=Capture 2=Focus 3=Mount 4=Guide), falling back to the first
            // train when unassigned — so each tab subscribes to / displays the
            // device it actually uses, not blindly the first train's.
            let resolve = |key: &str| -> Option<OpticalTrain> {
                profiles
                    .get(key)
                    .and_then(|v| v.as_i64())
                    .and_then(|id| trains.iter().find(|t| t.id == id).cloned())
                    .or_else(|| trains.first().cloned())
            };
            let cam_train   = resolve("1");
            let focus_train = resolve("2");
            let mount_train = resolve("3");
            let guide_train = resolve("4");

            if let Some(train) = cam_train.as_ref().filter(|t| !t.camera.is_empty() && t.camera != "--")
                .filter(|t| *last_cam.borrow() != t.camera)
            {
                *last_cam.borrow_mut() = train.camera.clone();
                // Seed the camera device name so the tab header shows it before
                // CCD_INFO comes back.
                camera_sig.update(|opt| {
                    opt.get_or_insert_with(CameraStatusData::default).device = train.camera.clone();
                });
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_INFO",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.sensor_width).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_TEMPERATURE",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.temperature).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_COOLER",
                    camera_sig,
                    |cs| cs.as_ref().and_then(|c| c.cooler_on).is_some(),
                );
                // Switch properties whose option list is the human label
                // (compact:false). ISO is DSLR-only — give up gracefully.
                spawn_retry_property_with(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_CAPTURE_FORMAT",
                    false,
                    camera_sig,
                    |cs| cs.as_ref().map_or(false, |c| !c.capture_format_options.is_empty()),
                );
                spawn_retry_property_with(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_TRANSFER_FORMAT",
                    false,
                    camera_sig,
                    |cs| cs.as_ref().map_or(false, |c| !c.transfer_format_options.is_empty()),
                );
                spawn_retry_property_with(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_FRAME_TYPE",
                    false,
                    camera_sig,
                    |cs| cs.as_ref().map_or(false, |c| !c.frame_type_options.is_empty()),
                );
                spawn_retry_property_with(
                    send_for_effect.clone(),
                    train.camera.clone(),
                    "CCD_ISO",
                    false,
                    camera_sig,
                    |cs| cs.as_ref().map_or(false, |c| !c.iso_options.is_empty()),
                );
            }

            if let Some(train) = cam_train.as_ref().filter(|t| !t.filterwheel.is_empty() && t.filterwheel != "--")
                .filter(|t| *last_filter.borrow() != t.filterwheel)
            {
                *last_filter.borrow_mut() = train.filterwheel.clone();
                filter_sig.update(|opt| {
                    opt.get_or_insert_with(FilterWheelStatusData::default).device = train.filterwheel.clone();
                });
                spawn_retry_property_with(
                    send_for_effect.clone(),
                    train.filterwheel.clone(),
                    "FILTER_NAME",
                    false,
                    filter_sig,
                    |fs| fs.as_ref().map_or(false, |f| !f.filter_names.is_empty()),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.filterwheel.clone(),
                    "FILTER_SLOT",
                    filter_sig,
                    |fs| fs.as_ref().and_then(|f| f.current_slot).is_some(),
                );
            }

            if let Some(train) = mount_train.as_ref().filter(|t| !t.mount.is_empty() && t.mount != "--")
                .filter(|t| *last_mount.borrow() != t.mount)
            {
                *last_mount.borrow_mut() = train.mount.clone();
                mount_sig.update(|opt| {
                    opt.get_or_insert_with(MountStatusData::default).device = train.mount.clone();
                });
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.mount.clone(),
                    "EQUATORIAL_EOD_COORD",
                    mount_sig,
                    |ms| ms.as_ref().and_then(|m| m.ra_h).is_some(),
                );
            }

            if let Some(train) = focus_train.as_ref().filter(|t| !t.focuser.is_empty() && t.focuser != "--")
                .filter(|t| *last_focuser.borrow() != t.focuser)
            {
                *last_focuser.borrow_mut() = train.focuser.clone();
                // Seed focus_status with the focuser device name so UI has
                // something to show before the first new_focus_state lands.
                focus_sig.update(|opt| {
                    let fs = opt.get_or_insert_with(FocusStatusData::default);
                    fs.device = train.focuser.clone();
                });
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.focuser.clone(),
                    "ABS_FOCUS_POSITION",
                    focus_sig,
                    |fs| fs.as_ref().and_then(|f| f.position).is_some(),
                );
                spawn_retry_property(
                    send_for_effect.clone(),
                    train.focuser.clone(),
                    "FOCUS_TEMPERATURE",
                    focus_sig,
                    |fs| fs.as_ref().and_then(|f| f.temperature).is_some(),
                );
            }

            // Guide camera: the guide tab only displays its name (no INDI
            // props consumed here), so just seed the device from the Guide
            // module's train.
            if let Some(train) = guide_train.as_ref().filter(|t| !t.guider.is_empty() && t.guider != "--")
                .filter(|t| *last_guider.borrow() != t.guider)
            {
                *last_guider.borrow_mut() = train.guider.clone();
                guide_sig.update(|opt| {
                    opt.get_or_insert_with(GuideStatusData::default).device = train.guider.clone();
                });
            }
        });
    }

    // Long-lived refresh loop — keeps INDI properties current after bootstrap.
    spawn_refresh_loop(send_fn.clone(), store.clone());
    // Dedicated fast loop for mount RA/Dec freshness + self-healing re-subscribe.
    spawn_mount_coord_loop(send_fn.clone(), store.clone());

    let store_for_ws = store.clone();
    spawn_local(async move {
        let ws = match WebSocket::open(&ws_url) {
            Ok(ws) => ws,
            Err(e) => {
                leptos::logging::error!("WS open failed: {:?}", e);
                return;
            }
        };
        let (mut writer, mut reader) = ws.split();

        spawn_local(async move {
            while let Some(msg) = cmd_rx.next().await {
                if writer.send(Message::Text(msg)).await.is_err() { break; }
            }
        });

        while let Some(msg) = reader.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        let type_str = val["type"].as_str().unwrap_or("").to_string();
                        let payload  = val["payload"].clone();
                        leptos::logging::log!("[ws] recv type={}", type_str);
                        store_for_ws.apply_ekos_event(&type_str, &payload);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    leptos::logging::error!("WS error: {:?}", e);
                    store_for_ws.connected.set(false);
                    store_for_ws.online.set(false);
                    break;
                }
            }
        }
    });

    (store, send_fn)
}
