//! The native Guide tab's equipment: a card (the profile, a chip per
//! device that connects or disconnects it, Connect / Disconnect all, the
//! focal length and pixel scale, a gear to the sheet) and the Equipment
//! sheet — PHD2's Connect Equipment dialog and the camera and optics
//! settings: the profiles (select, new, rename, delete, import, export as
//! PHD2's `PHD Profile 1` text), the INDI server, each role's choice and
//! INDI device (the devices the server listed for that role as
//! suggestions), the camera rows, the focal length with the pixel scale it
//! gives, and the manual pointing a rig with no mount position needs.
//!
//! The gear choices go through `set_equipment`, which the daemon refuses
//! while anything is connected (as PHD2's dialog locks them); the camera
//! and optics rows through `set_profile_value`.

use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::JsCast;

use crate::components::form::{setting_row, CARD, CARD_TITLE, CHIP, LABEL, ROW, SELECT};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::GuiderStatusData;
use crate::Tab;

use super::protocol::{profile_rows, second_tap, Reply, Rpc, KEY_FOCAL_LENGTH, KEY_PIXEL_SIZE};
use super::rows::{bool_row, num_row, num_row_with, section, LabelFn, Num, Rows};
use super::{download_text, Sheet};

const INPUT: &str = "input input--sm w-[190px] shrink-0 max-md:h-9";

/// What the card and the sheet read and drive.
#[derive(Clone)]
pub struct Equip {
    pub state: RwSignal<Option<GuiderStatusData>>,
    pub settings: RwSignal<Value>,
    pub profiles: RwSignal<Value>,
    pub devices: RwSignal<Value>,
    pub rpc: Rpc,
    pub lang: RwSignal<Lang>,
    pub sheet: RwSignal<Option<Sheet>>,
}

/// A gear role: its name on the wire (`connect_device`, `guider_devices`'
/// `roles`), its chooser key and INDI device key in the profile, PHD2's
/// INDI choice, the `set_equipment` names of both, the extra choices.
struct Role {
    id: &'static str,
    label: LabelFn,
    choice_key: &'static str,
    device_key: &'static str,
    indi: &'static str,
    choice_param: &'static str,
    device_param: &'static str,
    extra: &'static [&'static str],
}

const ROLES: [Role; 5] = [
    Role {
        id: "camera",
        label: |t| t.guider_role_camera,
        choice_key: "/camera/LastMenuChoice",
        device_key: "/indi/INDIcam",
        indi: "INDI Camera",
        choice_param: "camera",
        device_param: "camera_device",
        extra: &[],
    },
    Role {
        id: "mount",
        label: |t| t.guider_role_mount,
        choice_key: "/scope/LastMenuChoice",
        device_key: "/indi/INDImount",
        indi: "INDI Mount",
        choice_param: "mount",
        device_param: "mount_device",
        extra: &["On-camera", "On-AO"],
    },
    // PHD2 keeps one INDI mount device for both the mount and the aux mount.
    Role {
        id: "aux_mount",
        label: |t| t.guider_role_aux,
        choice_key: "/scope/LastAuxMenuChoice",
        device_key: "/indi/INDImount",
        indi: "INDI Mount",
        choice_param: "aux_mount",
        device_param: "mount_device",
        extra: &[],
    },
    Role {
        id: "ao",
        label: |t| t.guider_role_ao,
        choice_key: "/stepguider/LastMenuChoice",
        device_key: "/indi/INDIao",
        indi: "SX AO (INDI)",
        choice_param: "ao",
        device_param: "ao_device",
        extra: &[],
    },
    Role {
        id: "rotator",
        label: |t| t.guider_role_rotator,
        choice_key: "/rotator/LastMenuChoice",
        device_key: "/indi/INDIrotator",
        indi: "INDI Rotator",
        choice_param: "rotator",
        device_param: "rotator_device",
        extra: &[],
    },
];

