//! Equipment profile manager — CRUD over Ekos equipment profiles plus
//! launch/stop. Available before Ekos is online; profile_* commands are
//! dispatched in KStars before the Ekos-startup gate (message.cpp:249).
//!
//! Inbound:  `get_profiles` ({selectedProfile, profiles[]})
//! Outbound: `get_profiles`, `profile_add`, `profile_update`,
//!           `profile_delete`, `profile_start`, `profile_stop`

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::Event;

use crate::i18n::{t, Lang};
use crate::ws::{DeviceInfo, DriverInfo, OpticalTrain, ProfileInfo, ScopeInfo, SendCmd};

const GUIDING_LABELS: [&str; 4] = ["Internal", "PHD2", "LinGuider", "SEP"];

// `family` strings as KStars emits them — see indicommon.h:143 (DeviceFamilyLabels).
const FAM_TELESCOPE: &str = "Telescopes";
const FAM_CCD: &str = "CCDs";
const FAM_FOCUSER: &str = "Focusers";
const FAM_FILTER: &str = "Filter Wheels";
const FAM_AO: &str = "Adaptive Optics";
const FAM_DOME: &str = "Domes";
const FAM_WEATHER: &str = "Weather";
const AUX_FAMILIES: &[&str] = &[
    "Auxiliary",
    "Spectrographs",
    "Detectors",
    "Rotators",
    "Power",
];

