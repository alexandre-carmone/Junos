//! `DeviceStore` — Leptos signal bag plus the `apply_ekos_event` dispatch
//! that decodes Ekos Live wire messages into store updates.

use leptos::prelude::*;

use super::types::*;
use crate::ws_helpers::extract_indi_number;

#[derive(Clone)]
pub struct DeviceStore {
    pub connected: RwSignal<bool>,
    /// Ekos::Success, i.e. a profile is running and requests that gate on it
    /// will actually be handled. Driven by `new_connection_state.online`.
    pub online: RwSignal<bool>,
    /// Server's $HOME, injected by junos-server/proxy.rs on connect.
    /// Used to predict .esq file paths written via scheduler_save_sequence_file.
    pub home_dir: RwSignal<String>,
    pub mount_status: RwSignal<Option<MountStatusData>>,
    pub camera_status: RwSignal<Option<CameraStatusData>>,
    pub filter_wheel_status: RwSignal<Option<FilterWheelStatusData>>,
    pub telescope_settings: RwSignal<TelescopeSettingsData>,
    pub optical_trains: RwSignal<Vec<OpticalTrain>>,
    pub scopes: RwSignal<Vec<ScopeInfo>>,
    /// All connected INDI devices, from `get_devices`. Used by the rig editor
    /// to populate per-role device dropdowns (filtered by interface bitfield).
    pub devices: RwSignal<Vec<DeviceInfo>>,
    /// Per-module optical-train assignment, from `train_get_profiles`. Raw map
    /// `{ "<ProfileSettings enum>": <trainId>, … }` (0=Primary 1=Capture
    /// 2=Focus 3=Mount 4=Guide 5=Align 6=DarkLibrary).
    pub module_trains: RwSignal<serde_json::Value>,
    pub focus_status: RwSignal<Option<FocusStatusData>>,
    pub focus_settings: RwSignal<serde_json::Value>,
    pub focus_preview_url: RwSignal<Option<String>>,
    pub focus_hfr_history: RwSignal<Vec<HfrSample>>,
    pub capture_status: RwSignal<CaptureStatusData>,
    pub capture_settings: RwSignal<serde_json::Value>,
    pub capture_sequence: RwSignal<serde_json::Value>,
    pub capture_preview_url: RwSignal<Option<String>>,
    pub polar_state: RwSignal<PolarStateData>,
    pub align_settings: RwSignal<serde_json::Value>,
    pub align_solution: RwSignal<AlignSolutionData>,
    pub align_preview_url: RwSignal<Option<String>>,
    pub guide_status: RwSignal<Option<GuideStatusData>>,
    pub guide_settings: RwSignal<serde_json::Value>,
    /// Flattened `{name: value, ...}` map of global KStars `Options::`
    /// entries we care about (GuiderType, PHD2Host/Port, LinGuiderHost/Port).
    pub guide_options: RwSignal<serde_json::Value>,
    pub guide_preview_url: RwSignal<Option<String>>,
    pub scheduler_status: RwSignal<SchedulerStatusData>,
    pub scheduler_settings: RwSignal<serde_json::Value>,
    pub scheduler_jobs: RwSignal<Vec<serde_json::Value>>,
    pub mosaic_tiles: RwSignal<Option<serde_json::Value>>,
    pub livestacker_state: RwSignal<Option<LiveStackerState>>,
    pub livestacker_settings: RwSignal<serde_json::Value>,
    /// Dust cap / flat panel status. Driven by `new_cap_state` when KStars
    /// emits it (`updateCapStatus` declared at message.cpp:2597) and by
    /// INDI `CAP_PARK` / `FLAT_LIGHT_CONTROL` / `FLAT_LIGHT_INTENSITY`
    /// pushes for first-mount and continuous updates.
    pub dustcap_status: RwSignal<Option<DustCapStatusData>>,
    /// Equipment profile list, populated by `get_profiles` responses.
    /// Available before `online == true` — profile CRUD is dispatched
    /// before the Ekos-startup gate (message.cpp:249).
    pub profiles: RwSignal<Vec<ProfileInfo>>,
    /// Name of the currently-selected profile in KStars (`selectedProfile`).
    pub selected_profile: RwSignal<Option<String>>,
    /// Installed INDI drivers reported by `get_drivers`. Available before
    /// `online == true` (same gate as `get_profiles`).
    pub drivers: RwSignal<Vec<DriverInfo>>,
    /// Whether KStars process is running on the server host.
    /// Updated by `app_state` messages pushed from junos-server.
    pub kstars_running: RwSignal<bool>,
    /// Whether PHD2 process is running on the server host.
    /// Updated by `app_state` messages pushed from junos-server.
    pub phd2_running: RwSignal<bool>,
    /// Set true on the first `app_state` message — used by the app shell
    /// to make a one-shot startup decision (e.g. default to Profiles tab
    /// when KStars isn't running).
    pub kstars_state_known: RwSignal<bool>,
    /// Active modal dialog pushed by KStars (`dialog_get_info`). Holds the
    /// raw payload `{title, message, icon, timeout, buttons:[..], default}`
    /// while a dialog is open; `None` when the dialog is dismissed (KStars
    /// sends an empty `{}` payload on close). The web responds via
    /// `dialog_get_response {button: "<label>"}` to unblock KStars.
    pub active_dialog: RwSignal<Option<serde_json::Value>>,
}