/// A role's device in `guider_state.equipment`: (name, connected).
fn device(s: &GuiderStatusData, role: &str) -> Option<(String, bool)> {
    let e = &s.equipment;
    match role {
        "camera" => e.camera.as_ref().map(|c| (c.name.clone(), c.connected)),
        "mount" => e.mount.as_ref().map(|d| (d.name.clone(), d.connected)),
        "aux_mount" => e.aux_mount.as_ref().map(|d| (d.name.clone(), d.connected)),
        "ao" => e.ao.as_ref().map(|d| (d.name.clone(), d.connected)),
        _ => e.rotator.as_ref().map(|d| (d.name.clone(), d.connected)),
    }
}

fn any_connected(s: &Option<GuiderStatusData>) -> bool {
    s.as_ref().is_some_and(|s| ROLES.iter().any(|r| device(s, r.id).is_some_and(|d| d.1)))
}

/// `″/px` from a pixel size (µm), the binning and a focal length (mm).
fn pixel_scale(px_um: f64, bin: f64, fl_mm: f64) -> Option<f64> {
    (px_um > 0.0 && fl_mm > 0.0).then(|| 206.265 * px_um * bin.max(1.0) / fl_mm)
}

/// One call whose reply the view waits for: its error goes to `err`, an
/// export's text is downloaded.
#[derive(Clone, Copy, PartialEq)]
enum After {
    Nothing,
    Download,
}

fn watch_replies(err: RwSignal<Option<String>>) -> RwSignal<Vec<(Reply, After)>> {
    let calls = RwSignal::new(Vec::<(Reply, After)>::new());
    Effect::new(move |_| {
        let done: Vec<usize> = calls.with(|c| {
            c.iter()
                .enumerate()
                .filter_map(|(i, (r, after))| {
                    match r.get()? {
                        Err(e) => err.set(Some(e.message)),
                        Ok(v) if *after == After::Download => {
                            let name = v["name"].as_str().unwrap_or("profile");
                            download_text(&format!("{name}.phd"), v["text"].as_str().unwrap_or_default());
                        }
                        Ok(_) => {}
                    }
                    Some(i)
                })
                .collect()
        });
        if !done.is_empty() {
            calls.update(|c| {
                for i in done.into_iter().rev() {
                    c.remove(i);
                }
            });
        }
    });
    calls
}

fn profile_select(e: &Equip, err: RwSignal<Option<String>>, calls: RwSignal<Vec<(Reply, After)>>) -> impl IntoView + use<> {
    let (profiles, rpc) = (e.profiles, e.rpc.clone());
    view! {
        <select class=SELECT
                on:change=move |ev| {
                    let Ok(id) = event_target_value(&ev).parse::<i64>() else { return };
                    err.set(None);
                    calls.update(|c| c.push((rpc.call("set_profile", json!({ "id": id })), After::Nothing)));
                }>
            {move || profiles.with(|p| {
                p.as_array().cloned().unwrap_or_default().into_iter().map(|p| {
                    let id = p["id"].as_i64().unwrap_or(0);
                    let name = p["name"].as_str().unwrap_or_default().to_string();
                    view! { <option value=id.to_string() prop:selected=p["selected"].as_bool().unwrap_or(false)>{name}</option> }
                }).collect_view()
            })}
        </select>
    }
}

