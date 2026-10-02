//! Profiles tab (the gear): the server's applications, Ekos equipment
//! profiles, the running profile's optical trains (rigs) and the telescopes.
//!
//! Layout (phone-first, like Mount): a header (selected profile · Ekos state ·
//! refresh), then cards — Applications and Profiles, Rigs and Telescopes — in
//! one scrolling column on phones, two from `md`, and a pinned footer with New
//! profile. Each profile row launches or stops its session; its pencil, or a
//! rig's or a telescope's, opens an editor `sheet` with Delete and Save.
//!
//! KStars handles `get_*`, `profile_*` and `scope_*` before its Ekos-startup
//! gate (message.cpp:351-427), so they work offline; `train_*` mutations need
//! Ekos online, and `train_get_all` only lists the running profile's trains.
//!
//! Inbound:  `get_profiles`, `get_drivers`, `get_scopes`, `train_get_all`,
//!           `train_get_profiles` — folded by `ws/store.rs`
//! Outbound: `profile_{start,stop,add,update,delete}` (KStars re-sends
//!           `get_profiles`), `scope_{add,update,delete}` (re-sends
//!           `get_scopes`), `train_{add,update,delete,set}` (no reply, so
//!           re-fetched here)
//! HTTP:     `/api/apps/{launch,stop}` — KStars and PHD2 on the server host

use std::time::Duration;

use leptos::prelude::*;
use serde_json::json;

use crate::components::form::{setting_row, sheet, CARD, CARD_TITLE, CHECK, CHIP, FOOTER, LABEL, ROW, SELECT};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::{DeviceInfo, DriverInfo, OpticalTrain, ProfileInfo, ScopeInfo, SendCmd};
use crate::ws_helpers::send_cmd;
use crate::Tab;

type LabelFn = fn(&Translations) -> &'static str;
type Getter<T> = fn(&T) -> String;
type Setter<T> = fn(&mut T, String);

const GUIDING: [&str; 4] = ["Internal", "PHD2", "LinGuider", "SEP"];
const SCOPE_TYPES: [&str; 3] = ["Refractor", "Reflector", "Catadioptric"];

/// Profile driver slots. A slot holds a driver `label` (profileeditor.cpp:531)
/// picked among the drivers of its `family` (indicommon.h:143).
const AUX: &[&str] = &["Auxiliary", "Spectrographs", "Detectors", "Rotators", "Power"];
const DRIVERS: [(&str, &[&str], Getter<ProfileInfo>, Setter<ProfileInfo>); 12] = [
    ("Mount",   &["Telescopes"],      |p| p.mount.clone(),   |p, v| p.mount = v),
    ("CCD",     &["CCDs"],            |p| p.ccd.clone(),     |p, v| p.ccd = v),
    ("Guider",  &["CCDs"],            |p| p.guider.clone(),  |p, v| p.guider = v),
    ("Focuser", &["Focusers"],        |p| p.focuser.clone(), |p, v| p.focuser = v),
    ("Filter",  &["Filter Wheels"],   |p| p.filter.clone(),  |p, v| p.filter = v),
    ("AO",      &["Adaptive Optics"], |p| p.ao.clone(),      |p, v| p.ao = v),
    ("Dome",    &["Domes"],           |p| p.dome.clone(),    |p, v| p.dome = v),
    ("Weather", &["Weather"],         |p| p.weather.clone(), |p, v| p.weather = v),
    ("Aux 1",   AUX,                  |p| p.aux1.clone(),    |p, v| p.aux1 = v),
    ("Aux 2",   AUX,                  |p| p.aux2.clone(),    |p, v| p.aux2 = v),
    ("Aux 3",   AUX,                  |p| p.aux3.clone(),    |p, v| p.aux3 = v),
    ("Aux 4",   AUX,                  |p| p.aux4.clone(),    |p, v| p.aux4 = v),
];