// Class names for row-action buttons. Visual primitives live in base.css;
// state intent is expressed via `.btn-{primary,danger,ghost}` modifiers.
const BTN_BASE: &str = "btn btn--sm";
const BTN_LAUNCH: &str = "btn-primary";
const BTN_STOP: &str = "btn-danger";
const BTN_STARTING: &str = "btn-ghost text-state-warn";
const BTN_EDIT: &str = "btn-ghost";
const BTN_DELETE: &str = "btn-danger";

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

    // Editor state. None = list view; Some = inline form for add (empty name)
    // or edit (existing name preloaded).
    let editing = RwSignal::new(None::<EditorState>);
    // Name of the row pending delete confirmation, or None.
    let confirm_delete = RwSignal::new(None::<String>);
    // Set when launching to drive the "Starting…" indicator. Cleared when
    // `online` flips true or after a 30s safety timeout.
    let starting = RwSignal::new(None::<String>);

    // Auto-clear `starting` once Ekos comes online.
    Effect::new(move |_| {
        if online.get() {
            starting.set(None);
        }
    });

    let send_arc = send;

    let on_add = {
        let editing = editing;
        move |_| editing.set(Some(EditorState::new_blank()))
    };

    let on_refresh_send = send_arc.clone();
    let on_refresh = move |_| {
        on_refresh_send(r#"{"type":"get_profiles","payload":{}}"#.to_string());
        on_refresh_send(r#"{"type":"get_drivers","payload":{}}"#.to_string());
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono overflow-auto pt-5 pr-5 pb-5 pl-20 max-[759px]:pl-16 max-[759px]:pr-3 max-[759px]:pt-3 max-[759px]:pb-3">
            <div class="flex flex-wrap items-center gap-3 mb-4 max-[759px]:gap-sp-2">
                <h2 class="m-0 text-base text-text-blue tracking-[0.08em]">
                    {move || tr().profiles_title}
                </h2>
                <span class="text-[#556] text-sm">
                    {move || format!("({})", profiles.get().len())}
                </span>
                <span class="flex-1"></span>
                <button class="btn btn-primary" on:click=on_add>
                    "+ " {move || tr().profiles_add}
                </button>
                <button
                    class="btn-icon"
                    on:click=on_refresh
                    title=move || if connected.get() { "" } else { "WS not connected" }
                >
                    "↻"
                </button>
            </div>

            // Applications section
            <div class="border border-border-base rounded-lg pt-sp-3 pr-sp-4 pb-sp-3 pl-sp-4 mb-sp-4 bg-[rgba(12,14,24,0.6)]">
                <div class="text-xs text-text-blue font-semibold tracking-[0.08em] mb-sp-3 uppercase">
                    {move || tr().apps_section}
                </div>
                <div class="flex flex-col gap-sp-2">
                    <AppRow
                        label=move || tr().apps_kstars
                        running=kstars_running
                        app_name="kstars"
                    />
                    <AppRow
                        label=move || tr().apps_phd2
                        running=phd2_running
                        app_name="phd2"
                    />
                </div>
            </div>

            // Rig (optical-train) manager — operates on the active profile.
            {
                let send_for_rig = send_arc.clone();
                view! {
                    <RigSection
                        online=online
                        selected_profile=selected_profile
                        devices=devices
                        optical_trains=optical_trains
                        scopes=scopes
                        module_trains=module_trains
                        send=send_for_rig
                    />
                }
            }

            // Editor (inline panel above the list)
            {
                let send_for_form = send_arc.clone();
                move || editing.get().map(|_| {
                    view! {
                        <ProfileForm
                            editing=editing
                            profiles=profiles
                            drivers=drivers
                            send=send_for_form.clone()
                        />
                    }
                })
            }

            // List
            {
                let send_for_list = send_arc.clone();
                move || {
                    let list = profiles.get();
                    if list.is_empty() {
                        view! {
                            <div class="p-10 text-center text-[#556] border border-dashed border-border-base rounded-lg">
                                {tr().profiles_empty}
                            </div>
                        }.into_any()
                    } else {
                        let send_for_rows = send_for_list.clone();
                        view! {
                            <div class="flex flex-col gap-sp-2">
                                {list.into_iter().map(|p| {
                                    view! {
                                        <ProfileRow
                                            profile=p
                                            selected_profile=selected_profile
                                            online=online
                                            editing=editing
                                            confirm_delete=confirm_delete
                                            starting=starting
                                            send=send_for_rows.clone()
                                        />
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    }
                }
            }

            // Delete-confirm modal
            {
                let send_for_del = send_arc.clone();
                move || confirm_delete.get().map(|name| {
                    let name_for_yes = name.clone();
                    let send_yes = send_for_del.clone();
                    view! {
                        <div class="fixed inset-0 bg-black/60 z-[100] flex items-center justify-center"
                             on:click=move |_| confirm_delete.set(None)
                        >
                            <div class="bg-[#0a0a14] border border-[#5a2a2a] rounded-lg py-sp-5 px-[22px] min-w-[280px]"
                                 on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                            >
                                <div class="text-[12px] text-[#ee9999] mb-sp-2">
                                    {tr().profiles_confirm_delete}
                                </div>
                                <div class="text-md text-text-dim mb-sp-4">{name.clone()}</div>
                                <div class="flex gap-sp-3 justify-end">
                                    <button
                                        class=format!("{BTN_BASE} {BTN_EDIT}")
                                        on:click=move |_| confirm_delete.set(None)
                                    >{tr().profiles_cancel}</button>
                                    <button
                                        class=format!("{BTN_BASE} {BTN_DELETE}")
                                        on:click=move |_| {
                                            let payload = serde_json::json!({"name": name_for_yes});
                                            send_yes(serde_json::json!({
                                                "type": "profile_delete",
                                                "payload": payload,
                                            }).to_string());
                                            confirm_delete.set(None);
                                        }
                                    >{tr().profiles_delete}</button>
                                </div>
                            </div>
                        </div>
                    }
                })
            }
        </div>
    }
}

// ── AppRow ───────────────────────────────────────────────────────────────────

/// One row in the "Applications" section.  Renders the app name, a running/
/// stopped badge, and a Launch / Stop button.  Button clicks go directly to
/// the junos-server REST API; status updates come back via the `/ws` push.
#[component]
fn AppRow(
    /// Display label (e.g. "KStars").
    label: impl Fn() -> &'static str + Send + Sync + 'static,
    /// Signal tracking whether the app is currently running.
    running: RwSignal<bool>,
    /// Lowercase app name used in the API request body ("kstars" | "phd2").
    app_name: &'static str,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let on_action = move |_| {
        let currently_running = running.get_untracked();
        let endpoint = if currently_running {
            "/api/apps/stop"
        } else {
            "/api/apps/launch"
        };
        let body = serde_json::json!({ "app": app_name }).to_string();
        // Fire-and-forget fetch; status comes back via ws push.
        let _ = web_sys::window().map(|w| {
            use wasm_bindgen::JsValue;
            let opts = web_sys::RequestInit::new();
            opts.set_method("POST");
            opts.set_body(&JsValue::from_str(&body));
            let headers = web_sys::Headers::new().unwrap();
            let _ = headers.set("content-type", "application/json");
            opts.set_headers(&headers);
            let req = web_sys::Request::new_with_str_and_init(endpoint, &opts).unwrap();
            w.fetch_with_request(&req)
        });
    };

    view! {
        <div class="flex items-center gap-sp-3">
            <span class="text-sm text-text-dim font-semibold w-[72px] shrink-0">{label}</span>
            {move || if running.get() {
                view! {
                    <span class="badge badge--ok">{tr().apps_running}</span>
                }.into_any()
            } else {
                view! {
                    <span class="badge">{tr().apps_stopped}</span>
                }.into_any()
            }}
            <button
                class=move || {
                    if running.get() {
                        format!("{BTN_BASE} {BTN_STOP}")
                    } else {
                        format!("{BTN_BASE} {BTN_LAUNCH}")
                    }
                }
                on:click=on_action
            >
                {move || if running.get() { tr().apps_stop } else { tr().apps_launch }}
            </button>
        </div>
    }
}

// ── ProfileRow ───────────────────────────────────────────────────────────────

#[component]
fn ProfileRow(
    profile: ProfileInfo,
    selected_profile: RwSignal<Option<String>>,
    online: RwSignal<bool>,
    editing: RwSignal<Option<EditorState>>,
    confirm_delete: RwSignal<Option<String>>,
    starting: RwSignal<Option<String>>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let name = profile.name.clone();
    let is_simulators = name == "Simulators";

    let is_active = {
        let n = name.clone();
        move || selected_profile.get().as_deref() == Some(n.as_str())
    };
    let is_running = {
        let active = is_active.clone();
        move || active() && online.get()
    };
    let is_starting = {
        let n = name.clone();
        move || starting.get().as_deref() == Some(n.as_str())
    };
    let delete_disabled = {
        let n = name.clone();
        let running = is_running.clone();
        move || n == "Simulators" || running()
    };

    let mode = profile.mode.clone();
    let summary = device_summary(&profile);

    let on_edit = {
        let p = profile.clone();
        let editing = editing;
        move |_| editing.set(Some(EditorState::from_profile(&p)))
    };
    let on_delete = {
        let n = name.clone();
        let confirm_delete = confirm_delete;
        move |_| confirm_delete.set(Some(n.clone()))
    };
    let on_launch = {
        let n = name.clone();
        let send = send.clone();
        let starting = starting;
        let online_sig = online;
        move |_| {
            // If another session is online, ask first.
            if online_sig.get_untracked() {
                let msg = t(lang.get_untracked()).profiles_confirm_launch;
                let confirmed = web_sys::window()
                    .and_then(|w| w.confirm_with_message(msg).ok())
                    .unwrap_or(false);
                if !confirmed {
                    return;
                }
            }
            send(
                serde_json::json!({
                    "type": "profile_start",
                    "payload": {"name": n},
                })
                .to_string(),
            );
            starting.set(Some(n.clone()));
            // 30s safety: clear the spinner if `online` never flips.
            let n_for_timeout = n.clone();
            let cb = Closure::<dyn FnMut()>::new(move || {
                if starting.get_untracked().as_deref() == Some(n_for_timeout.as_str()) {
                    starting.set(None);
                }
            });
            if let Some(w) = web_sys::window() {
                let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    30_000,
                );
            }
            cb.forget();
        }
    };
    let on_stop = {
        let send = send.clone();
        move |_| {
            send(r#"{"type":"profile_stop","payload":{}}"#.to_string());
        }
    };

    let active_for_class = is_active.clone();

    view! {
        <div
            class=move || {
                let base = "flex items-center gap-3 py-sp-3 px-sp-4 border rounded-lg bg-[rgba(12,14,24,0.6)] max-[759px]:flex-col max-[759px]:items-stretch max-[759px]:gap-sp-2";
                if active_for_class() {
                    format!("{base} border-text-blue !bg-[rgba(60,90,160,0.10)]")
                } else {
                    format!("{base} border-border-base")
                }
            }
        >
            <div class="flex-1 min-w-0">
                <div class="flex items-center gap-sp-2">
                    <span class="text-md text-text-dim font-semibold">{name.clone()}</span>
                    {move || if is_active() {
                        view! {
                            <span class="badge badge--ok">
                                {tr().profiles_active}
                            </span>
                        }.into_any()
                    } else {
                        view! { <span></span> }.into_any()
                    }}
                    <span class="text-[#556] text-xs uppercase">
                        {mode.clone()}
                    </span>
                </div>
                <div class="text-sm text-[#778] mt-[3px] whitespace-nowrap overflow-hidden text-ellipsis">
                    {summary}
                </div>
            </div>
            <div class="flex gap-[6px] shrink-0 max-[759px]:flex-wrap max-[759px]:justify-end">
                {move || if is_running() {
                    view! {
                        <button
                            class=format!("{BTN_BASE} {BTN_STOP}")
                            on:click=on_stop.clone()
                        >{tr().profiles_stop}</button>
                    }.into_any()
                } else if is_starting() {
                    view! {
                        <span class=format!("{BTN_BASE} {BTN_STARTING}")>
                            {tr().profiles_starting}
                        </span>
                    }.into_any()
                } else {
                    view! {
                        <button
                            class=format!("{BTN_BASE} {BTN_LAUNCH}")
                            on:click=on_launch.clone()
                        >{tr().profiles_launch}</button>
                    }.into_any()
                }}
                <button
                    class=format!("{BTN_BASE} {BTN_EDIT}")
                    on:click=on_edit
                >{tr().profiles_edit}</button>
                <button
                    class=format!("{BTN_BASE} {BTN_DELETE}")
                    disabled=delete_disabled
                    title=move || if is_simulators { "Simulators is not deletable" } else { "" }
                    on:click=on_delete
                >{tr().profiles_delete}</button>
            </div>
        </div>
    }
}

// ── ProfileForm (add / edit) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct EditorState {
    /// Original name of the row being edited; empty when adding new.
    original_name: String,
    profile: ProfileInfo,
}

impl EditorState {
    fn new_blank() -> Self {
        let mut p = ProfileInfo::default();
        p.mode = "local".into();
        p.driver_source = "system".into();
        Self {
            original_name: String::new(),
            profile: p,
        }
    }
    fn from_profile(p: &ProfileInfo) -> Self {
        Self {
            original_name: p.name.clone(),
            profile: p.clone(),
        }
    }
    fn is_new(&self) -> bool {
        self.original_name.is_empty()
    }
}

#[component]
fn ProfileForm(
    editing: RwSignal<Option<EditorState>>,
    profiles: RwSignal<Vec<ProfileInfo>>,
    drivers: RwSignal<Vec<DriverInfo>>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Pull the editor state into individual signals for binding.
    let st = editing.get_untracked().expect("editor open");
    let is_new = st.is_new();

    let name = RwSignal::new(st.profile.name.clone());
    let mode = RwSignal::new(st.profile.mode.clone());
    let host = RwSignal::new(st.profile.remote_host.clone());
    let port = RwSignal::new(st.profile.remote_port.to_string());
    let auto_conn = RwSignal::new(st.profile.auto_connect);
    let port_sel = RwSignal::new(st.profile.port_selector);
    let guiding = RwSignal::new(st.profile.guiding);
    let g_host = RwSignal::new(st.profile.remote_guiding_host.clone());
    let g_port = RwSignal::new(st.profile.remote_guiding_port.to_string());
    let web_mgr = RwSignal::new(st.profile.use_web_manager);
    let mount = RwSignal::new(st.profile.mount.clone());
    let ccd = RwSignal::new(st.profile.ccd.clone());
    let guider_drv = RwSignal::new(st.profile.guider.clone());
    let focuser = RwSignal::new(st.profile.focuser.clone());
    let filter = RwSignal::new(st.profile.filter.clone());
    let ao = RwSignal::new(st.profile.ao.clone());
    let dome = RwSignal::new(st.profile.dome.clone());
    let weather = RwSignal::new(st.profile.weather.clone());
    let aux1 = RwSignal::new(st.profile.aux1.clone());
    let aux2 = RwSignal::new(st.profile.aux2.clone());
    let aux3 = RwSignal::new(st.profile.aux3.clone());
    let aux4 = RwSignal::new(st.profile.aux4.clone());
    let remote = RwSignal::new(st.profile.remote.clone());
    let ds = st.profile.driver_source.clone();
    let original = st.original_name.clone();

    let on_save = {
        let send = send.clone();
        move |_| {
            let n = name.get();
            if n.trim().is_empty() {
                return;
            }
            // Block name collision when adding new.
            if is_new && profiles.get_untracked().iter().any(|p| p.name == n) {
                return;
            }
            let p = ProfileInfo {
                name: n.clone(),
                auto_connect: auto_conn.get(),
                port_selector: port_sel.get(),
                mode: if mode.get().is_empty() {
                    "local".into()
                } else {
                    mode.get()
                },
                remote_host: host.get(),
                remote_port: port.get().parse().unwrap_or(0),
                guiding: guiding.get(),
                remote_guiding_host: g_host.get(),
                remote_guiding_port: g_port.get().parse().unwrap_or(0),
                use_web_manager: web_mgr.get(),
                mount: mount.get(),
                ccd: ccd.get(),
                guider: guider_drv.get(),
                focuser: focuser.get(),
                filter: filter.get(),
                ao: ao.get(),
                dome: dome.get(),
                weather: weather.get(),
                aux1: aux1.get(),
                aux2: aux2.get(),
                aux3: aux3.get(),
                aux4: aux4.get(),
                remote: remote.get(),
                driver_source: ds.clone(),
            };
            // Update keys the row by `name` — if the user renamed, KStars
            // would create a new profile; we don't support rename yet.
            let type_ = if is_new || original.is_empty() {
                "profile_add"
            } else if original != p.name {
                // Rename = delete-old + add-new. Do that explicitly.
                send(
                    serde_json::json!({
                        "type": "profile_delete",
                        "payload": {"name": original},
                    })
                    .to_string(),
                );
                "profile_add"
            } else {
                "profile_update"
            };
            send(
                serde_json::json!({
                    "type": type_,
                    "payload": p.to_json(),
                })
                .to_string(),
            );
            editing.set(None);
        }
    };

    let on_cancel = move |_| editing.set(None);

    let mode_for_remote = mode;
    let is_remote = move || mode_for_remote.get() == "remote";

    let select_cls = "input input--sm";

    view! {
        <div class="border border-[#3a3a5a] rounded-lg pt-sp-4 pr-4 pb-sp-4 pl-4 mb-sp-4 bg-[rgba(20,24,40,0.4)]">
            <div class="flex items-center gap-sp-3 mb-sp-3">
                <span class="text-[12px] text-text-blue font-semibold tracking-[0.06em]">
                    {move || if is_new { tr().profiles_new } else { tr().profiles_edit }}
                </span>
            </div>

            // Row 1: name + mode + auto_connect + port_selector + web_mgr
            <div class="flex flex-wrap gap-y-sp-3 gap-x-sp-5 mb-sp-3">
                <Field label=tr().profiles_name>
                    <TextInput value=name placeholder="Name"/>
                </Field>
                <Field label=tr().profiles_mode>
                    <select
                        class=select_cls
                        prop:value=move || mode.get()
                        on:change=move |ev: Event| {
                            if let Some(t) = ev.target() {
                                if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                                    mode.set(sel.value());
                                }
                            }
                        }
                    >
                        <option value="local">{tr().profiles_mode_local}</option>
                        <option value="remote">{tr().profiles_mode_remote}</option>
                    </select>
                </Field>
                <Field label=tr().profiles_auto_connect>
                    <CheckBox value=auto_conn/>
                </Field>
                <Field label=tr().profiles_port_selector>
                    <CheckBox value=port_sel/>
                </Field>
                <Field label=tr().profiles_web_manager>
                    <CheckBox value=web_mgr/>
                </Field>
                <Field label=tr().profiles_guiding>
                    <select
                        class=select_cls
                        prop:value=move || guiding.get().to_string()
                        on:change=move |ev: Event| {
                            if let Some(t) = ev.target() {
                                if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                                    guiding.set(sel.value().parse().unwrap_or(0));
                                }
                            }
                        }
                    >
                        {GUIDING_LABELS.iter().enumerate().map(|(i, label)| {
                            view! { <option value=i.to_string()>{*label}</option> }
                        }).collect_view()}
                    </select>
                </Field>
            </div>

            // Remote section (visible only when mode == remote)
            <Show when=is_remote>
                <div class="flex flex-wrap gap-y-sp-3 gap-x-sp-5 mb-sp-3">
                    <Field label=tr().profiles_host>
                        <TextInput value=host placeholder="localhost"/>
                    </Field>
                    <Field label=tr().profiles_port>
                        <TextInput value=port placeholder="7624"/>
                    </Field>
                    <Field label="Guiding host">
                        <TextInput value=g_host placeholder=""/>
                    </Field>
                    <Field label="Guiding port">
                        <TextInput value=g_port placeholder=""/>
                    </Field>
                </div>
            </Show>

            // Drivers section
            <div class="text-sm text-text-blue font-semibold mt-sp-1 mb-[6px] tracking-[0.06em]">
                {move || tr().profiles_drivers}
            </div>
            <div class="grid grid-cols-[repeat(auto-fill,minmax(180px,1fr))] gap-y-sp-2 gap-x-sp-4 mb-sp-3">
                <DriverSelect label="Mount"   value=mount      drivers=drivers families=&[FAM_TELESCOPE]/>
                <DriverSelect label="CCD"     value=ccd        drivers=drivers families=&[FAM_CCD]/>
                <DriverSelect label="Guider"  value=guider_drv drivers=drivers families=&[FAM_CCD]/>
                <DriverSelect label="Focuser" value=focuser    drivers=drivers families=&[FAM_FOCUSER]/>
                <DriverSelect label="Filter"  value=filter     drivers=drivers families=&[FAM_FILTER]/>
                <DriverSelect label="AO"      value=ao         drivers=drivers families=&[FAM_AO]/>
                <DriverSelect label="Dome"    value=dome       drivers=drivers families=&[FAM_DOME]/>
                <DriverSelect label="Weather" value=weather    drivers=drivers families=&[FAM_WEATHER]/>
                <DriverSelect label="Aux 1"   value=aux1       drivers=drivers families=AUX_FAMILIES/>
                <DriverSelect label="Aux 2"   value=aux2       drivers=drivers families=AUX_FAMILIES/>
                <DriverSelect label="Aux 3"   value=aux3       drivers=drivers families=AUX_FAMILIES/>
                <DriverSelect label="Aux 4"   value=aux4       drivers=drivers families=AUX_FAMILIES/>
            </div>
            <Field label=tr().profiles_remote_drivers>
                <TextInput value=remote placeholder="indi_eqmod_telescope,..."/>
            </Field>

            <div class="flex flex-wrap gap-sp-3 justify-end mt-sp-4">
                <button
                    class=format!("{BTN_BASE} {BTN_EDIT}")
                    on:click=on_cancel
                >{tr().profiles_cancel}</button>
                <button
                    class=format!("{BTN_BASE} {BTN_LAUNCH}")
                    on:click=on_save
                >{tr().profiles_save}</button>
            </div>
        </div>
    }
}

#[component]
fn Field(label: &'static str, children: Children) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-[3px] min-w-[120px]">
            <span class="text-xs text-[#778] tracking-[0.05em]">{label}</span>
            {children()}
        </div>
    }
}

#[component]
fn DriverSelect(
    label: &'static str,
    value: RwSignal<String>,
    drivers: RwSignal<Vec<DriverInfo>>,
    families: &'static [&'static str],
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-[3px] min-w-0">
            <span class="text-xs text-[#778] tracking-[0.05em]">{label}</span>
            <select
                class="input input--sm w-full min-w-0"
                on:change=move |ev: Event| {
                    if let Some(t) = ev.target() {
                        if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                            value.set(sel.value());
                        }
                    }
                }
            >
                <option value="" selected=move || value.get().is_empty()>"--"</option>
                {move || {
                    // Filter installed drivers by family, sort by label.
                    let mut list: Vec<String> = drivers.get().into_iter()
                        .filter(|d| families.contains(&d.family.as_str()))
                        .map(|d| d.label)
                        .filter(|l| !l.is_empty())
                        .collect();
                    list.sort();
                    list.dedup();

                    // If the current value isn't in the filtered list (e.g.
                    // an old profile referencing an uninstalled driver, or
                    // the family-filter excludes it), keep it as a sticky
                    // option so save doesn't silently drop it.
                    let current = value.get();
                    let sticky = if !current.is_empty() && !list.iter().any(|l| l == &current) {
                        Some(current)
                    } else {
                        None
                    };

                    view! {
                        {sticky.map(|s| {
                            let v = s.clone();
                            view! {
                                <option
                                    value=v
                                    selected=move || value.get() == s
                                >{format!("{s} (missing)")}</option>
                            }
                        })}
                        {list.into_iter().map(|l| {
                            let v = l.clone();
                            let sel_l = l.clone();
                            view! {
                                <option
                                    value=v
                                    selected=move || value.get() == sel_l
                                >{l}</option>
                            }
                        }).collect_view()}
                    }
                }}
            </select>
        </div>
    }
}

