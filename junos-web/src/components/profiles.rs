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
use crate::ws::{DriverInfo, ProfileInfo, SendCmd};

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

// Silence unused-import warnings if SendCmd is dropped from a closure path.
const _: fn() = || {
    let _ = std::mem::size_of::<Arc<dyn Fn(String) + Send + Sync>>();
};