/// Train device slots, picked among connected devices by their libindi
/// driver-interface bit (basedevice.h).
const TRAIN_ROLES: [(LabelFn, i64, Getter<OpticalTrain>, Setter<OpticalTrain>); 6] = [
    (|t| t.rig_role_mount,   1 << 0,  |t| t.mount.clone(),       |t, v| t.mount = v),
    (|t| t.rig_role_camera,  1 << 1,  |t| t.camera.clone(),      |t, v| t.camera = v),
    (|t| t.rig_role_guider,  1 << 1,  |t| t.guider.clone(),      |t, v| t.guider = v),
    (|t| t.rig_role_focuser, 1 << 3,  |t| t.focuser.clone(),     |t, v| t.focuser = v),
    (|t| t.rig_role_filter,  1 << 4,  |t| t.filterwheel.clone(), |t, v| t.filterwheel = v),
    (|t| t.rig_role_rotator, 1 << 12, |t| t.rotator.clone(),     |t, v| t.rotator = v),
];

/// (module wire name, ProfileSettings key — profilesettings.h:49-56, label).
const MODULES: [(&str, &str, LabelFn); 5] = [
    ("capture", "1", |t| t.rig_module_capture),
    ("focus",   "2", |t| t.rig_module_focus),
    ("guide",   "4", |t| t.rig_module_guide),
    ("align",   "5", |t| t.rig_module_align),
    ("mount",   "3", |t| t.rig_module_mount),
];

/// Text field, as wide as form.rs' SELECT.
const TEXT: &str = "input input--sm w-[190px] shrink-0 max-md:h-9";
/// A profile, rig or telescope in its card.
const ITEM: &str = "flex items-center gap-2 min-h-[52px] px-3 py-1.5 rounded-md border border-border-base bg-bg-elev-2";
const ACTION: &str = "btn h-10 md:h-9 px-4 w-[5.5rem] shrink-0";
const PENCIL: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20h4L19 9l-4-4L4 16z M13.5 6.5l4 4"/></svg>"##;

/// The open editor sheet; `None` inside means a new item.
#[derive(Clone)]
enum Editor {
    Profile(Option<ProfileInfo>),
    Train(Option<OpticalTrain>),
    Scope(Option<ScopeInfo>),
}