/// The card in the tab's column.
pub fn equipment_card(e: Equip) -> impl IntoView {
    let lang = e.lang;
    let tr = move || t(lang.get());
    let (state, settings, rpc, sheet) = (e.state, e.settings, e.rpc.clone(), e.sheet);
    let err = RwSignal::new(None::<String>);
    let calls = watch_replies(err);
    let call = move |method: &'static str, params: Value| {
        err.set(None);
        calls.update(|c| c.push((rpc.call(method, params), After::Nothing)));
    };
    let call2 = call.clone();
    let chips = move || {
        let call = call.clone();
        state.with(|s| {
            let Some(s) = s else { return Vec::new() };
            ROLES
                .iter()
                .filter_map(|r| device(s, r.id).map(|d| (r, d)))
                .map(|(r, (name, on))| {
                    let call = call.clone();
                    let id = r.id;
                    let dot = if on { "bg-state-ok" } else { "bg-text-faint" };
                    view! {
                        <button type="button" class=format!("{CHIP} gap-1.5 max-w-full") title=name.clone()
                                on:click=move |_| call("connect_device", json!({ "role": id, "connected": !on }))>
                            <span class=format!("w-2 h-2 rounded-full shrink-0 {dot}")></span>
                            <span class="truncate">{move || (r.label)(tr())}</span>
                        </button>
                    }
                })
                .collect::<Vec<_>>()
        })
    };
    let select = profile_select(&e, err, calls);
    let connected = Memo::new(move |_| state.with(any_connected));
    let connecting = Memo::new(move |_| state.with(|s| s.as_ref().is_some_and(|s| s.connecting)));
    let optics = move || {
        let fl = settings.with(|s| s.get(KEY_FOCAL_LENGTH).and_then(Value::as_f64)).filter(|f| *f > 0.0);
        let scale = state.with(|s| s.as_ref().and_then(|s| s.pixel_scale));
        let mut parts = Vec::new();
        if let Some(fl) = fl {
            parts.push(format!("{fl:.0} mm"));
        }
        parts.push(scale.map_or_else(|| tr().guider_scale_unknown.to_string(), |k| format!("{k:.2}\u{2033}/px")));
        parts.join(" \u{00b7} ")
    };
    view! {
        <div class=CARD>
            <div class="flex items-center gap-2">
                <span class=format!("{CARD_TITLE} flex-1")>{move || tr().guider_equipment_title}</span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().guider_equipment_title
                        on:click=move |_| sheet.set(Some(Sheet::Equipment))>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Profiles)></span>
                </button>
            </div>
            {setting_row(move || tr().guider_profile, select)}
            <div class="flex flex-wrap gap-1.5">{chips}</div>
            <span class="text-xs text-text-muted font-mono">{optics}</span>
            <div class="flex gap-2">
                <button class="btn btn-ghost h-9 flex-1" disabled=move || connecting.get()
                        on:click={
                            let call = call2.clone();
                            move |_| call("set_connected", json!({ "connected": true }))
                        }>
                    {move || if connecting.get() { tr().guider_connecting } else { tr().guider_connect_all }}
                </button>
                <button class="btn btn-ghost h-9 flex-1" disabled=move || !connected.get()
                        on:click=move |_| call2("set_connected", json!({ "connected": false }))>
                    {move || tr().guider_disconnect_all}
                </button>
            </div>
            {move || err.get().map(|m| view! { <span class="text-xs text-state-err">{m}</span> })}
        </div>
    }
}

/// A role's row: its choice, and its INDI device (typed, or picked from
/// what the server listed for the role).
fn role_row<G: Fn(Value) + Clone + Send + Sync + 'static>(r: &'static Role, rows: &Rows, devices: RwSignal<Value>, gear: G) -> impl IntoView + use<G> {
    let values = rows.values;
    let tr = rows.label(r.label);
    let list = format!("guider-dev-{}", r.id);
    let choice = Memo::new(move |_| values.with(|v| v.get(r.choice_key).and_then(Value::as_str).unwrap_or("None").to_string()));
    let device = move || values.with(|v| v.get(r.device_key).and_then(Value::as_str).unwrap_or_default().to_string());
    let gear2 = gear.clone();
    let options = move || {
        let cur = choice.get();
        let mut all: Vec<&str> = vec!["None", r.indi];
        all.extend(r.extra);
        let extra = (!all.contains(&cur.as_str())).then(|| cur.clone());
        extra
            .into_iter()
            .chain(all.iter().map(|o| o.to_string()))
            .map(|o| {
                let sel = o == cur;
                view! { <option value=o.clone() prop:selected=sel>{o.clone()}</option> }
            })
            .collect_view()
    };
    let suggestions = move || {
        devices.with(|d| {
            d["devices"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["roles"].as_array().is_some_and(|rs| rs.iter().any(|x| x == r.id)))
                .map(|d| view! { <option value=d["name"].as_str().unwrap_or_default().to_string()></option> })
                .collect_view()
        })
    };
    view! {
        <div class="flex flex-col">
            <div class=ROW>
                <span class=format!("{LABEL} flex-1")>{tr}</span>
                <select class=SELECT on:change=move |ev| gear(json!({ r.choice_param: event_target_value(&ev) }))>
                    {options}
                </select>
            </div>
            <Show when=move || choice.with(|c| c.contains("INDI"))>
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1 pl-3 text-text-muted")>"INDI"</span>
                    <input class=INPUT list=list.clone() prop:value=device
                           on:change={
                               let gear = gear2.clone();
                               move |ev| gear(json!({ r.device_param: event_target_value(&ev) }))
                           } />
                    <datalist id=list.clone()>{suggestions}</datalist>
                </div>
            </Show>
        </div>
    }
}