#[component]
fn TextInput(
    value: RwSignal<String>,
    #[prop(optional, into)] placeholder: String,
) -> impl IntoView {
    view! {
        <input
            class="input input--sm w-full min-w-0"
            type="text"
            prop:value=move || value.get()
            placeholder=placeholder
            on:input=move |ev: Event| {
                if let Some(t) = ev.target() {
                    if let Ok(inp) = t.dyn_into::<web_sys::HtmlInputElement>() {
                        value.set(inp.value());
                    }
                }
            }
        />
    }
}

#[component]
fn CheckBox(value: RwSignal<bool>) -> impl IntoView {
    view! {
        <input
            type="checkbox"
            prop:checked=move || value.get()
            on:change=move |ev: Event| {
                if let Some(t) = ev.target() {
                    if let Ok(inp) = t.dyn_into::<web_sys::HtmlInputElement>() {
                        value.set(inp.checked());
                    }
                }
            }
        />
    }
}

fn device_summary(p: &ProfileInfo) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for s in [&p.mount, &p.ccd, &p.guider, &p.focuser, &p.filter] {
        let s = s.trim();
        if !s.is_empty() && s != "--" {
            parts.push(s);
        }
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

// ── Rig (optical-train) manager ───────────────────────────────────────────────
//
// Trains are per-profile and only exposed for the *running* profile
// (train_get_all is filtered to the active profile id, and train_* mutations
// require Ekos started — message.cpp:264). So this whole section gates on
// `online`. Scopes (telescope DB) are global and editable regardless, but we
// surface them here since trains reference them.

// libindi driver-interface bits (basedevice.h) for per-role device filtering.
const IFACE_TELESCOPE: i64 = 1 << 0;
const IFACE_CCD:       i64 = 1 << 1;
const IFACE_FOCUSER:   i64 = 1 << 3;
const IFACE_FILTER:    i64 = 1 << 4;
const IFACE_ROTATOR:   i64 = 1 << 12;

#[derive(Clone)]
struct TrainEdit {
    is_new: bool,
    train: OpticalTrain,
}

#[derive(Clone)]
struct ScopeEdit {
    is_new: bool,
    scope: ScopeInfo,
}

#[component]
fn RigSection(
    online: RwSignal<bool>,
    selected_profile: RwSignal<Option<String>>,
    devices: RwSignal<Vec<DeviceInfo>>,
    optical_trains: RwSignal<Vec<OpticalTrain>>,
    scopes: RwSignal<Vec<ScopeInfo>>,
    module_trains: RwSignal<serde_json::Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let train_editing = RwSignal::new(None::<TrainEdit>);
    let scope_editing = RwSignal::new(None::<ScopeEdit>);
    let confirm_train = RwSignal::new(None::<String>); // train name
    let confirm_scope = RwSignal::new(None::<(String, String)>); // (id, display)

    let on_add_train = move |_| {
        train_editing.set(Some(TrainEdit {
            is_new: true,
            train: OpticalTrain::default(),
        }))
    };
    let on_add_scope = move |_| {
        scope_editing.set(Some(ScopeEdit {
            is_new: true,
            scope: ScopeInfo::default(),
        }))
    };

    let modules_send = send.clone();
    let train_form_send = send.clone();
    let scope_form_send = send.clone();
    let confirm_train_send = send.clone();
    let confirm_scope_send = send.clone();

    view! {
        <div class="border border-border-base rounded-lg pt-sp-3 pr-sp-4 pb-sp-3 pl-sp-4 mb-sp-4 bg-[rgba(12,14,24,0.6)]">
            <div class="flex flex-wrap items-center gap-sp-3 mb-sp-3">
                <span class="text-xs text-text-blue font-semibold tracking-[0.08em] uppercase">
                    {move || tr().rig_section}
                </span>
                {move || selected_profile.get().map(|p| view! {
                    <span class="text-[#556] text-xs uppercase">{p}</span>
                })}
                <span class="flex-1"></span>
                <button
                    class="btn btn--sm btn-primary"
                    disabled=move || !online.get()
                    on:click=on_add_train
                >"+ " {move || tr().rig_add_train}</button>
            </div>

            {move || if !online.get() {
                view! {
                    <div class="p-6 text-center text-[#556] text-sm border border-dashed border-border-base rounded-lg">
                        {tr().rig_offline_hint}
                    </div>
                }.into_any()
            } else {
                let modules_send = modules_send.clone();
                let train_form_send = train_form_send.clone();
                let scope_form_send = scope_form_send.clone();
                view! {
                    // Module assignment (only meaningful with ≥1 train)
                    {move || (!optical_trains.get().is_empty()).then(|| {
                        view! {
                            <div class="text-sm text-text-blue font-semibold mb-[6px] tracking-[0.06em]">
                                {tr().rig_modules_section}
                            </div>
                            <div class="mb-sp-3">
                                <ModuleAssign
                                    optical_trains=optical_trains
                                    module_trains=module_trains
                                    send=modules_send.clone()
                                />
                            </div>
                        }
                    })}

                    // Train editor (inline)
                    {
                        let train_form_send = train_form_send.clone();
                        move || train_editing.get().map(|_| view! {
                            <TrainForm
                                train_editing=train_editing
                                devices=devices
                                scopes=scopes
                                send=train_form_send.clone()
                            />
                        })
                    }

                    // Train list
                    {
                        move || {
                            let list = optical_trains.get();
                            if list.is_empty() {
                                view! {
                                    <div class="p-4 text-center text-[#556] text-sm">
                                        {tr().rig_no_trains}
                                    </div>
                                }.into_any()
                            } else {
                                view! {
                                    <div class="flex flex-col gap-sp-2">
                                        {list.into_iter().map(|trn| {
                                            view! {
                                                <TrainRow
                                                    train=trn
                                                    train_editing=train_editing
                                                    confirm_train=confirm_train
                                                />
                                            }
                                        }).collect_view()}
                                    </div>
                                }.into_any()
                            }
                        }
                    }

                    // Scopes sub-panel
                    <div class="flex items-center gap-sp-3 mt-sp-4 mb-[6px]">
                        <span class="text-sm text-text-blue font-semibold tracking-[0.06em]">
                            {tr().rig_scopes_section}
                        </span>
                        <span class="flex-1"></span>
                        <button class="btn btn--sm btn-ghost" on:click=on_add_scope>
                            "+ " {tr().rig_add_scope}
                        </button>
                    </div>
                    {
                        let scope_form_send = scope_form_send.clone();
                        move || scope_editing.get().map(|_| view! {
                            <ScopeForm scope_editing=scope_editing send=scope_form_send.clone()/>
                        })
                    }
                    {
                        move || {
                            let list = scopes.get();
                            if list.is_empty() {
                                view! {
                                    <div class="p-3 text-center text-[#556] text-sm">{tr().rig_no_scopes}</div>
                                }.into_any()
                            } else {
                                view! {
                                    <div class="flex flex-col gap-[6px]">
                                        {list.into_iter().map(|sc| {
                                            view! {
                                                <ScopeRow
                                                    scope=sc
                                                    scope_editing=scope_editing
                                                    confirm_scope=confirm_scope
                                                />
                                            }
                                        }).collect_view()}
                                    </div>
                                }.into_any()
                            }
                        }
                    }
                }.into_any()
            }}

            // Delete-confirm modal (train)
            {
                let confirm_train_send = confirm_train_send.clone();
                move || confirm_train.get().map(|name| {
                    let name_yes = name.clone();
                    let send_yes = confirm_train_send.clone();
                    view! {
                        <div class="fixed inset-0 bg-black/60 z-[100] flex items-center justify-center"
                             on:click=move |_| confirm_train.set(None)>
                            <div class="bg-[#0a0a14] border border-[#5a2a2a] rounded-lg py-sp-5 px-[22px] min-w-[280px]"
                                 on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                                <div class="text-[12px] text-[#ee9999] mb-sp-2">{tr().rig_confirm_delete_train}</div>
                                <div class="text-md text-text-dim mb-sp-4">{name.clone()}</div>
                                <div class="flex gap-sp-3 justify-end">
                                    <button class=format!("{BTN_BASE} {BTN_EDIT}")
                                            on:click=move |_| confirm_train.set(None)>{tr().profiles_cancel}</button>
                                    <button class=format!("{BTN_BASE} {BTN_DELETE}")
                                            on:click=move |_| {
                                                send_yes(serde_json::json!({"type":"train_delete","payload":{"name": name_yes}}).to_string());
                                                send_yes(r#"{"type":"train_get_all","payload":{}}"#.to_string());
                                                send_yes(r#"{"type":"train_get_profiles","payload":{}}"#.to_string());
                                                confirm_train.set(None);
                                            }>{tr().profiles_delete}</button>
                                </div>
                            </div>
                        </div>
                    }
                })
            }

            // Delete-confirm modal (scope)
            {
                let confirm_scope_send = confirm_scope_send.clone();
                move || confirm_scope.get().map(|(id, disp)| {
                    let send_yes = confirm_scope_send.clone();
                    view! {
                        <div class="fixed inset-0 bg-black/60 z-[100] flex items-center justify-center"
                             on:click=move |_| confirm_scope.set(None)>
                            <div class="bg-[#0a0a14] border border-[#5a2a2a] rounded-lg py-sp-5 px-[22px] min-w-[280px]"
                                 on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()>
                                <div class="text-[12px] text-[#ee9999] mb-sp-2">{tr().rig_confirm_delete_scope}</div>
                                <div class="text-md text-text-dim mb-sp-4">{disp}</div>
                                <div class="flex gap-sp-3 justify-end">
                                    <button class=format!("{BTN_BASE} {BTN_EDIT}")
                                            on:click=move |_| confirm_scope.set(None)>{tr().profiles_cancel}</button>
                                    <button class=format!("{BTN_BASE} {BTN_DELETE}")
                                            on:click=move |_| {
                                                send_yes(serde_json::json!({"type":"scope_delete","payload":{"id": id}}).to_string());
                                                confirm_scope.set(None);
                                            }>{tr().profiles_delete}</button>
                                </div>
                            </div>
                        </div>
                    }
                })
            }
        </div>
    }
}

#[component]
fn TrainRow(
    train: OpticalTrain,
    train_editing: RwSignal<Option<TrainEdit>>,
    confirm_train: RwSignal<Option<String>>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let name = train.name.clone();
    let summary = train_summary(&train);
    let on_edit = {
        let train = train.clone();
        move |_| train_editing.set(Some(TrainEdit { is_new: false, train: train.clone() }))
    };
    let on_delete = {
        let n = name.clone();
        move |_| confirm_train.set(Some(n.clone()))
    };

    view! {
        <div class="flex items-center gap-3 py-sp-2 px-sp-3 border border-border-base rounded-lg bg-[rgba(12,14,24,0.4)] max-[759px]:flex-col max-[759px]:items-stretch max-[759px]:gap-sp-2">
            <div class="flex-1 min-w-0">
                <span class="text-md text-text-dim font-semibold">{name.clone()}</span>
                <div class="text-sm text-[#778] mt-[2px] whitespace-nowrap overflow-hidden text-ellipsis">{summary}</div>
            </div>
            <div class="flex gap-[6px] shrink-0 max-[759px]:justify-end">
                <button class=format!("{BTN_BASE} {BTN_EDIT}") on:click=on_edit>{tr().profiles_edit}</button>
                <button class=format!("{BTN_BASE} {BTN_DELETE}") on:click=on_delete>{tr().profiles_delete}</button>
            </div>
        </div>
    }
}

#[component]
fn ScopeRow(
    scope: ScopeInfo,
    scope_editing: RwSignal<Option<ScopeEdit>>,
    confirm_scope: RwSignal<Option<(String, String)>>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let display = if scope.name.is_empty() {
        format!("{} {}", scope.vendor, scope.model).trim().to_string()
    } else {
        scope.name.clone()
    };
    let specs = format!("{:.0}mm f/{:.1}", scope.focal_length_mm,
        if scope.aperture_mm > 0.0 { scope.focal_length_mm / scope.aperture_mm } else { 0.0 });
    let on_edit = {
        let scope = scope.clone();
        move |_| scope_editing.set(Some(ScopeEdit { is_new: false, scope: scope.clone() }))
    };
    let on_delete = {
        let id = scope.id.clone();
        let disp = display.clone();
        move |_| confirm_scope.set(Some((id.clone(), disp.clone())))
    };

    view! {
        <div class="flex items-center gap-3 py-[6px] px-sp-3 border border-border-base rounded-lg bg-[rgba(12,14,24,0.4)]">
            <div class="flex-1 min-w-0">
                <span class="text-sm text-text-dim">{display}</span>
                <span class="text-xs text-[#556] ml-sp-2">{specs}</span>
            </div>
            <div class="flex gap-[6px] shrink-0">
                <button class=format!("{BTN_BASE} {BTN_EDIT}") on:click=on_edit>{tr().profiles_edit}</button>
                <button class=format!("{BTN_BASE} {BTN_DELETE}") on:click=on_delete>{tr().profiles_delete}</button>
            </div>
        </div>
    }
}

#[component]
fn TrainForm(
    train_editing: RwSignal<Option<TrainEdit>>,
    devices: RwSignal<Vec<DeviceInfo>>,
    scopes: RwSignal<Vec<ScopeInfo>>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let st = train_editing.get_untracked().expect("train editor open");
    let is_new = st.is_new;
    let id = st.train.id;

    let name = RwSignal::new(st.train.name.clone());
    let mount = RwSignal::new(st.train.mount.clone());
    let camera = RwSignal::new(st.train.camera.clone());
    let scope = RwSignal::new(st.train.scope.clone());
    let guider = RwSignal::new(st.train.guider.clone());
    let focuser = RwSignal::new(st.train.focuser.clone());
    let filterwheel = RwSignal::new(st.train.filterwheel.clone());
    let rotator = RwSignal::new(st.train.rotator.clone());
    let reducer = RwSignal::new(st.train.reducer.to_string());
    // Preserve fields we don't expose in the form.
    let dustcap = st.train.dustcap.clone();
    let lightbox = st.train.lightbox.clone();

    let on_save = {
        let send = send.clone();
        move |_| {
            let n = name.get();
            if n.trim().is_empty() {
                return;
            }
            let train = OpticalTrain {
                id,
                name: n,
                mount: mount.get(),
                camera: camera.get(),
                scope: scope.get(),
                guider: guider.get(),
                focuser: focuser.get(),
                filterwheel: filterwheel.get(),
                rotator: rotator.get(),
                dustcap: dustcap.clone(),
                lightbox: lightbox.clone(),
                reducer: reducer.get().parse().unwrap_or(1.0),
            };
            let type_ = if is_new { "train_add" } else { "train_update" };
            send(serde_json::json!({ "type": type_, "payload": train.to_json(!is_new) }).to_string());
            // KStars sends no auto-reply after train mutations — re-fetch.
            send(r#"{"type":"train_get_all","payload":{}}"#.to_string());
            send(r#"{"type":"train_get_profiles","payload":{}}"#.to_string());
            train_editing.set(None);
        }
    };
    let on_cancel = move |_| train_editing.set(None);

    view! {
        <div class="border border-[#3a3a5a] rounded-lg pt-sp-4 pr-4 pb-sp-4 pl-4 mb-sp-3 bg-[rgba(20,24,40,0.4)]">
            <div class="text-[12px] text-text-blue font-semibold tracking-[0.06em] mb-sp-3">
                {move || if is_new { tr().rig_new_train } else { tr().rig_edit_train }}
            </div>
            <div class="flex flex-wrap gap-y-sp-3 gap-x-sp-5 mb-sp-3">
                <Field label=tr().rig_train_name><TextInput value=name placeholder=""/></Field>
                <Field label=tr().rig_reducer><TextInput value=reducer placeholder="1.0"/></Field>
            </div>
            <div class="grid grid-cols-[repeat(auto-fill,minmax(180px,1fr))] gap-y-sp-2 gap-x-sp-4 mb-sp-3">
                <DeviceSelect label=tr().rig_role_mount value=mount devices=devices iface_bit=IFACE_TELESCOPE/>
                <DeviceSelect label=tr().rig_role_camera value=camera devices=devices iface_bit=IFACE_CCD/>
                <ScopeSelect label=tr().rig_role_scope value=scope scopes=scopes/>
                <DeviceSelect label=tr().rig_role_guider value=guider devices=devices iface_bit=IFACE_CCD/>
                <DeviceSelect label=tr().rig_role_focuser value=focuser devices=devices iface_bit=IFACE_FOCUSER/>
                <DeviceSelect label=tr().rig_role_filter value=filterwheel devices=devices iface_bit=IFACE_FILTER/>
                <DeviceSelect label=tr().rig_role_rotator value=rotator devices=devices iface_bit=IFACE_ROTATOR/>
            </div>
            <div class="flex gap-sp-3 justify-end">
                <button class=format!("{BTN_BASE} {BTN_EDIT}") on:click=on_cancel>{tr().profiles_cancel}</button>
                <button class=format!("{BTN_BASE} {BTN_LAUNCH}") on:click=on_save>{tr().profiles_save}</button>
            </div>
        </div>
    }
}

#[component]
fn ScopeForm(
    scope_editing: RwSignal<Option<ScopeEdit>>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let st = scope_editing.get_untracked().expect("scope editor open");
    let is_new = st.is_new;
    let id = st.scope.id.clone();

    let vendor = RwSignal::new(st.scope.vendor.clone());
    let model = RwSignal::new(st.scope.model.clone());
    let type_ = RwSignal::new(if st.scope.type_.is_empty() {
        "Refractor".to_string()
    } else {
        st.scope.type_.clone()
    });
    let fl = RwSignal::new(if st.scope.focal_length_mm > 0.0 {
        st.scope.focal_length_mm.to_string()
    } else {
        String::new()
    });
    let ap = RwSignal::new(if st.scope.aperture_mm > 0.0 {
        st.scope.aperture_mm.to_string()
    } else {
        String::new()
    });

    let on_save = {
        let send = send.clone();
        move |_| {
            let mut payload = serde_json::json!({
                "model":        model.get(),
                "vendor":       vendor.get(),
                "type":         type_.get(),
                "focal_length": fl.get().parse::<f64>().unwrap_or(0.0),
                "aperture":     ap.get().parse::<f64>().unwrap_or(0.0),
            });
            let cmd = if is_new {
                "scope_add"
            } else {
                payload["id"] = serde_json::json!(id);
                "scope_update"
            };
            send(serde_json::json!({ "type": cmd, "payload": payload }).to_string());
            // KStars re-sends get_scopes automatically after any scope command.
            scope_editing.set(None);
        }
    };
    let on_cancel = move |_| scope_editing.set(None);

    let type_sel = "input input--sm w-full min-w-0";

    view! {
        <div class="border border-[#3a3a5a] rounded-lg pt-sp-3 pr-4 pb-sp-3 pl-4 mb-sp-3 bg-[rgba(20,24,40,0.4)]">
            <div class="text-[12px] text-text-blue font-semibold tracking-[0.06em] mb-sp-3">
                {move || if is_new { tr().rig_new_scope } else { tr().rig_edit_scope }}
            </div>
            <div class="flex flex-wrap gap-y-sp-3 gap-x-sp-4 mb-sp-3">
                <Field label=tr().rig_scope_vendor><TextInput value=vendor placeholder=""/></Field>
                <Field label=tr().rig_scope_model><TextInput value=model placeholder=""/></Field>
                <Field label=tr().rig_scope_type>
                    <select
                        class=type_sel
                        prop:value=move || type_.get()
                        on:change=move |ev: Event| {
                            if let Some(t) = ev.target() {
                                if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                                    type_.set(sel.value());
                                }
                            }
                        }
                    >
                        <option value="Refractor">"Refractor"</option>
                        <option value="Reflector">"Reflector"</option>
                        <option value="Catadioptric">"Catadioptric"</option>
                    </select>
                </Field>
                <Field label=tr().rig_scope_fl><TextInput value=fl placeholder="600"/></Field>
                <Field label=tr().rig_scope_aperture><TextInput value=ap placeholder="120"/></Field>
            </div>
            <div class="flex gap-sp-3 justify-end">
                <button class=format!("{BTN_BASE} {BTN_EDIT}") on:click=on_cancel>{tr().profiles_cancel}</button>
                <button class=format!("{BTN_BASE} {BTN_LAUNCH}") on:click=on_save>{tr().profiles_save}</button>
            </div>
        </div>
    }
}

#[component]
fn DeviceSelect(
    label: &'static str,
    value: RwSignal<String>,
    devices: RwSignal<Vec<DeviceInfo>>,
    iface_bit: i64,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-[3px] min-w-0">
            <span class="text-xs text-[#778] tracking-[0.05em]">{label}</span>
            <select
                class="input input--sm w-full min-w-0"
                on:change=move |ev: Event| {
                    if let Some(t) = ev.target() {
                        if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                            value.set(sel.value());
                        }
                    }
                }
            >
                <option value="" selected=move || { let v = value.get(); v.is_empty() || v == "--" }>"--"</option>
                {move || {
                    let mut list: Vec<String> = devices.get().into_iter()
                        .filter(|d| d.interface & iface_bit != 0)
                        .map(|d| d.name)
                        .filter(|n| !n.is_empty())
                        .collect();
                    list.sort();
                    list.dedup();
                    // Keep the current value as a sticky option if it isn't in
                    // the connected-device list (e.g. driver offline).
                    let current = value.get();
                    let sticky = if !current.is_empty() && current != "--" && !list.iter().any(|n| n == &current) {
                        Some(current)
                    } else {
                        None
                    };
                    view! {
                        {sticky.map(|s| {
                            let v = s.clone();
                            view! { <option value=v selected=move || value.get() == s>{format!("{s} (offline)")}</option> }
                        })}
                        {list.into_iter().map(|n| {
                            let v = n.clone();
                            let sel_n = n.clone();
                            view! { <option value=v selected=move || value.get() == sel_n>{n}</option> }
                        }).collect_view()}
                    }
                }}
            </select>
        </div>
    }
}

#[component]
fn ScopeSelect(
    label: &'static str,
    value: RwSignal<String>,
    scopes: RwSignal<Vec<ScopeInfo>>,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-[3px] min-w-0">
            <span class="text-xs text-[#778] tracking-[0.05em]">{label}</span>
            <select
                class="input input--sm w-full min-w-0"
                on:change=move |ev: Event| {
                    if let Some(t) = ev.target() {
                        if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                            value.set(sel.value());
                        }
                    }
                }
            >
                <option value="" selected=move || value.get().is_empty()>"--"</option>
                {move || {
                    let names: Vec<String> = scopes.get().into_iter()
                        .map(|s| s.name)
                        .filter(|n| !n.is_empty())
                        .collect();
                    let current = value.get();
                    let sticky = if !current.is_empty() && !names.iter().any(|n| n == &current) {
                        Some(current)
                    } else {
                        None
                    };
                    view! {
                        {sticky.map(|s| {
                            let v = s.clone();
                            view! { <option value=v selected=move || value.get() == s>{format!("{s} (missing)")}</option> }
                        })}
                        {names.into_iter().map(|n| {
                            let v = n.clone();
                            let sel_n = n.clone();
                            view! { <option value=v selected=move || value.get() == sel_n>{n}</option> }
                        }).collect_view()}
                    }
                }}
            </select>
        </div>
    }
}