#[component]
pub fn ProfilesTab(
    profiles: RwSignal<Vec<ProfileInfo>>,
    selected_profile: RwSignal<Option<String>>,
    drivers: RwSignal<Vec<DriverInfo>>,
    online: RwSignal<bool>,
    connected: RwSignal<bool>,
    kstars_running: RwSignal<bool>,
    phd2_running: RwSignal<bool>,
    devices: RwSignal<Vec<DeviceInfo>>,
    optical_trains: RwSignal<Vec<OpticalTrain>>,
    scopes: RwSignal<Vec<ScopeInfo>>,
    module_trains: RwSignal<serde_json::Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let editor = RwSignal::new(None::<Editor>);
    // Profile being launched; cleared once Ekos is online, or after 30 s.
    let starting = RwSignal::new(None::<String>);
    Effect::new(move |_| if online.get() { starting.set(None) });

    let state = Memo::new(move |_| {
        let tr = tr();
        if !connected.get() {
            (tr.disconnected, "badge")
        } else if starting.get().is_some() {
            (tr.profiles_starting, "badge badge--warn")
        } else if online.get() {
            (tr.running, "badge badge--ok")
        } else {
            (tr.stopped, "badge")
        }
    });

    let send_refresh = send.clone();
    let refresh = move |_| {
        for ty in ["get_profiles", "get_drivers", "get_scopes"] {
            send_cmd(&send_refresh, ty, json!({}));
        }
        if online.get_untracked() {
            refetch_trains(&send_refresh);
        }
    };

    let send_launch = send.clone();
    let launch = Callback::new(move |name: String| {
        // KStars stops a running session itself before starting another.
        if online.get_untracked() && !confirm(t(lang.get_untracked()).profiles_confirm_launch) {
            return;
        }
        send_cmd(&send_launch, "profile_start", json!({ "name": name }));
        starting.set(Some(name.clone()));
        set_timeout(
            move || if starting.get_untracked().as_deref() == Some(name.as_str()) { starting.set(None) },
            Duration::from_secs(30),
        );
    });
    let send_stop = send.clone();
    let stop = Callback::new(move |()| send_cmd(&send_stop, "profile_stop", json!({})));

    let send_modules = send.clone();
    let send_editor = send.clone();

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Profiles)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_profiles}</span>
                <span class="min-w-0 truncate text-sm text-text-muted">{move || selected_profile.get()}</span>
                <span class=move || format!("{} ml-auto shrink-0", state.get().1)>{move || state.get().0}</span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().files_refresh on:click=refresh>
                    "\u{21bb}"
                </button>
            </div>

            // Cards — one column on phones, two from md.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                <div class="max-w-[1100px] mx-auto grid grid-cols-1 gap-3 md:grid-cols-2 items-start">
                    <div class="flex flex-col gap-3">
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().apps_section}</span>
                            {app_row(move || tr().apps_kstars, "kstars", kstars_running, lang)}
                            {app_row(move || tr().apps_phd2, "phd2", phd2_running, lang)}
                        </div>

                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().profiles_title}</span>
                            {move || {
                                let tr = tr();
                                let list = profiles.get();
                                if list.is_empty() {
                                    return hint(tr.profiles_empty).into_any();
                                }
                                let live = online.get().then(|| selected_profile.get()).flatten();
                                let launching = starting.get();
                                let can_launch = connected.get() && launching.is_none();
                                list.into_iter().map(|p| {
                                    let running = live.as_deref() == Some(p.name.as_str());
                                    let is_starting = launching.as_deref() == Some(p.name.as_str());
                                    profile_row(p, tr, running, is_starting, can_launch, editor, launch, stop)
                                }).collect_view().into_any()
                            }}
                        </div>
                    </div>

                    <div class="flex flex-col gap-3">
                        <div class=CARD>
                            {card_head(move || tr().rig_section, move || tr().rig_add_train, move || !online.get(),
                                       move || editor.set(Some(Editor::Train(None))))}
                            {move || {
                                let tr = tr();
                                if !online.get() {
                                    return hint(tr.rig_offline_hint).into_any();
                                }
                                let trains = optical_trains.get();
                                if trains.is_empty() {
                                    return hint(tr.rig_no_trains).into_any();
                                }
                                let modules = module_trains.get();
                                let send = send_modules.clone();
                                view! {
                                    {trains.iter().cloned().map(|trn| {
                                        let detail = summary([&trn.scope, &trn.mount, &trn.camera, &trn.guider]);
                                        item_row(trn.name.clone(), detail, tr,
                                                 move || editor.set(Some(Editor::Train(Some(trn.clone())))))
                                    }).collect_view()}
                                    <span class=format!("{CARD_TITLE} pt-2")>{tr.rig_modules_section}</span>
                                    {MODULES.map(|m| {
                                        let cur = modules.get(m.1).and_then(|v| v.as_i64());
                                        module_row(m, trains.clone(), cur, tr, send.clone())
                                    })}
                                }.into_any()
                            }}
                        </div>

                        <div class=CARD>
                            {card_head(move || tr().rig_scopes_section, move || tr().rig_add_scope, || false,
                                       move || editor.set(Some(Editor::Scope(None))))}
                            {move || {
                                let tr = tr();
                                let list = scopes.get();
                                if list.is_empty() {
                                    return hint(tr.rig_no_scopes).into_any();
                                }
                                list.into_iter().map(|sc| {
                                    let name = if sc.name.is_empty() {
                                        format!("{} {}", sc.vendor, sc.model).trim().to_string()
                                    } else {
                                        sc.name.clone()
                                    };
                                    let ratio = if sc.aperture_mm > 0.0 { sc.focal_length_mm / sc.aperture_mm } else { 0.0 };
                                    let detail = format!("{} \u{00b7} {:.0} mm f/{ratio:.1}", sc.type_, sc.focal_length_mm);
                                    item_row(name, detail, tr, move || editor.set(Some(Editor::Scope(Some(sc.clone())))))
                                }).collect_view().into_any()
                            }}
                        </div>
                    </div>
                </div>
            </div>

            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <button class="btn btn-primary h-11 px-5 font-semibold md:ml-auto max-md:flex-1"
                        on:click=move |_| editor.set(Some(Editor::Profile(None)))>
                    {move || format!("+ {}", tr().profiles_new)}
                </button>
            </div>

            // Only `editor` is tracked: a store update mustn't reset the form.
            {move || editor.get().map(|e| untrack(|| {
                let send = send_editor.clone();
                match e {
                    Editor::Profile(p) => {
                        let live = online.get().then(|| selected_profile.get()).flatten();
                        profile_sheet(p, live, profiles, drivers, editor, send, lang).into_any()
                    }
                    Editor::Train(trn) => train_sheet(trn, devices, scopes, editor, send, lang).into_any(),
                    Editor::Scope(sc) => scope_sheet(sc, editor, send, lang).into_any(),
                }
            }))}
        </div>
    }
}