/// The Equipment sheet's body.
pub fn equipment_sheet(e: Equip) -> impl IntoView {
    let lang = e.lang;
    let tr = move || t(lang.get());
    let (state, settings, devices, rpc) = (e.state, e.settings, e.devices, e.rpc.clone());
    let err = RwSignal::new(None::<String>);
    let calls = watch_replies(err);
    let call = {
        let rpc = rpc.clone();
        move |method: &'static str, params: Value, after: After| {
            err.set(None);
            calls.update(|c| c.push((rpc.call(method, params), after)));
        }
    };
    let rows = {
        let rpc = rpc.clone();
        profile_rows(
            settings,
            vec![
                ("/indi/INDIhost", json!("localhost")),
                ("/indi/INDIport", json!(7624)),
                ("/indi/INDIcam_ccd", json!(0)),
                ("/camera/binning", json!(1)),
                ("/camera/UseSubframes", json!(false)),
                (KEY_PIXEL_SIZE, json!(0.0)),
                ("/camera/SaturationByADU", json!(true)),
                ("/camera/SaturationADU", json!(65535)),
                ("/camera/TimeoutMs", json!(15000)),
                ("/camera/gain", json!(95)),
                ("/camera/CoolerSetpt", json!(10)),
                ("/indi/INDIcam_forcevideo", json!(false)),
                ("/indi/INDIcam_forceexposure", json!(false)),
                (KEY_FOCAL_LENGTH, json!(0)),
            ],
            lang,
            err,
            move |k, v| rpc.set_profile_value(k, v),
        )
    };
    let locked = Memo::new(move |_| state.with(any_connected));
    let gear = {
        let call = call.clone();
        move |params: Value| call("set_equipment", params, After::Nothing)
    };

    // Profiles: the name field New and Rename take.
    let name = RwSignal::new(String::new());
    let armed = RwSignal::new(false);
    let current = move || state.with(|s| s.as_ref().map_or(0, |s| s.profile.id));
    let import = {
        let call = call.clone();
        move |ev: web_sys::Event| {
            let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
            let Some(file) = input.files().and_then(|f| f.get(0)) else { return };
            input.set_value("");
            let call = call.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let Ok(text) = wasm_bindgen_futures::JsFuture::from(file.text()).await else { return };
                let fname = file.name();
                let stem = fname.rsplit_once('.').map_or(fname.as_str(), |(s, _)| s).to_string();
                call("import_profile", json!({ "text": text.as_string().unwrap_or_default(), "name": stem }), After::Nothing);
            });
        }
    };
    let (c_new, c_ren, c_del, c_exp) = (call.clone(), call.clone(), call.clone(), call.clone());
    let select = profile_select(&e, err, calls);
    let profiles_body = view! {
        {setting_row(move || tr().guider_profile, select)}
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || tr().guider_profile_name}</span>
            <input class=INPUT prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
        </div>
        <div class="flex flex-wrap gap-2 pt-1">
            <button class="btn btn-ghost h-9 px-3" disabled=move || name.with(|n| n.trim().is_empty())
                    on:click=move |_| c_new("new_profile", json!({ "name": name.get_untracked().trim() }), After::Nothing)>
                {move || tr().guider_profile_new}
            </button>
            <button class="btn btn-ghost h-9 px-3" disabled=move || name.with(|n| n.trim().is_empty())
                    on:click=move |_| c_ren("rename_profile", json!({ "id": current(), "name": name.get_untracked().trim() }), After::Nothing)>
                {move || tr().guider_profile_rename}
            </button>
            <button class=move || if armed.get() { "btn btn-danger h-9 px-3" } else { "btn btn-ghost h-9 px-3" }
                    on:click=move |_| if second_tap(armed) { c_del("delete_profile", json!({ "id": current() }), After::Nothing) }>
                {move || if armed.get() { tr().guider_tap_to_confirm } else { tr().guider_profile_delete }}
            </button>
            <label class="btn btn-ghost h-9 px-3 cursor-pointer">
                {move || tr().guider_profile_import}
                <input type="file" accept=".phd,.txt" class="hidden" on:change=import />
            </label>
            <button class="btn btn-ghost h-9 px-3"
                    on:click=move |_| c_exp("export_profile", json!({}), After::Download)>
                {move || tr().guider_profile_export}
            </button>
        </div>
    };

    let indi_body = {
        let (g1, g2) = (gear.clone(), gear.clone());
        let values = rows.values;
        let host = move || values.with(|v| v.get("/indi/INDIhost").and_then(Value::as_str).unwrap_or_default().to_string());
        let port = move || values.with(|v| v.get("/indi/INDIport").and_then(Value::as_i64).unwrap_or(7624).to_string());
        let up = move || state.with(|s| s.as_ref().is_some_and(|s| s.indi.connected));
        view! {
            <div class=ROW>
                <span class=format!("{LABEL} flex-1 flex items-center gap-2")>
                    <span class=move || format!("w-2 h-2 rounded-full {}", if up() { "bg-state-ok" } else { "bg-text-faint" })></span>
                    {move || tr().guider_indi_host}
                </span>
                <input class=INPUT prop:value=host on:change=move |ev| g1(json!({ "indi_host": event_target_value(&ev) })) />
            </div>
            <div class=ROW>
                <span class=format!("{LABEL} flex-1")>{move || tr().guider_indi_port}</span>
                <input type="number" inputmode="numeric" min="1" max="65535" class="input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9"
                       prop:value=port
                       on:change=move |ev| {
                           if let Ok(p) = event_target_value(&ev).parse::<i64>() {
                               g2(json!({ "indi_port": p }))
                           }
                       } />
            </div>
        }
    };

    let devices_body = {
        let roles = ROLES.iter().map(|r| role_row(r, &rows, devices, gear.clone())).collect_view();
        view! {
            <Show when=move || locked.get()>
                <span class="text-xs text-state-warn">{move || tr().guider_gear_locked}</span>
            </Show>
            {roles}
        }
    };

    let camera_body = {
        let values = rows.values;
        let set = rows.set.clone();
        let gain = move || state.with(|s| s.as_ref().and_then(|s| s.equipment.camera.as_ref()).is_some_and(|c| c.gain_supported));
        let ccd = move || values.with(|v| v.get("/indi/INDIcam_ccd").and_then(Value::as_i64).unwrap_or(0));
        let rows2 = rows.clone();
        view! {
            {setting_row(move || tr().guider_cam_ccd, view! {
                <select class=SELECT on:change=move |ev| set("/indi/INDIcam_ccd", json!(event_target_value(&ev).parse::<i64>().unwrap_or(0)))>
                    <option value="0" prop:selected=move || ccd() == 0>{move || tr().guider_cam_ccd_main}</option>
                    <option value="1" prop:selected=move || ccd() != 0>{move || tr().guider_cam_ccd_guide}</option>
                </select>
            })}
            {num_row(&rows, "/camera/binning", |t| t.guider_cam_binning, 1.0, 4.0, 1.0)}
            {bool_row(&rows, "/camera/UseSubframes", |t| t.guider_cam_subframes)}
            {num_row(&rows, KEY_PIXEL_SIZE, |t| t.guider_cam_pixel_size, 0.0, 99.9, 0.1)}
            {bool_row(&rows, "/camera/SaturationByADU", |t| t.guider_cam_sat_by_adu)}
            {num_row(&rows, "/camera/SaturationADU", |t| t.guider_cam_sat_adu, 0.0, 65535.0, 1.0)}
            {num_row_with(&rows, "/camera/TimeoutMs", |t| t.guider_cam_timeout,
                          Num { min: 5.0, max: 9999.0, step: 1.0, scale: 0.001, int: true })}
            {move || gain().then(|| num_row(&rows2, "/camera/gain", |t| t.guider_cam_gain, 0.0, 100.0, 1.0))}
            {num_row(&rows, "/camera/CoolerSetpt", |t| t.guider_cam_cooler, -99.0, 99.0, 1.0)}
            {bool_row(&rows, "/indi/INDIcam_forcevideo", |t| t.guider_cam_force_video)}
            {bool_row(&rows, "/indi/INDIcam_forceexposure", |t| t.guider_cam_force_exposure)}
        }
    };

    let optics_body = {
        let values = rows.values;
        let computed = move || {
            let get = |k: &str| values.with(|v| v.get(k).and_then(Value::as_f64).unwrap_or(0.0));
            pixel_scale(get(KEY_PIXEL_SIZE), get("/camera/binning"), get(KEY_FOCAL_LENGTH))
                .map_or_else(|| tr().guider_scale_unknown.to_string(), |k| format!("{k:.2}\u{2033}/px"))
        };
        view! {
            {num_row(&rows, KEY_FOCAL_LENGTH, |t| t.guider_focal_length, 0.0, 9999.0, 1.0)}
            {setting_row(move || tr().guider_pixel_scale, view! { <span class="font-mono text-sm text-text-dim">{computed}</span> })}
        }
    };

    // Manual pointing: sent whole, for a rig no mount reports a position for.
    let mp: [RwSignal<String>; 4] = std::array::from_fn(|_| RwSignal::new(String::new()));
    let side = RwSignal::new("Unknown".to_string());
    let (c_mp, c_clear) = (call.clone(), call.clone());
    let field = move |i: usize, label: LabelFn| view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label(tr())}</span>
            <input type="number" inputmode="decimal" step="any" class="input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9"
                   prop:value=move || mp[i].get() on:input=move |ev| mp[i].set(event_target_value(&ev)) />
        </div>
    };
    let pointing_body = view! {
        <span class="text-xs text-text-muted">{move || tr().guider_manual_pointing_hint}</span>
        {field(0, |t| t.guider_latitude)}
        {field(1, |t| t.guider_longitude)}
        {field(2, |t| t.guider_ra_hours)}
        {field(3, |t| t.guider_dec_deg)}
        {setting_row(move || tr().guider_pier_side, view! {
            <select class=SELECT on:change=move |ev| side.set(event_target_value(&ev))>
                {["Unknown", "East", "West"].map(|s| view! { <option value=s prop:selected=move || side.with(|v| v == s)>{s}</option> })}
            </select>
        })}
        <div class="flex gap-2 pt-1">
            <button class="btn btn-ghost h-9 flex-1"
                    on:click=move |_| {
                        let v: Vec<Option<f64>> = mp.iter().map(|s| s.get_untracked().trim().parse().ok()).collect();
                        let Some([lat, lon, ra, dec]) = v.into_iter().collect::<Option<Vec<f64>>>().and_then(|v| <[f64; 4]>::try_from(v).ok()) else {
                            err.set(Some(tr().guider_manual_pointing_incomplete.to_string()));
                            return;
                        };
                        c_mp("set_manual_pointing", json!({
                            "latitude": lat, "longitude": lon, "ra": ra, "dec": dec, "pier_side": side.get_untracked(),
                        }), After::Nothing);
                    }>
                {move || tr().guider_apply}
            </button>
            <button class="btn btn-ghost h-9 flex-1"
                    on:click=move |_| c_clear("set_manual_pointing", json!({ "ra": null }), After::Nothing)>
                {move || tr().guider_clear}
            </button>
        </div>
    };

    let title = |f: fn(&Translations) -> &'static str| move || f(t(lang.get()));
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    flex flex-col gap-3">
            {move || err.get().map(|m| view! { <div class="text-sm text-state-err">{m}</div> })}
            <div class="grid gap-3 md:grid-cols-2 items-start">
                <div class="flex flex-col gap-3">
                    {section(title(|t| t.guider_profiles_title), true, profiles_body)}
                    {section(title(|t| t.guider_indi_title), true, indi_body)}
                    {section(title(|t| t.guider_devices_title), true, devices_body)}
                </div>
                <div class="flex flex-col gap-3">
                    {section(title(|t| t.guider_camera_title), true, camera_body)}
                    {section(title(|t| t.guider_optics_title), true, optics_body)}
                    {section(title(|t| t.guider_manual_pointing_title), false, pointing_body)}
                </div>
            </div>
        </div>
    }
}