#[component]
fn ModuleAssign(
    optical_trains: RwSignal<Vec<OpticalTrain>>,
    module_trains: RwSignal<serde_json::Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // (module wire string, ProfileSettings enum key, label). Keys per
    // profilesettings.h:49-56: 1=Capture 2=Focus 3=Mount 4=Guide 5=Align.
    let mods: [(&'static str, &'static str, &'static str); 5] = [
        ("capture", "1", tr().rig_module_capture),
        ("focus", "2", tr().rig_module_focus),
        ("guide", "4", tr().rig_module_guide),
        ("align", "5", tr().rig_module_align),
        ("mount", "3", tr().rig_module_mount),
    ];

    view! {
        <div class="grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-y-sp-2 gap-x-sp-4">
            {mods.into_iter().map(|(module, key, label)| {
                let send = send.clone();
                view! {
                    <div class="flex flex-col gap-[3px] min-w-0">
                        <span class="text-xs text-[#778] tracking-[0.05em]">{label}</span>
                        <select
                            class="input input--sm w-full min-w-0"
                            on:change=move |ev: Event| {
                                if let Some(t) = ev.target() {
                                    if let Ok(sel) = t.dyn_into::<web_sys::HtmlSelectElement>() {
                                        let name = sel.value();
                                        send(serde_json::json!({"type":"train_set","payload":{"module": module, "name": name}}).to_string());
                                        send(r#"{"type":"train_get_profiles","payload":{}}"#.to_string());
                                    }
                                }
                            }
                        >
                            {move || {
                                let cur_id = module_trains.with(|m| m.get(key).and_then(|v| v.as_i64()));
                                optical_trains.get().into_iter().map(|trn| {
                                    let selected = Some(trn.id) == cur_id;
                                    let nm = trn.name.clone();
                                    let nm2 = nm.clone();
                                    view! { <option value=nm selected=selected>{nm2}</option> }
                                }).collect_view()
                            }}
                        </select>
                    </div>
                }
            }).collect_view()}
        </div>
    }
}

fn train_summary(t: &OpticalTrain) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for s in [&t.mount, &t.camera, &t.scope, &t.guider] {
        let s = s.trim();
        if !s.is_empty() && s != "--" {
            parts.push(s);
        }
    }
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(" · ")
    }
}

// Silence unused-import warnings if SendCmd is dropped from a closure path.
const _: fn() = || {
    let _ = std::mem::size_of::<Arc<dyn Fn(String) + Send + Sync>>();
};