// ── Cards ─────────────────────────────────────────────────────────────────────

/// KStars or PHD2 on the server host: state, then Launch / Stop. The state
/// comes back over `/ws`; only a failure needs the HTTP reply.
fn app_row(
    label: impl Fn() -> &'static str + Send + 'static,
    app: &'static str,
    running: RwSignal<bool>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let error = RwSignal::new(None::<String>);
    let toggle = move |_| {
        let endpoint = if running.get_untracked() { "/api/apps/stop" } else { "/api/apps/launch" };
        error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post(endpoint)
                    .json(&json!({ "app": app })).map_err(|e| e.to_string())?
                    .send().await.map_err(|e| e.to_string())?;
                if !resp.ok() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
                match body["ok"].as_bool() {
                    Some(true) => Ok(()),
                    _ => Err(body["error"].as_str().unwrap_or("unknown error").to_string()),
                }
            }.await;
            if let Err(e) = result {
                error.set(Some(e));
            }
        });
    };
    view! {
        <div class=ROW>
            <span class=move || if running.get() {
                "w-2 h-2 shrink-0 rounded-full bg-state-ok"
            } else {
                "w-2 h-2 shrink-0 rounded-full bg-text-faint"
            }></span>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            <span class="text-xs text-text-muted">
                {move || if running.get() { tr().apps_running } else { tr().apps_stopped }}
            </span>
            <button class=move || if running.get() { format!("{ACTION} btn-danger") } else { format!("{ACTION} btn-primary") }
                    on:click=toggle>
                {move || if running.get() { tr().apps_stop } else { tr().apps_launch }}
            </button>
        </div>
        {move || error.get().map(|e| view! { <div class="text-xs text-state-err break-words">{e}</div> })}
    }
}

/// One profile: name, mode and drivers, its pencil, then Launch / Stop.
#[allow(clippy::too_many_arguments)]
fn profile_row(
    p: ProfileInfo,
    tr: &'static Translations,
    running: bool,
    starting: bool,
    can_launch: bool,
    editor: RwSignal<Option<Editor>>,
    launch: Callback<String>,
    stop: Callback<()>,
) -> impl IntoView {
    let name = p.name.clone();
    let detail = summary([&p.mode, &p.mount, &p.ccd, &p.guider, &p.focuser, &p.filter]);
    let action = if running {
        view! {
            <button class=format!("{ACTION} btn-danger") on:click=move |_| stop.run(())>{tr.profiles_stop}</button>
        }.into_any()
    } else if starting {
        view! { <span class=format!("{ACTION} btn-ghost text-state-warn")>{tr.profiles_starting}</span> }.into_any()
    } else {
        let n = name.clone();
        view! {
            <button class=format!("{ACTION} btn-primary") disabled=!can_launch on:click=move |_| launch.run(n.clone())>
                {tr.profiles_launch}
            </button>
        }.into_any()
    };
    view! {
        <div class=if running { format!("{ITEM} btn--active") } else { ITEM.to_string() }>
            <div class="flex-1 min-w-0">
                <div class="flex items-center gap-2">
                    <span class="truncate font-semibold text-text-dim">{name}</span>
                    {running.then(|| view! { <span class="badge badge--ok shrink-0">{tr.running}</span> })}
                </div>
                <div class="truncate text-xs text-text-muted">{detail}</div>
            </div>
            {pencil(tr, move || editor.set(Some(Editor::Profile(Some(p.clone())))))}
            {action}
        </div>
    }
}

/// A rig or a telescope: name, details and its pencil.
fn item_row(name: String, detail: String, tr: &'static Translations, edit: impl Fn() + 'static) -> impl IntoView {
    view! {
        <div class=ITEM>
            <div class="flex-1 min-w-0">
                <div class="truncate font-semibold text-text-dim">{name}</div>
                <div class="truncate text-xs text-text-muted">{detail}</div>
            </div>
            {pencil(tr, edit)}
        </div>
    }
}

fn pencil(tr: &'static Translations, edit: impl Fn() + 'static) -> impl IntoView {
    view! {
        <button class="btn-icon shrink-0 text-text-muted" title=tr.profiles_edit on:click=move |_| edit()>
            <span class="inline-block w-5 h-5" inner_html=PENCIL></span>
        </button>
    }
}