impl DeviceStore {
    /// Resolve the optical train assigned to an Ekos module by its
    /// `ProfileSettings` enum key ("1"=Capture, "2"=Focus, "3"=Mount,
    /// "4"=Guide, "5"=Align), reading the per-module map from
    /// `module_trains` (`train_get_profiles`). Falls back to the first train
    /// when the module has no explicit assignment yet. Untracked read — for
    /// use in the non-reactive timer loops in `retry.rs`.
    pub(super) fn module_train_untracked(&self, key: &str) -> Option<OpticalTrain> {
        let trains = self.optical_trains.get_untracked();
        let id = self
            .module_trains
            .with_untracked(|m| m.get(key).and_then(|v| v.as_i64()));
        id.and_then(|id| trains.iter().find(|t| t.id == id).cloned())
            .or_else(|| trains.first().cloned())
    }

    pub(super) fn new() -> Self {
        Self {
            connected: RwSignal::new(false),
            online: RwSignal::new(false),
            home_dir: RwSignal::new(String::new()),
            mount_status: RwSignal::new(None),
            camera_status: RwSignal::new(None),
            filter_wheel_status: RwSignal::new(None),
            telescope_settings: RwSignal::new(TelescopeSettingsData::default()),
            optical_trains: RwSignal::new(Vec::new()),
            scopes: RwSignal::new(Vec::new()),
            devices: RwSignal::new(Vec::new()),
            module_trains: RwSignal::new(serde_json::Value::Null),
            focus_status: RwSignal::new(None),
            focus_settings: RwSignal::new(serde_json::Value::Null),
            focus_preview_url: RwSignal::new(None),
            focus_hfr_history: RwSignal::new(Vec::new()),
            capture_status: RwSignal::new(CaptureStatusData::default()),
            capture_settings: RwSignal::new(serde_json::Value::Null),
            capture_sequence: RwSignal::new(serde_json::Value::Null),
            capture_preview_url: RwSignal::new(None),
            polar_state: RwSignal::new(PolarStateData::default()),
            align_settings: RwSignal::new(serde_json::Value::Null),
            align_solution: RwSignal::new(AlignSolutionData::default()),
            align_preview_url: RwSignal::new(None),
            guide_status: RwSignal::new(None),
            guide_settings: RwSignal::new(serde_json::Value::Null),
            guide_options: RwSignal::new(serde_json::Value::Null),
            guide_preview_url: RwSignal::new(None),
            scheduler_status: RwSignal::new(SchedulerStatusData::default()),
            scheduler_settings: RwSignal::new(serde_json::Value::Null),
            scheduler_jobs: RwSignal::new(Vec::new()),
            mosaic_tiles: RwSignal::new(None),
            livestacker_state: RwSignal::new(None),
            livestacker_settings: RwSignal::new(serde_json::Value::Null),
            dustcap_status: RwSignal::new(None),
            profiles: RwSignal::new(Vec::new()),
            selected_profile: RwSignal::new(None),
            drivers: RwSignal::new(Vec::new()),
            kstars_running: RwSignal::new(false),
            phd2_running: RwSignal::new(false),
            kstars_state_known: RwSignal::new(false),
            active_dialog: RwSignal::new(None),
        }
    }