/// Card title with a "+" on the right.
fn card_head(
    title: impl Fn() -> &'static str + Send + 'static,
    add_title: impl Fn() -> &'static str + Send + 'static,
    disabled: impl Fn() -> bool + Send + Sync + 'static,
    add: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <div class="flex items-center gap-2 -my-1">
            <span class=format!("{CARD_TITLE} flex-1")>{move || title()}</span>
            <button class="btn-icon shrink-0 text-lg text-accent-cyan" title=move || add_title()
                    disabled=disabled on:click=move |_| add()>
                "+"
            </button>
        </div>
    }
}

/// Which train a module uses; KStars applies a change at once.
fn module_row(
    (module, _, label): (&'static str, &'static str, LabelFn),
    trains: Vec<OpticalTrain>,
    cur: Option<i64>,
    tr: &'static Translations,
    send: SendCmd,
) -> impl IntoView {
    setting_row(move || label(tr), view! {
        <select class=SELECT on:change=move |ev| {
            send_cmd(&send, "train_set", json!({ "module": module, "name": event_target_value(&ev) }));
            send_cmd(&send, "train_get_profiles", json!({}));
        }>
            <option value="" disabled selected=cur.is_none()>"--"</option>
            {trains.into_iter().map(|trn| view! {
                <option value=trn.name.clone() selected=Some(trn.id) == cur>{trn.name.clone()}</option>
            }).collect_view()}
        </select>
    })
}

fn hint(text: &'static str) -> impl IntoView {
    view! { <div class="py-4 text-center text-sm text-text-faint">{text}</div> }
}

/// Non-empty slots joined with " · ", or a dash.
fn summary<const N: usize>(slots: [&String; N]) -> String {
    let parts: Vec<&str> = slots.iter().map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "--").collect();
    if parts.is_empty() { "\u{2014}".into() } else { parts.join(" \u{00b7} ") }
}

// ── Editor sheets ─────────────────────────────────────────────────────────────

fn profile_sheet(
    orig: Option<ProfileInfo>,
    live: Option<String>,
    profiles: RwSignal<Vec<ProfileInfo>>,
    drivers: RwSignal<Vec<DriverInfo>>,
    editor: RwSignal<Option<Editor>>,
    send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let title: LabelFn = if orig.is_none() { |t| t.profiles_new } else { |t| t.profiles_edit };
    let original = orig.as_ref().map(|p| p.name.clone());
    let p = RwSignal::new(orig.unwrap_or_else(|| ProfileInfo {
        mode: "local".into(),
        driver_source: "system".into(),
        ..Default::default()
    }));

    // KStars keys profiles by name: no duplicates, and a rename is delete + add.
    let taken = {
        let original = original.clone();
        move || p.with(|p| {
            let n = p.name.trim();
            original.as_deref() != Some(n) && profiles.with(|l| l.iter().any(|q| q.name == n))
        })
    };
    let invalid = {
        let taken = taken.clone();
        move || p.with(|p| p.name.trim().is_empty()) || taken()
    };
    let save = {
        let (send, original, invalid) = (send.clone(), original.clone(), invalid.clone());
        move || {
            if invalid() {
                return;
            }
            let mut q = p.get_untracked();
            q.name = q.name.trim().to_string();
            let cmd = match &original {
                Some(o) if *o == q.name => "profile_update",
                Some(o) => {
                    send_cmd(&send, "profile_delete", json!({ "name": o }));
                    "profile_add"
                }
                None => "profile_add",
            };
            send_cmd(&send, cmd, q.to_json());
            editor.set(None);
        }
    };
    let can_delete = original.as_deref().map(|n| n != "Simulators" && live.as_deref() != Some(n));
    let delete = move || {
        if let Some(o) = &original {
            if confirm(t(lang.get_untracked()).profiles_confirm_delete) {
                send_cmd(&send, "profile_delete", json!({ "name": o }));
                editor.set(None);
            }
        }
    };

    let trs = t(lang.get_untracked());
    let cards = view! {
        <div class=CARD>
            {text_row(move || tr().profiles_name, p, |p| p.name.clone(), |p, v| p.name = v, "", "text")}
            <Show when=taken>
                <span class="text-xs text-state-err text-right">{move || tr().profiles_name_taken}</span>
            </Show>
            {pills_row(move || tr().profiles_mode, p, |p, remote| (p.mode == "remote") == remote,
                       |p, remote| p.mode = if remote { "remote" } else { "local" }.into(),
                       vec![(false, trs.profiles_mode_local), (true, trs.profiles_mode_remote)])}
            {pills_row(move || tr().profiles_guiding, p, |p, g| p.guiding == g, |p, g| p.guiding = g,
                       GUIDING.iter().enumerate().map(|(i, l)| (i as i32, *l)).collect())}
            {flag_row(move || tr().profiles_auto_connect, p, |p| p.auto_connect, |p, v| p.auto_connect = v)}
            {flag_row(move || tr().profiles_port_selector, p, |p| p.port_selector, |p, v| p.port_selector = v)}
            {flag_row(move || tr().profiles_web_manager, p, |p| p.use_web_manager, |p, v| p.use_web_manager = v)}
        </div>
        <Show when=move || p.with(|p| p.mode == "remote")>
            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().profiles_mode_remote}</span>
                {text_row(move || tr().profiles_host, p, |p| p.remote_host.clone(), |p, v| p.remote_host = v,
                          "localhost", "url")}
                {text_row(move || tr().profiles_port, p, |p| port_text(p.remote_port),
                          |p, v| p.remote_port = parse_port(&v), "7624", "numeric")}
                {text_row(move || tr().profiles_guiding_host, p, |p| p.remote_guiding_host.clone(),
                          |p, v| p.remote_guiding_host = v, "", "url")}
                {text_row(move || tr().profiles_guiding_port, p, |p| port_text(p.remote_guiding_port),
                          |p, v| p.remote_guiding_port = parse_port(&v), "", "numeric")}
            </div>
        </Show>
        <div class=CARD>
            <span class=CARD_TITLE>{move || tr().profiles_drivers}</span>
            {DRIVERS.map(|(label, families, get, set)| pick_row(move || label, p, get, set, move || {
                drivers.with(|d| d.iter().filter(|d| families.contains(&d.family.as_str())).map(|d| d.label.clone()).collect())
            }))}
            {text_row(move || tr().profiles_remote_drivers, p, |p| p.remote.clone(), |p, v| p.remote = v,
                      "indi_eqmod_telescope,\u{2026}", "text")}
        </div>
    };
    editor_sheet(title, editor, lang, cards, can_delete, delete, invalid, save)
}

fn train_sheet(
    orig: Option<OpticalTrain>,
    devices: RwSignal<Vec<DeviceInfo>>,
    scopes: RwSignal<Vec<ScopeInfo>>,
    editor: RwSignal<Option<Editor>>,
    send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let title: LabelFn = if orig.is_none() { |t| t.rig_new_train } else { |t| t.rig_edit_train };
    let original = orig.as_ref().map(|trn| trn.name.clone());
    let trn = RwSignal::new(orig.unwrap_or_default());
    // Kept as text so "1." can be typed; parsed on save.
    let reducer = RwSignal::new(trn.with_untracked(|trn| trn.reducer.to_string()));

    let invalid = move || trn.with(|trn| trn.name.trim().is_empty());
    let save = {
        let (send, is_new) = (send.clone(), original.is_none());
        move || {
            if invalid() {
                return;
            }
            let mut out = trn.get_untracked();
            out.name = out.name.trim().to_string();
            out.reducer = reducer.get_untracked().trim().parse().ok().filter(|r| *r > 0.0).unwrap_or(1.0);
            send_cmd(&send, if is_new { "train_add" } else { "train_update" }, out.to_json(!is_new));
            refetch_trains(&send);
            editor.set(None);
        }
    };
    let can_delete = original.as_ref().map(|_| true);
    let delete = move || {
        if let Some(name) = &original {
            if confirm(t(lang.get_untracked()).rig_confirm_delete_train) {
                send_cmd(&send, "train_delete", json!({ "name": name }));
                refetch_trains(&send);
                editor.set(None);
            }
        }
    };

    let cards = view! {
        <div class=CARD>
            {text_row(move || tr().rig_train_name, trn, |t| t.name.clone(), |t, v| t.name = v, "", "text")}
            {pick_row(move || tr().rig_role_scope, trn, |t| t.scope.clone(), |t, v| t.scope = v,
                      move || scopes.with(|s| s.iter().map(|s| s.name.clone()).collect()))}
            {text_row(move || tr().rig_reducer, reducer, String::clone, |r, v| *r = v, "1.0", "decimal")}
            {TRAIN_ROLES.map(|(label, bit, get, set)| pick_row(move || label(tr()), trn, get, set, move || {
                devices.with(|d| d.iter().filter(|d| d.interface & bit != 0).map(|d| d.name.clone()).collect())
            }))}
        </div>
    };
    editor_sheet(title, editor, lang, cards, can_delete, delete, invalid, save)
}