    pub(super) fn apply_ekos_event(&self, type_str: &str, payload: &serde_json::Value) {
        match type_str {
            "file_default_path" => {
                if let Some(s) = payload.as_str() {
                    if !s.is_empty() {
                        self.home_dir.set(s.to_string());
                    }
                }
            }

            "new_connection_state" => {
                let connected = payload["connected"].as_bool().unwrap_or(false);
                let online = payload["online"].as_bool().unwrap_or(false);
                self.connected.set(connected);
                self.online.set(connected && online);
                if let Some(h) = payload["home_dir"].as_str() {
                    if !h.is_empty() {
                        self.home_dir.set(h.to_string());
                    }
                }
                if !connected {
                    self.mount_status.set(None);
                    self.camera_status.set(None);
                    self.filter_wheel_status.set(None);
                    self.focus_status.set(None);
                    self.focus_preview_url.set(None);
                    self.focus_hfr_history.set(Vec::new());
                    self.polar_state.set(PolarStateData::default());
                    self.align_settings.set(serde_json::Value::Null);
                    self.align_solution.set(AlignSolutionData::default());
                    self.align_preview_url.set(None);
                    self.guide_status.set(None);
                    self.guide_preview_url.set(None);
                    // guide_settings / guide_options left intact so the
                    // Guide tab doesn't flicker between blank and populated
                    // on transient disconnects.
                    self.scheduler_status.set(SchedulerStatusData::default());
                    self.scheduler_jobs.set(Vec::new());
                    self.mosaic_tiles.set(None);
                    self.livestacker_state.set(None);
                    self.dustcap_status.set(None);
                }
            }

            // KStars device inventory (message.cpp:342). Payload is an array
            // of `{name, connected, version, interface}` — `interface` is an
            // INDI driver-interface bitfield (libindi/basedevice.h). Bits
            // we care about here:
            //   DUSTCAP_INTERFACE  = 1 << 9  = 512
            //   LIGHTBOX_INTERFACE = 1 << 10 = 1024
            // Many flat-panel drivers (Flip Flat, Alnitak) advertise both
            // bits on a single device.
            "get_devices" => {
                const DUSTCAP_BIT:  i64 = 1 << 9;
                const LIGHTBOX_BIT: i64 = 1 << 10;
                if let Some(arr) = payload.as_array() {
                    // Retain the full device list for the rig (optical-train)
                    // editor's per-role dropdowns.
                    self.devices
                        .set(arr.iter().map(DeviceInfo::from_json).collect());
                    let cap_dev = arr.iter().find(|d| {
                        let iface = d["interface"].as_i64().unwrap_or(0);
                        iface & (DUSTCAP_BIT | LIGHTBOX_BIT) != 0
                    });
                    if let Some(dev) = cap_dev {
                        let name = dev["name"].as_str().unwrap_or("").to_string();
                        let connected = dev["connected"].as_bool().unwrap_or(false);
                        let iface = dev["interface"].as_i64().unwrap_or(0);
                        let has_light = iface & LIGHTBOX_BIT != 0;
                        self.dustcap_status.update(|opt| {
                            let s = opt.get_or_insert_with(DustCapStatusData::default);
                            if !name.is_empty() { s.device = name; }
                            s.connected = connected;
                            s.has_light_panel = has_light;
                        });
                    }
                }
            }

            // Direct cap-state push from KStars (`updateCapStatus` declared
            // at message.cpp:2597). Currently no call site, but if it
            // becomes wired the payload is a free-form QJsonObject — we
            // accept the same field set we synthesise from INDI props.
            "new_cap_state" => {
                self.dustcap_status.update(|opt| {
                    let s = opt.get_or_insert_with(DustCapStatusData::default);
                    if let Some(v) = payload["device"].as_str() {
                        if !v.is_empty() { s.device = v.to_string(); }
                    }
                    if let Some(v) = payload["connected"].as_bool() { s.connected = v; }
                    if let Some(v) = payload["lightOn"].as_bool() { s.light_on = Some(v); }
                    if let Some(v) = payload["brightness"].as_f64() { s.brightness = Some(v); }
                    if let Some(v) = payload["parkStatus"].as_i64() {
                        s.park_state = match v {
                            0 => DustCapParkState::Parked,
                            1 => DustCapParkState::Unparked,
                            _ => DustCapParkState::Unknown,
                        };
                    }
                });
            }

            "mount_get_all_settings" => {
                // Reply to our `mount_get_all_settings` request, also sent
                // (debounced) after a `mount_set_all_settings`. Payload is the
                // full Mount widget map; we only care about the meridian-flip
                // pair. Cf. message.cpp:604-608 (sendMountSettings).
                self.mount_status.update(|opt| {
                    let ms = opt.get_or_insert_with(MountStatusData::default);
                    if let Some(b) = payload["executeMeridianFlip"].as_bool() {
                        ms.meridian_flip_enabled = Some(b);
                    }
                    if let Some(v) = payload["meridianFlipOffsetDegrees"].as_f64() {
                        ms.meridian_flip_offset_deg = Some(v);
                    }
                });
            }

            "new_mount_state" => {
                self.mount_status.update(|opt| {
                    let ms = opt.get_or_insert_with(MountStatusData::default);
                    if let Some(dev) = payload["device"].as_str() {
                        if !dev.is_empty() {
                            ms.device = dev.to_string();
                        }
                    }
                    if let Some(status) = payload["status"].as_str() {
                        let sl = status.to_lowercase();
                        ms.slewing = sl.contains("slewing");
                        ms.tracking = sl.contains("tracking");
                        ms.parked = sl.contains("park");
                        ms.connected = true;
                    }
                    // KStars sends RA and Dec in degrees.
                    if let Some(ra_deg) = payload["ra"].as_f64() {
                        ms.ra_h = Some(ra_deg / 15.0);
                    }
                    if let Some(dec) = payload["de"].as_f64() {
                        ms.dec_deg = Some(dec);
                    }
                    // HA in degrees (manager.cpp:3189). Sent with the coord
                    // payload and throttled to 1 s (message.cpp:2552).
                    if let Some(ha) = payload["ha"].as_f64() {
                        ms.ha_deg = Some(ha);
                    }
                    // Pier side: -1/0/1 per kstars/indi/indimount.h:39.
                    // Emitted standalone on pierSideChanged (manager.cpp:2698).
                    if let Some(p) = payload["pierSide"].as_i64() {
                        ms.pier_side = Some(p as i32);
                    }
                    // Az/Alt and J2000 coords come with the throttled coord payload.
                    if let Some(v) = payload["az"].as_f64() {
                        ms.az_deg = Some(v);
                    }
                    if let Some(v) = payload["at"].as_f64() {
                        ms.alt_deg = Some(v);
                    }
                    if let Some(v) = payload["ra0"].as_f64() {
                        ms.ra0_h = Some(v / 15.0);
                    }
                    if let Some(v) = payload["de0"].as_f64() {
                        ms.dec0_deg = Some(v);
                    }
                    // Slew rate index, target name, and info banners.
                    if let Some(v) = payload["slewRate"].as_i64() {
                        ms.slew_rate = Some(v as i32);
                    }
                    if let Some(v) = payload["target"].as_str() {
                        if !v.is_empty() {
                            ms.target = v.to_string();
                        }
                    }
                    if let Some(v) = payload["meridianFlipStatus"].as_str() {
                        ms.meridian_flip_status = v.to_string();
                    }
                    if let Some(v) = payload["autoParkCountdown"].as_str() {
                        ms.auto_park_countdown = v.to_string();
                    }
                    if let Some(s) = payload["status"].as_str() {
                        ms.status_str = s.to_string();
                    }
                });
            }

            "get_profiles" => {
                // Equipment profile list. Wire shape from message.cpp:1322-1339:
                //   { selectedProfile: "<name>", profiles: [ProfileInfo.toJson, …] }
                if let Some(arr) = payload["profiles"].as_array() {
                    let list: Vec<ProfileInfo> = arr.iter().map(ProfileInfo::from_json).collect();
                    self.profiles.set(list);
                }
                if let Some(s) = payload["selectedProfile"].as_str() {
                    self.selected_profile.set(if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    });
                }
            }

            "get_drivers" => {
                // INDI driver list. Wire shape: payload is an array of
                // {name, label, binary, version, manufacturer, skel, family}
                // (driverinfo.h:57). We keep only the fields the UI needs.
                if let Some(arr) = payload.as_array() {
                    let list: Vec<DriverInfo> = arr.iter().map(DriverInfo::from_json).collect();
                    self.drivers.set(list);
                }
            }

            "get_scopes" => {
                // OAL scope DB — full list, not just the active one.
                // Shape: [{ id, model, vendor, type, name, focal_length, aperture }].
                if let Some(arr) = payload.as_array() {
                    let scopes: Vec<ScopeInfo> = arr.iter().map(ScopeInfo::from_json).collect();
                    self.scopes.set(scopes);
                }
            }

            "train_get_all" => {
                if let Some(arr) = payload.as_array() {
                    let trains: Vec<OpticalTrain> =
                        arr.iter().map(OpticalTrain::from_json).collect();
                    for t in &trains {
                        leptos::logging::log!(
                            "[ws] train: name={:?} mount={:?} scope={:?} camera={:?}",
                            t.name,
                            t.mount,
                            t.scope,
                            t.camera
                        );
                    }
                    // Per-module device names (camera, filterwheel, mount,
                    // focuser, guide camera) are seeded from each module's
                    // assigned train by the bootstrap effect in `ws/mod.rs`,
                    // which resolves `module_trains` → device. Don't seed from
                    // `trains.first()` here — that ignored the assignment.
                    self.optical_trains.set(trains);
                }
            }

            // Per-module train assignment (message.cpp:381). Map keyed by the
            // ProfileSettings enum value as a string ("0".."6"), value = train
            // id. The rig tab resolves ids to names via `optical_trains`.
            "train_get_profiles" => {
                self.module_trains.set(payload.clone());
            }

            // INDI property reply — either a direct get response, or a
            // pushed update from a prior device_property_subscribe.
            // We only consume two properties: CCD_INFO (camera sensor specs)
            // and EQUATORIAL_EOD_COORD (mount RA/Dec fast path).
            "device_property_get" | "device_property_set" => {
                let prop = payload["name"].as_str().unwrap_or("");
                leptos::logging::log!(
                    "[ws] recv {} device={} prop={}",
                    type_str,
                    payload["device"].as_str().unwrap_or("?"),
                    prop
                );

                if prop == "CCD_INFO" {
                    let max_x = extract_indi_number(payload, "CCD_MAX_X");
                    let max_y = extract_indi_number(payload, "CCD_MAX_Y");
                    let pix_x = extract_indi_number(payload, "CCD_PIXEL_SIZE_X");
                    let pix_y = extract_indi_number(payload, "CCD_PIXEL_SIZE_Y");
                    let pix_any = extract_indi_number(payload, "CCD_PIXEL_SIZE");
                    let pix = pix_x.or(pix_y).or(pix_any);
                    let sw = max_x.map(|v| v as u32);
                    let sh = max_y.map(|v| v as u32);
                    if pix.is_some() || sw.is_some() || sh.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            if pix.is_some() {
                                cs.pixel_size_um = pix;
                            }
                            if sw.is_some() {
                                cs.sensor_width = sw;
                            }
                            if sh.is_some() {
                                cs.sensor_height = sh;
                            }
                        });
                    }
                } else if prop == "ABS_FOCUS_POSITION" {
                    let pos =
                        extract_indi_number(payload, "FOCUS_ABSOLUTE_POSITION").map(|v| v as i64);
                    if pos.is_some() {
                        self.focus_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FocusStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    fs.device = dev.to_string();
                                }
                            }
                            if pos.is_some() {
                                fs.position = pos;
                            }
                        });
                    }
                } else if prop == "FOCUS_TEMPERATURE" {
                    let temp = extract_indi_number(payload, "TEMPERATURE");
                    if temp.is_some() {
                        self.focus_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FocusStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    fs.device = dev.to_string();
                                }
                            }
                            fs.temperature = temp;
                        });
                    }
                } else if prop == "CCD_TEMPERATURE" {
                    let t = extract_indi_number(payload, "CCD_TEMPERATURE_VALUE");
                    if t.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    cs.device = dev.to_string();
                                }
                            }
                            cs.temperature = t;
                        });
                    }
                } else if prop == "CCD_COOLER" {
                    let mut on: Option<bool> = None;
                    if let Some(arr) = payload["switches"].as_array() {
                        for el in arr {
                            let n = el["name"].as_str().unwrap_or("");
                            let v = el["value"]
                                .as_bool()
                                .or_else(|| el["state"].as_str().map(|s| s == "On"));
                            if n == "COOLER_ON" {
                                on = v;
                            }
                        }
                    }
                    if on.is_some() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            cs.cooler_on = on;
                        });
                    }
                } else if prop == "CCD_CAPTURE_FORMAT"
                    || prop == "CCD_TRANSFER_FORMAT"
                    || prop == "CCD_ISO"
                    || prop == "CCD_FRAME_TYPE"
                {
                    // Switch property — option list comes from element labels
                    // (compact:false). See indicamera.cpp:141 for the canonical
                    // example: m_CaptureFormats is built from getLabel().
                    let mut labels: Vec<String> = Vec::new();
                    if let Some(arr) = payload["switches"].as_array() {
                        for el in arr {
                            if let Some(lbl) = el["label"].as_str() {
                                if !lbl.is_empty() {
                                    labels.push(lbl.to_string());
                                    continue;
                                }
                            }
                            // Fallback: switch name (compact-mode payload).
                            if let Some(n) = el["name"].as_str() {
                                if !n.is_empty() {
                                    labels.push(n.to_string());
                                }
                            }
                        }
                    }
                    if !labels.is_empty() {
                        self.camera_status.update(|opt| {
                            let cs = opt.get_or_insert_with(CameraStatusData::default);
                            match prop {
                                "CCD_CAPTURE_FORMAT" => cs.capture_format_options = labels,
                                "CCD_TRANSFER_FORMAT" => cs.transfer_format_options = labels,
                                "CCD_ISO" => cs.iso_options = labels,
                                "CCD_FRAME_TYPE" => cs.frame_type_options = labels,
                                _ => {}
                            }
                        });
                    }
                } else if prop == "FILTER_NAME" {
                    // Text property — `texts:[{name, text}, …]`. The `text`
                    // field is the user-visible filter label (e.g. "Ha", "OIII").
                    let mut names: Vec<String> = Vec::new();
                    if let Some(arr) = payload["texts"].as_array() {
                        for el in arr {
                            if let Some(s) = el["text"].as_str() {
                                names.push(s.to_string());
                            }
                        }
                    }
                    if !names.is_empty() {
                        self.filter_wheel_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FilterWheelStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    fs.device = dev.to_string();
                                }
                            }
                            fs.filter_names = names;
                        });
                    }
                } else if prop == "FILTER_SLOT" {
                    let slot = extract_indi_number(payload, "FILTER_SLOT_VALUE").map(|v| v as i32);
                    if slot.is_some() {
                        self.filter_wheel_status.update(|opt| {
                            let fs = opt.get_or_insert_with(FilterWheelStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    fs.device = dev.to_string();
                                }
                            }
                            fs.current_slot = slot;
                        });
                    }
                } else if prop == "CAP_PARK" {
                    // Switch property — element states tell us park vs unpark.
                    // INDI also surfaces "Busy" while the cap is in motion;
                    // we map that to `Moving`. Element names per
                    // kstars/indi/indidustcap.cpp:34.
                    let state_busy = payload["state"].as_str() == Some("Busy");
                    let mut park_on: Option<bool> = None;
                    let mut unpark_on: Option<bool> = None;
                    if let Some(arr) = payload["switches"].as_array() {
                        for el in arr {
                            let n = el["name"].as_str().unwrap_or("");
                            let v = el["value"].as_bool()
                                .or_else(|| el["state"].as_str().map(|s| s == "On"));
                            if n == "PARK" { park_on = v; }
                            else if n == "UNPARK" { unpark_on = v; }
                        }
                    }
                    let park_state = if state_busy {
                        DustCapParkState::Moving
                    } else {
                        match (park_on, unpark_on) {
                            (Some(true), _) => DustCapParkState::Parked,
                            (_, Some(true)) => DustCapParkState::Unparked,
                            _ => DustCapParkState::Unknown,
                        }
                    };
                    self.dustcap_status.update(|opt| {
                        let s = opt.get_or_insert_with(DustCapStatusData::default);
                        if let Some(dev) = payload["device"].as_str() {
                            if !dev.is_empty() { s.device = dev.to_string(); }
                        }
                        s.park_state = park_state;
                    });
                } else if prop == "FLAT_LIGHT_CONTROL" {
                    // Light on/off switch. Element name per
                    // kstars/indi/indilightbox.cpp:47.
                    let mut light_on: Option<bool> = None;
                    if let Some(arr) = payload["switches"].as_array() {
                        for el in arr {
                            if el["name"].as_str() == Some("FLAT_LIGHT_ON") {
                                light_on = el["value"].as_bool()
                                    .or_else(|| el["state"].as_str().map(|s| s == "On"));
                            }
                        }
                    }
                    if light_on.is_some() {
                        self.dustcap_status.update(|opt| {
                            let s = opt.get_or_insert_with(DustCapStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() { s.device = dev.to_string(); }
                            }
                            s.has_light_panel = true;
                            s.light_on = light_on;
                        });
                    }
                } else if prop == "FLAT_LIGHT_INTENSITY" {
                    // Number property — value + min/max per
                    // kstars/indi/indilightbox.cpp:63,114.
                    let mut brightness: Option<f64> = None;
                    let mut bmin: Option<f64> = None;
                    let mut bmax: Option<f64> = None;
                    if let Some(arr) = payload["numbers"].as_array() {
                        for el in arr {
                            // Different drivers name the element differently
                            // (FLAT_LIGHT_INTENSITY_VALUE vs FLAT_INTENSITY).
                            // The lightbox usually exposes a single number,
                            // so just take the first one we see.
                            brightness = el["value"].as_f64().or(brightness);
                            bmin = el["min"].as_f64().or(bmin);
                            bmax = el["max"].as_f64().or(bmax);
                            if brightness.is_some() { break; }
                        }
                    }
                    self.dustcap_status.update(|opt| {
                        let s = opt.get_or_insert_with(DustCapStatusData::default);
                        if let Some(dev) = payload["device"].as_str() {
                            if !dev.is_empty() { s.device = dev.to_string(); }
                        }
                        s.has_light_panel = true;
                        if brightness.is_some() { s.brightness = brightness; }
                        if bmin.is_some() { s.brightness_min = bmin; }
                        if bmax.is_some() { s.brightness_max = bmax; }
                    });
                } else if prop == "EQUATORIAL_EOD_COORD" {
                    // INDI mount coord property: RA in hours, DEC in degrees.
                    let ra_h = extract_indi_number(payload, "RA");
                    let de_d = extract_indi_number(payload, "DEC");
                    if ra_h.is_some() || de_d.is_some() {
                        self.mount_status.update(|opt| {
                            let ms = opt.get_or_insert_with(MountStatusData::default);
                            if let Some(dev) = payload["device"].as_str() {
                                if !dev.is_empty() {
                                    ms.device = dev.to_string();
                                }
                            }
                            if ra_h.is_some() {
                                ms.ra_h = ra_h;
                            }
                            if de_d.is_some() {
                                ms.dec_deg = de_d;
                            }
                        });
                    }
                }
            }

            // Focus module state. Partial payloads: any subset of
            // {status, hfr, pos, log, focusAdvisorMessage, focusAdvisorStage,
            //  focusinitHFRPlot, title}. See kstars manager.cpp:2530+ for the
            // emission sites.
            "new_focus_state" => {
                let hfr = payload["hfr"].as_f64();
                let pos = payload["pos"]
                    .as_i64()
                    .or_else(|| payload["pos"].as_f64().map(|v| v as i64));
                self.focus_status.update(|opt| {
                    let fs = opt.get_or_insert_with(FocusStatusData::default);
                    fs.connected = true;
                    if let Some(s) = payload["status"].as_str() {
                        fs.status = s.to_string();
                    }
                    if let Some(h) = hfr {
                        fs.hfr = Some(h);
                    }
                    if let Some(p) = pos {
                        fs.position = Some(p);
                    }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() {
                            fs.log = l.to_string();
                        }
                    }
                });
                if let Some(h) = hfr {
                    if h > 0.0 && h.is_finite() {
                        let t_ms = web_sys::js_sys::Date::now();
                        self.focus_hfr_history.update(|v| {
                            v.push(HfrSample {
                                t_ms,
                                hfr: h,
                                position: pos,
                            });
                            if v.len() > 200 {
                                let drop = v.len() - 200;
                                v.drain(..drop);
                            }
                        });
                    }
                }
            }

            // Debounced settings snapshot reply (see message.cpp:734).
            "focus_get_all_settings" => {
                self.focus_settings.set(payload.clone());
            }

            // Camera state: temperature (see message.cpp:446 sendTemperature).
            // Payload is {name, temperature}.
            "new_camera_state" => {
                self.camera_status.update(|opt| {
                    let cs = opt.get_or_insert_with(CameraStatusData::default);
                    if let Some(dev) = payload["name"].as_str() {
                        if !dev.is_empty() {
                            cs.device = dev.to_string();
                        }
                    }
                    if let Some(t) = payload["temperature"].as_f64() {
                        cs.temperature = Some(t);
                    }
                });
            }

            // Capture module status (message.cpp:2567). Partial payloads.
            "new_capture_state" => {
                self.capture_status.update(|c| {
                    if let Some(s) = payload["status"].as_str() {
                        c.status = s.to_string();
                    }
                    if let Some(t) = payload["target"].as_str() {
                        c.target = t.to_string();
                    }
                    if let Some(v) = payload["seqr"].as_i64() {
                        c.seq_total = Some(v);
                    }
                    if let Some(v) = payload["seqv"].as_i64() {
                        c.seq_current = Some(v);
                    }
                    if let Some(v) = payload["ovp"].as_f64() {
                        c.progress = Some(v);
                    }
                    if let Some(s) = payload["seqt"].as_str() {
                        c.seq_remaining_time = s.to_string();
                    }
                    if let Some(s) = payload["ovt"].as_str() {
                        c.overall_remaining_time = s.to_string();
                    }
                    if let Some(v) = payload["expv"].as_f64() {
                        c.exposure_left = Some(v);
                    }
                    if let Some(v) = payload["expr"].as_f64() {
                        c.exposure_total = Some(v);
                    }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() {
                            c.log = l.to_string();
                        }
                    }
                });
            }

            "capture_get_all_settings" => {
                self.capture_settings.set(payload.clone());
            }

            "capture_get_sequences" => {
                self.capture_sequence.set(payload.clone());
            }

            // Polar alignment state (PAA). Partial payloads: any subset of
            // {stage, message, enabled, vector, updatedError*}. See
            // kstars/ekos/ekoslive/message.cpp:1157-1263.
            "new_polar_state" => {
                self.polar_state.update(|p| {
                    if let Some(s) = payload["stage"].as_str() {
                        p.stage = s.to_string();
                    }
                    if let Some(m) = payload["message"].as_str() {
                        p.message = m.to_string();
                    }
                    if let Some(e) = payload["enabled"].as_bool() {
                        p.enabled = e;
                    }
                    if let Some(obj) = payload.get("vector").and_then(|v| v.as_object()) {
                        p.vector = Some(PolarVectorData {
                            center_x: obj.get("center_x").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            center_y: obj.get("center_y").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            mag: obj.get("mag").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            pa: obj.get("pa").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            error: obj.get("error").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            az_error: obj.get("azError").and_then(|x| x.as_f64()).unwrap_or(0.0),
                            alt_error: obj.get("altError").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        });
                    }
                    if let Some(v) = payload.get("updatedError").and_then(|x| x.as_f64()) {
                        p.updated_error = Some(v);
                    }
                    if let Some(v) = payload.get("updatedAZError").and_then(|x| x.as_f64()) {
                        p.updated_az_error = Some(v);
                    }
                    if let Some(v) = payload.get("updatedALTError").and_then(|x| x.as_f64()) {
                        p.updated_alt_error = Some(v);
                    }
                });
            }

            // Align module settings snapshot (reply to align_get_all_settings
            // and echoed after align_set_all_settings). See message.cpp:576-580.
            // Payload is the settings map directly.
            "align_get_all_settings" => {
                self.align_settings.set(payload.clone());
            }

            // Align state. KStars emits this from two places:
            //   - setAlignStatus  → {status}
            //   - setAlignSolution → {solution: {ra.Hours, de.Degrees, PA, pix, fov, ...}}
            // (see message.cpp:927, 943; align.cpp:2351 for the solution map).
            // RA/Dec in the solution are JNow.
            "new_align_state" => {
                if let Some(s) = payload.get("status").and_then(|v| v.as_str()) {
                    let s = s.to_string();
                    self.align_solution.update(|a| {
                        a.status = Some(s);
                    });
                }
                if let Some(sol) = payload.get("solution").and_then(|v| v.as_object()) {
                    leptos::logging::log!(
                        "[ws] new_align_state solution: {}",
                        serde_json::to_string(sol).unwrap_or_default()
                    );
                    let ra_h = sol.get("ra.Hours").and_then(|x| x.as_f64());
                    let de_d = sol.get("de.Degrees").and_then(|x| x.as_f64());
                    let pa = sol.get("PA").and_then(|x| x.as_f64());
                    let pix = sol.get("pix").and_then(|x| x.as_f64());
                    if ra_h.is_some() || de_d.is_some() || pa.is_some() || pix.is_some() {
                        self.align_solution.update(|a| {
                            if let Some(v) = ra_h {
                                a.ra_jnow_deg = Some(v * 15.0);
                            }
                            if let Some(v) = de_d {
                                a.dec_jnow_deg = Some(v);
                            }
                            if let Some(v) = pa {
                                a.orientation_deg = Some(v);
                            }
                            if let Some(v) = pix {
                                a.pixscale_arcsec = Some(v);
                            }
                            a.solved_at_ms = Some(web_sys::js_sys::Date::now());
                        });
                    }
                }
            }

            // Guide module status. KStars emits `new_guide_state` from
            // multiple sites (see manager.cpp:2769-2786) with **partial
            // payloads**. Only `setStatus`/`updateGuideStatus` include a
            // `status` field; the other emissions carry just {rarms,derms},
            // {drift_ra,drift_de}, or {log}. So we must treat every field
            // as optional and never reset state from a missing field.
            "new_guide_state" => {
                self.guide_status.update(|opt| {
                    let gs = opt.get_or_insert_with(GuideStatusData::default);
                    gs.connected = true;

                    if let Some(status) = payload["status"].as_str() {
                        if gs.status != status {
                            let t_ms = web_sys::js_sys::Date::now();
                            gs.history.push(GuideStateSample {
                                t_ms,
                                status: status.to_string(),
                            });
                            if gs.history.len() > 256 {
                                let drop = gs.history.len() - 256;
                                gs.history.drain(..drop);
                            }
                            gs.status = status.to_string();
                        }
                    }

                    // Drift + RMS samples — pushed per-frame from
                    // manager.cpp:2664 (updateSigmas) and the
                    // newAxisDelta lambda at :2772-2776.
                    let drift_ra = payload["drift_ra"].as_f64();
                    let drift_de = payload["drift_de"].as_f64();
                    if drift_ra.is_some() || drift_de.is_some() {
                        let t_ms = web_sys::js_sys::Date::now();
                        gs.drift.push(GuideDriftSample {
                            t_ms,
                            ra: drift_ra.unwrap_or(f64::NAN),
                            de: drift_de.unwrap_or(f64::NAN),
                        });
                        if gs.drift.len() > 600 {
                            let drop = gs.drift.len() - 600;
                            gs.drift.drain(..drop);
                        }
                    }
                    if let Some(v) = payload["rarms"].as_f64() {
                        gs.ra_rms = Some(v);
                    }
                    if let Some(v) = payload["derms"].as_f64() {
                        gs.de_rms = Some(v);
                    }

                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() {
                            gs.log = l.to_string();
                        }
                    }
                });
            }

            // Guide settings snapshot — debounced reply to
            // guide_get_all_settings (message.cpp:585-590). Flat widget map.
            "guide_get_all_settings" => {
                self.guide_settings.set(payload.clone());
            }

            // Reply to option_get — array of {name, value}. Walk it and
            // cache any guide-relevant keys. Ignore other keys so we don't
            // stomp on future consumers that request their own options.
            "option_get" => {
                const GUIDE_KEYS: &[&str] = &[
                    "GuiderType",
                    "PHD2Host",
                    "PHD2Port",
                    "LinGuiderHost",
                    "LinGuiderPort",
                ];
                if let Some(arr) = payload.as_array() {
                    self.guide_options.update(|opt| {
                        let map = match opt {
                            serde_json::Value::Object(m) => m,
                            _ => {
                                *opt = serde_json::Value::Object(serde_json::Map::new());
                                opt.as_object_mut().unwrap()
                            }
                        };
                        for el in arr {
                            let Some(name) = el["name"].as_str() else {
                                continue;
                            };
                            if !GUIDE_KEYS.contains(&name) {
                                continue;
                            }
                            if let Some(v) = el.get("value") {
                                map.insert(name.to_string(), v.clone());
                            }
                        }
                    });
                }
            }

            // Binary media frames come through as JSON after server-side
            // decoding (junos-server/src/kstars_ws.rs:169-198). Metadata is the
            // parsed header; we only consume focus frames (uuid starts with
            // "+F", see kstars media.cpp:752).
            "new_preview_image" => {
                let uuid = payload["metadata"]["uuid"].as_str().unwrap_or("");
                if let Some(b64) = payload["data"].as_str() {
                    let ext = payload["metadata"]["ext"].as_str().unwrap_or("jpg");
                    let mime = if ext == "jpg" {
                        "image/jpeg"
                    } else {
                        "image/png"
                    };
                    let url = format!("data:{};base64,{}", mime, b64);
                    // Frame uuid prefixes come from kstars/ekos/ekoslive/media.cpp:
                    //   "+F" focus (line 752)
                    //   "+A" align / polar align (line 587, 640 — sendUpdatedFrame)
                    //   "+G" guide (line 753)
                    // Everything else → capture preview target.
                    if uuid.starts_with("+F") {
                        self.focus_preview_url.set(Some(url));
                    } else if uuid.starts_with("+A") {
                        self.align_preview_url.set(Some(url));
                    } else if uuid.starts_with("+G") {
                        self.guide_preview_url.set(Some(url));
                    } else {
                        self.capture_preview_url.set(Some(url));
                    }
                }
            }

            // Scheduler status push: {status: int} or {log: string}.
            // Both emitted from manager.cpp:417 and :427 via sendSchedulerStatus.
            "new_scheduler_state" => {
                self.scheduler_status.update(|s| {
                    if let Some(v) = payload["status"].as_i64() {
                        s.status = v;
                    }
                    if let Some(l) = payload["log"].as_str() {
                        if !l.is_empty() {
                            s.log = l.to_string();
                        }
                    }
                });
            }

            // Reply to scheduler_get_jobs — {"jobs": [...]}
            "scheduler_get_jobs" => {
                if let Some(arr) = payload["jobs"].as_array() {
                    // KStars only emits `new_scheduler_state` on transitions, so a
                    // browser that connects after the scheduler started never sees
                    // RUNNING. Recover by inspecting per-job states: SCHEDJOB_BUSY
                    // (3) means the scheduler is actively processing that job.
                    // Only promote when we still hold the default IDLE so a real
                    // PAUSED/ABORTED push isn't overwritten.
                    if self.scheduler_status.with(|s| s.status) == 0
                        && arr.iter().any(|j| j["state"].as_i64() == Some(3))
                    {
                        self.scheduler_status.update(|s| s.status = 2);
                    }
                    self.scheduler_jobs.set(arr.clone());
                }
            }

            // Debounced settings reply (message.cpp:623).
            "scheduler_get_all_settings" => {
                self.scheduler_settings.set(payload.clone());
            }

            // Mosaic tile grid pushed by KStars Framing Assistant.
            "new_mosaic_tiles" => {
                self.mosaic_tiles.set(Some(payload.clone()));
            }

            // LiveStacker push: either {state:"..."} or {state:"stacking", ok,
            // frames_stacked, total_frames, mean_snr, min_snr, max_snr}.
            // Merge — preserve previous numeric stats when only state changes.
            "new_livestacker_state" => {
                self.livestacker_state.update(|opt| {
                    let s = opt.get_or_insert_with(LiveStackerState::default);
                    if let Some(v) = payload["state"].as_str() {
                        s.state = v.to_string();
                    }
                    if let Some(v) = payload["ok"].as_bool() {
                        s.ok = v;
                    }
                    if let Some(v) = payload["frames_stacked"].as_u64() {
                        s.frames_stacked = v as u32;
                    }
                    if let Some(v) = payload["total_frames"].as_u64() {
                        s.total_frames = v as u32;
                    }
                    if let Some(v) = payload["mean_snr"].as_f64() {
                        s.mean_snr = v;
                    }
                    if let Some(v) = payload["min_snr"].as_f64() {
                        s.min_snr = v;
                    }
                    if let Some(v) = payload["max_snr"].as_f64() {
                        s.max_snr = v;
                    }
                    if let Some(v) = payload["message"].as_str() {
                        s.message = Some(v.to_string());
                    }
                });
            }

            "livestacker_get_all_settings" => {
                self.livestacker_settings.set(payload.clone());
            }

            // junos-server app launcher status: {"kstars":"running"|"stopped",
            // "phd2":"running"|"stopped"}.
            "app_state" => {
                if let Some(s) = payload["kstars"].as_str() {
                    self.kstars_running.set(s == "running");
                }
                if let Some(s) = payload["phd2"].as_str() {
                    self.phd2_running.set(s == "running");
                }
                self.kstars_state_known.set(true);
            }

            // KStars-side modal dialog (KSMessageBox). Payload schema
            // (kstars/auxiliary/ksmessagebox.cpp:275): {title, message,
            // icon, timeout, buttons:[label,...], default}. When the
            // dialog closes (accepted or rejected) KStars sends an empty
            // `{}` — treat that as a dismiss signal so the web modal
            // disappears in sync with the KStars one. Respond via
            // `dialog_get_response {button: "<label>"}` (mnemonic `&`
            // stripped — see message.cpp:2734 + ksmessagebox.cpp:301).
            "dialog_get_info" => {
                let has_dialog = payload
                    .as_object()
                    .map(|o| o.contains_key("buttons") || o.contains_key("message"))
                    .unwrap_or(false);
                if has_dialog {
                    self.active_dialog.set(Some(payload.clone()));
                } else {
                    self.active_dialog.set(None);
                }
            }

            _ => {}
        }
    }
}