fn scope_sheet(
    orig: Option<ScopeInfo>,
    editor: RwSignal<Option<Editor>>,
    send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let title: LabelFn = if orig.is_none() { |t| t.rig_new_scope } else { |t| t.rig_edit_scope };
    let is_new = orig.is_none();
    let sc = orig.unwrap_or_else(|| ScopeInfo { type_: SCOPE_TYPES[0].into(), ..Default::default() });
    let id = sc.id.clone();
    // Kept as text so "4.5" can be typed; parsed on save.
    let mm = |v: f64| if v > 0.0 { v.to_string() } else { String::new() };
    let fl = RwSignal::new(mm(sc.focal_length_mm));
    let ap = RwSignal::new(mm(sc.aperture_mm));
    let sc = RwSignal::new(sc);
    let num = |v: RwSignal<String>| v.with(|v| v.trim().parse::<f64>().unwrap_or(0.0));

    let invalid = move || sc.with(|s| s.model.trim().is_empty()) || num(fl) <= 0.0;
    let save = {
        let (send, id) = (send.clone(), id.clone());
        move || {
            if invalid() {
                return;
            }
            let mut payload = sc.with_untracked(|s| json!({
                "model":        s.model.trim(),
                "vendor":       s.vendor.trim(),
                "type":         s.type_,
                "focal_length": num(fl),
                "aperture":     num(ap),
            }));
            if !is_new {
                payload["id"] = json!(id);
            }
            send_cmd(&send, if is_new { "scope_add" } else { "scope_update" }, payload);
            editor.set(None);
        }
    };
    let delete = move || {
        if confirm(t(lang.get_untracked()).rig_confirm_delete_scope) {
            send_cmd(&send, "scope_delete", json!({ "id": id }));
            editor.set(None);
        }
    };

    let cards = view! {
        <div class=CARD>
            {text_row(move || tr().rig_scope_vendor, sc, |s| s.vendor.clone(), |s, v| s.vendor = v, "", "text")}
            {text_row(move || tr().rig_scope_model, sc, |s| s.model.clone(), |s, v| s.model = v, "", "text")}
            {pills_row(move || tr().rig_scope_type, sc, |s, ty| s.type_ == ty, |s, ty| s.type_ = ty.into(),
                       SCOPE_TYPES.iter().map(|ty| (*ty, *ty)).collect())}
            {text_row(move || tr().rig_scope_fl, fl, String::clone, |f, v| *f = v, "600", "decimal")}
            {text_row(move || tr().rig_scope_aperture, ap, String::clone, |a, v| *a = v, "120", "decimal")}
        </div>
    };
    editor_sheet(title, editor, lang, cards, (!is_new).then_some(true), delete, invalid, save)
}

/// An editor `sheet`: scrolling cards over a footer with Delete — shown for an
/// existing item (`can_delete` is `Some`), enabled when it holds `true` — and
/// Save, disabled while the form is `invalid`.
#[allow(clippy::too_many_arguments)]
fn editor_sheet(
    title: LabelFn,
    editor: RwSignal<Option<Editor>>,
    lang: RwSignal<Lang>,
    cards: impl IntoView,
    can_delete: Option<bool>,
    delete: impl Fn() + Send + Sync + 'static,
    invalid: impl Fn() -> bool + Send + Sync + 'static,
    save: impl Fn() + Send + Sync + 'static,
) -> impl IntoView {
    let tr = move || t(lang.get());
    sheet(move || title(tr()), move || editor.set(None), view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            {cards}
        </div>
        <div class=FOOTER>
            {can_delete.map(|enabled| view! {
                <button class="btn btn-danger h-11 px-4" disabled=!enabled on:click=move |_| delete()>
                    {move || tr().profiles_delete}
                </button>
            })}
            <button class="btn btn-primary h-11 px-5 font-semibold ml-auto max-md:flex-1"
                    disabled=invalid on:click=move |_| save()>
                {move || tr().profiles_save}
            </button>
        </div>
    })
}

// ── Draft-bound rows ──────────────────────────────────────────────────────────
//
// Each editor keeps its item in one `RwSignal`; a row reads and writes one
// field of it through a `get` / `set` pair.

fn text_row<T: Send + Sync + 'static>(
    label: impl Fn() -> &'static str + Send + 'static,
    draft: RwSignal<T>,
    get: Getter<T>,
    set: Setter<T>,
    placeholder: &'static str,
    inputmode: &'static str,
) -> impl IntoView {
    setting_row(label, view! {
        <input type="text" class=TEXT inputmode=inputmode placeholder=placeholder
               prop:value=move || draft.with(get)
               on:input=move |ev| {
                   let v = event_target_value(&ev);
                   draft.update(|d| set(d, v));
               } />
    })
}

/// A select over `options`. "" and "--" both mean none; a current value
/// missing from the options stays listed, so saving never silently drops it.
fn pick_row<T: Send + Sync + 'static>(
    label: impl Fn() -> &'static str + Send + 'static,
    draft: RwSignal<T>,
    get: Getter<T>,
    set: Setter<T>,
    options: impl Fn() -> Vec<String> + Send + Sync + 'static,
) -> impl IntoView {
    // Its own memo, so typing in another field doesn't rebuild the options.
    let cur = Memo::new(move |_| draft.with(get));
    setting_row(label, view! {
        <select class=SELECT on:change=move |ev| {
            let v = event_target_value(&ev);
            draft.update(|d| set(d, v));
        }>
            {move || {
                let cur = cur.get();
                let none = cur.is_empty() || cur == "--";
                let mut list = options();
                list.retain(|s| !s.is_empty());
                list.sort();
                list.dedup();
                let mut opts = vec![(String::new(), "--".to_string())];
                if !none && !list.contains(&cur) {
                    opts.push((cur.clone(), format!("{cur} (missing)")));
                }
                opts.extend(list.into_iter().map(|s| (s.clone(), s)));
                opts.into_iter().map(|(v, text)| {
                    let selected = if v.is_empty() { none } else { v == cur };
                    view! { <option value=v selected=selected>{text}</option> }
                }).collect_view()
            }}
        </select>
    })
}

fn flag_row<T: Send + Sync + 'static>(
    label: impl Fn() -> &'static str + Send + 'static,
    draft: RwSignal<T>,
    get: fn(&T) -> bool,
    set: fn(&mut T, bool),
) -> impl IntoView {
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || draft.with(get)
                   on:change=move |ev| {
                       let v = event_target_checked(&ev);
                       draft.update(|d| set(d, v));
                   } />
            <span class=LABEL>{move || label()}</span>
        </label>
    }
}

/// One-of-N pills under their label (mode, guiding, telescope type).
fn pills_row<T: Send + Sync + 'static, V: Copy + Send + Sync + 'static>(
    label: impl Fn() -> &'static str + Send + 'static,
    draft: RwSignal<T>,
    is: fn(&T, V) -> bool,
    set: fn(&mut T, V),
    options: Vec<(V, &'static str)>,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-1.5 py-1">
            <span class=LABEL>{move || label()}</span>
            <div class="flex flex-wrap gap-1.5">
                {options.into_iter().map(|(v, text)| {
                    let on = move || draft.with(|d| is(d, v));
                    view! {
                        <button type="button"
                                class=move || if on() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                                aria-pressed=move || on().to_string()
                                on:click=move |_| draft.update(|d| set(d, v))>
                            {text}
                        </button>
                    }
                }).collect_view()}
            </div>
        </div>
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn port_text(port: u16) -> String {
    if port == 0 { String::new() } else { port.to_string() }
}

/// Digits only, so a stray letter is dropped rather than clearing the field.
fn parse_port(v: &str) -> u16 {
    v.chars().filter(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
}

/// KStars sends nothing back after a `train_*` mutation — re-fetch.
fn refetch_trains(send: &SendCmd) {
    send_cmd(send, "train_get_all", json!({}));
    send_cmd(send, "train_get_profiles", json!({}));
}

fn confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.confirm_with_message(message).ok())
        .unwrap_or(false)
}
