//! Files tab — capture-folder browser + LiveStacker control panel.
//!
//! Talks to:
//!   - rekos-server `/api/files/*` HTTP endpoints (sandboxed to the configured
//!     captures dir) for directory listing, metadata, thumbnails, raw image
//!     bytes (FITS auto-stretched server-side).
//!   - KStars Ekos Live `livestacker_*` messages over the existing WS.

use std::sync::Arc;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};
use wasm_bindgen::JsCast;

use crate::i18n::{t, Lang, Translations};
use crate::ws::{LiveStackerState, SendCmd};
use crate::ws_helpers::send_cmd;

// ── Server response types ────────────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
struct DirEntry {
    name: String,
    kind: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mtime: u64,
    #[serde(default)]
    ext: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ListReply {
    path: String,
    parent: Option<String>,
    entries: Vec<DirEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct FileMeta {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mtime: u64,
    #[serde(default)]
    ext: String,
    fits: Option<FitsInfo>,
}

#[derive(Clone, Debug, Deserialize)]
struct FitsInfo {
    header: Vec<FitsRow>,
    parsed: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct FitsRow {
    key: String,
    value: String,
    comment: String,
}

// ── Tailwind class fragments ─────────────────────────────────────────────────
const FILES_ROW: &str = "flex items-center gap-[6px] w-full text-left py-[6px] px-sp-2 mb-[2px] bg-transparent text-text border border-transparent rounded-sm cursor-pointer hover:bg-[rgba(136,170,255,0.08)] hover:border-border-strong";
const FILES_THUMB_BASE: &str = "bg-bg-input-deep border border-border-strong rounded-sm overflow-hidden cursor-pointer p-0 text-inherit flex flex-col hover:border-text-blue";
const FILES_BTN: &str = "bg-bg-button text-text-dim border border-border-strong py-[5px] px-sp-3 rounded-sm text-[12px] cursor-pointer hover:border-text-blue";
const FILES_FIELD: &str = "flex flex-col gap-[3px] text-sm text-text-blue";
const FILES_FIELD_INPUT: &str = "bg-bg-input-deep text-text border border-border-strong py-1 px-[6px] rounded-sm text-[12px] focus:outline-none focus:border-text-blue";
const FILES_SECTION: &str = "text-text-blue text-sm uppercase tracking-[0.06em] mt-[14px] mb-[6px] font-semibold";
const FILES_KV: &str = "flex justify-between gap-sp-2";

// ── Component ────────────────────────────────────────────────────────────────

#[component]
pub fn FilesTab(
    livestacker_state: RwSignal<Option<LiveStackerState>>,
    livestacker_settings: RwSignal<Value>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // ── Persisted current path ────────────────────────────────────────────
    let ls = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
    let initial_path = ls
        .as_ref()
        .and_then(|s| s.get_item("files_path").ok().flatten())
        .unwrap_or_default();
    let current_path = RwSignal::new(initial_path);

    // Persist on change.
    Effect::new(move |_| {
        let p = current_path.get();
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item("files_path", &p);
        }
    });

    let listing = RwSignal::new(None::<ListReply>);
    let list_error = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);

    // Refetch the directory listing whenever the path changes.
    Effect::new(move |_| {
        let path = current_path.get();
        loading.set(true);
        list_error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_list(&path).await {
                Ok(reply) => {
                    listing.set(Some(reply));
                    list_error.set(None);
                }
                Err(e) => {
                    listing.set(None);
                    list_error.set(Some(e));
                }
            }
            loading.set(false);
        });
    });

    let selected = RwSignal::new(None::<String>);
    let selected_meta = RwSignal::new(None::<FileMeta>);
    let meta_error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        let Some(rel) = selected.get() else {
            selected_meta.set(None);
            return;
        };
        meta_error.set(None);
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_meta(&rel).await {
                Ok(m) => {
                    selected_meta.set(Some(m));
                    meta_error.set(None);
                }
                Err(e) => {
                    selected_meta.set(None);
                    meta_error.set(Some(e));
                }
            }
        });
    });

    // Fetch settings once on mount.
    {
        let send_init = Arc::clone(&send);
        Effect::new(move |prev: Option<()>| {
            if prev.is_none() {
                send_cmd(&send_init, "livestacker_get_all_settings", json!({}));
            }
        });
    }

    let send_btn = Arc::clone(&send);

    view! {
        <div class="absolute inset-0 bg-bg text-text font-mono grid grid-rows-[auto_1fr] overflow-hidden">
            // Header strip
            <div class="flex items-center gap-3 py-sp-3 pr-5 pl-20 border-b border-border-base bg-bg-panel-solid text-md min-h-[44px]">
                <span class="text-text-blue font-semibold tracking-[0.06em]">{move || tr().files_title}</span>
                <span class="text-text">
                    {move || render_breadcrumb(&current_path.get(), tr().files_breadcrumb_root, current_path)}
                </span>
                <span class="flex-1"></span>
                <span class="text-text-blue text-sm">
                    {move || if loading.get() { tr().files_loading } else { "" }}
                </span>
            </div>

            <div class="grid grid-cols-[220px_minmax(0,1fr)_380px] max-[900px]:grid-cols-[160px_1fr_280px] max-[700px]:grid-cols-1 max-[700px]:grid-rows-[auto_auto_auto] max-[700px]:overflow-y-auto min-h-0 overflow-hidden">
                // Left: folder list + parent
                <div class="border-r border-border-base overflow-y-auto p-sp-2">
                    <Show when=move || !current_path.with(|p| p.is_empty())>
                        <button
                            class=format!("{FILES_ROW} text-text-blue")
                            on:click=move |_| {
                                let cur = current_path.get();
                                let parent = parent_of(&cur);
                                current_path.set(parent);
                                selected.set(None);
                            }
                        >
                            {move || tr().files_parent}
                        </button>
                    </Show>
                    {move || {
                        let l = listing.get();
                        let dirs: Vec<DirEntry> = l
                            .as_ref()
                            .map(|r| r.entries.iter().filter(|e| e.kind == "dir").cloned().collect())
                            .unwrap_or_default();
                        dirs.into_iter().map(|d| {
                            let name = d.name.clone();
                            view! {
                                <button
                                    class=FILES_ROW
                                    on:click=move |_| {
                                        let cur = current_path.get();
                                        let next = if cur.is_empty() { name.clone() }
                                                   else { format!("{}/{}", cur, name) };
                                        current_path.set(next);
                                        selected.set(None);
                                    }
                                >
                                    <span class="text-[14px]">"\u{1F4C1}"</span>
                                    <span class="whitespace-nowrap overflow-hidden text-ellipsis">{d.name.clone()}</span>
                                </button>
                            }
                        }).collect_view()
                    }}
                    <Show when=move || list_error.get().is_some()>
                        <div class="text-[#ff8888] p-sp-2 text-[12px]">
                            {move || format!("{}: {}", tr().files_error, list_error.get().unwrap_or_default())}
                        </div>
                    </Show>
                </div>

                // Center: thumbnails
                <div class="overflow-y-auto p-3 grid grid-cols-[repeat(auto-fill,minmax(150px,1fr))] gap-sp-3 content-start">
                    {move || {
                        let l = listing.get();
                        let files: Vec<DirEntry> = l
                            .as_ref()
                            .map(|r| r.entries.iter().filter(|e| e.kind == "file" && is_image_ext(&e.ext)).cloned().collect())
                            .unwrap_or_default();
                        if files.is_empty() && !loading.get() && list_error.get().is_none() {
                            return view! {
                                <div class="text-text-faint p-5 text-center">{tr().files_empty_dir}</div>
                            }.into_any();
                        }
                        let cur_path = current_path.get();
                        files.into_iter().map(|f| {
                            let rel = if cur_path.is_empty() { f.name.clone() }
                                      else { format!("{}/{}", cur_path, f.name) };
                            let rel_for_thumb = rel.clone();
                            let rel_for_click = rel.clone();
                            let active = move || selected.get().as_deref() == Some(rel.as_str());
                            view! {
                                <button
                                    class=move || if active() { format!("{FILES_THUMB_BASE} !border-text-blue shadow-[0_0_0_2px_rgba(136,170,255,0.25)]") } else { FILES_THUMB_BASE.to_string() }
                                    on:click=move |_| selected.set(Some(rel_for_click.clone()))
                                >
                                    <img
                                        class="w-full aspect-square object-cover bg-black block"
                                        src=format!("/api/files/thumb?size=256&path={}", url_encode(&rel_for_thumb))
                                        loading="lazy"
                                    />
                                    <span class="text-xs py-1 px-[6px] text-text-muted whitespace-nowrap overflow-hidden text-ellipsis">{f.name.clone()}</span>
                                </button>
                            }.into_any()
                        }).collect_view().into_any()
                    }}
                </div>

                // Right: detail panel + LiveStacker pane
                <div class="border-l border-border-base overflow-y-auto p-3 bg-bg-panel-dim">
                    {move || render_detail(selected.get(), selected_meta.get(), meta_error.get(), &tr())}

                    <div class="mt-[18px] pt-3 border-t border-border-base">
                        <h3 class=FILES_SECTION>{move || tr().livestack_title}</h3>
                        <div class="flex gap-[6px] flex-wrap mb-sp-2">
                            {
                                let s1 = Arc::clone(&send_btn);
                                let s2 = Arc::clone(&send_btn);
                                let s3 = Arc::clone(&send_btn);
                                let s4 = Arc::clone(&send_btn);
                                view! {
                                    <button class=FILES_BTN on:click=move |_| send_cmd(&s1, "livestacker_initialize", json!({}))>
                                        {move || tr().livestack_init}
                                    </button>
                                    <button class=FILES_BTN on:click=move |_| send_cmd(&s2, "livestacker_start", json!({}))>
                                        {move || tr().livestack_start}
                                    </button>
                                    <button class=FILES_BTN on:click=move |_| send_cmd(&s3, "livestacker_stop", json!({}))>
                                        {move || tr().livestack_stop}
                                    </button>
                                    <button class=FILES_BTN on:click=move |_| send_cmd(&s4, "livestacker_close", json!({}))>
                                        {move || tr().livestack_close}
                                    </button>
                                }
                            }
                        </div>
                        <div class="text-[12px] min-h-[22px]">
                            {move || render_livestack_status(livestacker_state.get(), &tr())}
                        </div>
                        <details class="mt-sp-3 border border-border-strong rounded-sm bg-[rgba(6,6,15,0.4)] py-[6px] px-sp-2">
                            <summary class="cursor-pointer text-text-blue text-sm uppercase tracking-[0.06em]">{move || tr().livestack_settings}</summary>
                            <LiveStackSettings settings=livestacker_settings send=Arc::clone(&send_btn) />
                        </details>
                    </div>
                </div>
            </div>
        </div>
    }
}

// ── Live stacker settings sub-component ──────────────────────────────────────

#[component]
fn LiveStackSettings(settings: RwSignal<Value>, send: SendCmd) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let dir_in = RwSignal::new(String::new());
    let dir_out = RwSignal::new(String::new());
    let low_sigma = RwSignal::new(String::new());
    let high_sigma = RwSignal::new(String::new());
    let looping = RwSignal::new(false);
    let calc_snr = RwSignal::new(false);

    // Hydrate inputs from incoming settings payload.
    Effect::new(move |_| {
        let v = settings.get();
        if let Some(s) = v.get("stackingDirectory").and_then(|x| x.as_str()) {
            dir_in.set(s.to_string());
        }
        if let Some(s) = v.get("outputDirectory").and_then(|x| x.as_str()) {
            dir_out.set(s.to_string());
        }
        if let Some(f) = v.get("lowSigma").and_then(|x| x.as_f64()) {
            low_sigma.set(format!("{}", f));
        }
        if let Some(f) = v.get("highSigma").and_then(|x| x.as_f64()) {
            high_sigma.set(format!("{}", f));
        }
        if let Some(b) = v.get("looping").and_then(|x| x.as_bool()) {
            looping.set(b);
        }
        if let Some(b) = v.get("calcSNR").and_then(|x| x.as_bool()) {
            calc_snr.set(b);
        }
    });

    let send_apply = send;
    let on_apply = move |_| {
        let payload = json!({
            "stackingDirectory": dir_in.get(),
            "outputDirectory":   dir_out.get(),
            "looping":           looping.get(),
            "calcSNR":           calc_snr.get(),
            "lowSigma":          low_sigma.get().parse::<f64>().unwrap_or(3.0),
            "highSigma":         high_sigma.get().parse::<f64>().unwrap_or(3.0),
        });
        send_cmd(&send_apply, "livestacker_set_all_settings", payload);
    };

    let row_field = "flex flex-row items-center gap-sp-2 text-text text-[12px]";

    view! {
        <div class="flex flex-col gap-sp-2 py-sp-2">
            <label class=FILES_FIELD>
                <span>{move || tr().livestack_dir_in}</span>
                <input
                    type="text"
                    class=FILES_FIELD_INPUT
                    prop:value=move || dir_in.get()
                    on:input=move |ev| dir_in.set(event_value(&ev))
                />
            </label>
            <label class=FILES_FIELD>
                <span>{move || tr().livestack_dir_out}</span>
                <input
                    type="text"
                    class=FILES_FIELD_INPUT
                    prop:value=move || dir_out.get()
                    on:input=move |ev| dir_out.set(event_value(&ev))
                />
            </label>
            <label class=row_field>
                <input
                    type="checkbox"
                    prop:checked=move || looping.get()
                    on:change=move |ev| looping.set(event_checked(&ev))
                />
                <span>{move || tr().livestack_looping}</span>
            </label>
            <label class=row_field>
                <input
                    type="checkbox"
                    prop:checked=move || calc_snr.get()
                    on:change=move |ev| calc_snr.set(event_checked(&ev))
                />
                <span>{move || tr().livestack_calc_snr}</span>
            </label>
            <label class=FILES_FIELD>
                <span>{move || tr().livestack_low_sigma}</span>
                <input
                    type="number" step="0.1"
                    class=FILES_FIELD_INPUT
                    prop:value=move || low_sigma.get()
                    on:input=move |ev| low_sigma.set(event_value(&ev))
                />
            </label>
            <label class=FILES_FIELD>
                <span>{move || tr().livestack_high_sigma}</span>
                <input
                    type="number" step="0.1"
                    class=FILES_FIELD_INPUT
                    prop:value=move || high_sigma.get()
                    on:input=move |ev| high_sigma.set(event_value(&ev))
                />
            </label>
            <button class=format!("{FILES_BTN} bg-[rgba(40,60,110,0.95)] border-text-blue") on:click=on_apply>
                {move || tr().livestack_apply}
            </button>
        </div>
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn render_livestack_status(s: Option<LiveStackerState>, tr: &Translations) -> impl IntoView {
    match s {
        None => view! { <span class="text-text-faint">{tr.livestack_no_state}</span> }.into_any(),
        Some(st) => {
            let frames = format!("{} / {}", st.frames_stacked, st.total_frames);
            let snr = if st.mean_snr > 0.0 { format!("{:.2}", st.mean_snr) } else { "—".into() };
            let state_label = st.state.clone();
            view! {
                <div class="flex gap-[14px] flex-wrap items-center">
                    <span class="bg-[rgba(40,60,110,0.95)] border border-text-blue py-[2px] px-sp-2 rounded-[10px] text-sm text-text-dim">{state_label}</span>
                    <span><b>{tr.livestack_frames}":"</b>" "{frames}</span>
                    <span><b>{tr.livestack_snr}":"</b>" "{snr}</span>
                </div>
            }.into_any()
        }
    }
}

fn render_detail(
    sel: Option<String>,
    meta: Option<FileMeta>,
    err: Option<String>,
    tr: &Translations,
) -> impl IntoView {
    let Some(rel) = sel else {
        return view! { <div class="text-text-faint p-5 text-center text-[12px]">{tr.files_no_selection}</div> }.into_any();
    };
    if let Some(e) = err {
        return view! { <div class="text-[#ff8888] p-sp-2 text-[12px]">{format!("{}: {}", tr.files_error, e)}</div> }.into_any();
    }
    let preview_url = format!("/api/files/raw?as=preview&path={}", url_encode(&rel));
    let meta_view = match meta {
        None => view! { <div class="text-text-faint p-5 text-center text-[12px]">{tr.files_loading}</div> }.into_any(),
        Some(m) => render_meta_blocks(&m, tr).into_any(),
    };
    view! {
        <img class="w-full max-h-[260px] object-contain bg-black border border-border-strong rounded-sm mb-3" src=preview_url />
        {meta_view}
    }.into_any()
}

fn render_meta_blocks(m: &FileMeta, tr: &Translations) -> impl IntoView {
    let p = m.fits.as_ref().map(|f| f.parsed.clone()).unwrap_or(Value::Null);

    let kv = |label: &'static str, value: String| {
        view! { <div class=FILES_KV><span class="text-text-blue">{label}</span><span class="text-[#d0d0e0] text-right break-words">{value}</span></div> }
    };
    let basic = vec![
        kv(tr.files_filename, m.name.clone()),
        kv(tr.files_size, format_size(m.size)),
        kv(tr.files_mtime, format_mtime(m.mtime)),
        kv(tr.files_exposure, value_or_dash(p.get("exposure"))),
        kv(tr.files_gain, value_or_dash(p.get("gain"))),
        kv(tr.files_binning, value_or_dash(p.get("binning"))),
        kv(tr.files_frame_type, value_or_dash(p.get("frame_type"))),
    ];
    let optical = vec![
        kv(tr.files_filter, value_or_dash(p.get("filter"))),
        kv(tr.files_target, value_or_dash(p.get("target"))),
        kv(tr.files_focal, value_or_dash(p.get("focal_length"))),
        kv(tr.files_pixel_size, value_or_dash(p.get("pixel_size"))),
        kv(tr.files_temp, value_or_dash(p.get("ccd_temp"))),
    ];
    let astrom = vec![
        kv(tr.files_ra, value_or_dash(p.get("ra"))),
        kv(tr.files_dec, value_or_dash(p.get("dec"))),
        kv(tr.files_fov, fov_str(p.get("fov_arcmin"))),
        kv(tr.files_rotation, value_or_dash(p.get("rotation"))),
        kv(tr.files_plate_solved, p.get("plate_solved").and_then(|v| v.as_bool()).map(|b| if b { "Y" } else { "N" }.to_string()).unwrap_or_else(|| "—".into())),
    ];

    let header_rows: Vec<FitsRow> = m.fits.as_ref().map(|f| f.header.clone()).unwrap_or_default();
    let has_header = !header_rows.is_empty();
    // Copy out the &'static str labels so the closures below don't capture
    // the function-scoped &Translations reference.
    let lbl_basics = tr.files_capture_basics;
    let lbl_optical = tr.files_optical;
    let lbl_astrom = tr.files_astrometry;
    let lbl_raw = tr.files_raw_header;

    let kv_list = "grid gap-[3px] text-[12px]";

    view! {
        <h3 class=FILES_SECTION>{lbl_basics}</h3>
        <div class=kv_list>{basic.into_iter().collect_view()}</div>
        <h3 class=FILES_SECTION>{lbl_optical}</h3>
        <div class=kv_list>{optical.into_iter().collect_view()}</div>
        <h3 class=FILES_SECTION>{lbl_astrom}</h3>
        <div class=kv_list>{astrom.into_iter().collect_view()}</div>
        <Show when=move || has_header>
            <details class="mt-sp-3 border border-border-strong rounded-sm bg-[rgba(6,6,15,0.4)] py-[6px] px-sp-2">
                <summary class="cursor-pointer text-text-blue text-sm uppercase tracking-[0.06em]">{lbl_raw}</summary>
                <div class="mt-sp-2 max-h-[280px] overflow-auto text-sm font-mono">
                    {header_rows.iter().map(|r| view! {
                        <div class="grid grid-cols-[80px_1fr_1.2fr] gap-[6px] py-[1px] border-b border-dotted border-[#1a1a25]">
                            <span class="text-text-blue">{r.key.clone()}</span>
                            <span class="text-[#d0d0e0] break-all">{r.value.clone()}</span>
                            <span class="text-text-faint italic break-all">{r.comment.clone()}</span>
                        </div>
                    }).collect_view()}
                </div>
            </details>
        </Show>
    }
}

fn render_breadcrumb(path: &str, root_label: &'static str, current_path: RwSignal<String>) -> impl IntoView {
    let mut acc = String::new();
    let mut chips: Vec<(String, String)> = vec![(String::new(), root_label.to_string())];
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() { acc.push('/'); }
        acc.push_str(seg);
        chips.push((acc.clone(), seg.to_string()));
    }
    let crumb_base = "bg-transparent border-none cursor-pointer font-inherit py-[2px] px-sp-1 rounded-sm hover:bg-[rgba(136,170,255,0.15)]";
    chips.into_iter().enumerate().map(|(i, (target, label))| {
        let is_last = i + 1 == path.split('/').filter(|s| !s.is_empty()).count() + 1;
        view! {
            <span class="text-text-faint">{if i == 0 { "" } else { " / " }}</span>
            <button
                class=if is_last { format!("{crumb_base} text-text-dim") } else { format!("{crumb_base} text-text-blue") }
                on:click=move |_| current_path.set(target.clone())
            >{label}</button>
        }
    }).collect_view()
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "fits" | "fit" | "fts" | "jpg" | "jpeg" | "png" | "tif" | "tiff" | "xisf" | "cr2" | "nef" | "arw")
}

fn url_encode(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

fn event_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn event_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or(false)
}

fn format_size(n: u64) -> String {
    if n < 1024 { return format!("{} B", n); }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 { return format!("{:.1} KB", kb); }
    let mb = kb / 1024.0;
    if mb < 1024.0 { return format!("{:.1} MB", mb); }
    format!("{:.1} GB", mb / 1024.0)
}

fn format_mtime(secs: u64) -> String {
    if secs == 0 { return "—".into(); }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
    d.to_iso_string().as_string().unwrap_or_default()
}

fn value_or_dash(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "—".into(),
        Some(Value::String(s)) if s.is_empty() => "—".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => (if *b { "Y" } else { "N" }).into(),
        Some(Value::Number(n)) => {
            if let Some(f) = n.as_f64() { format!("{:.4}", f).trim_end_matches('0').trim_end_matches('.').to_string() }
            else { n.to_string() }
        }
        Some(other) => other.to_string(),
    }
}

fn fov_str(v: Option<&Value>) -> String {
    let Some(o) = v else { return "—".into() };
    if o.is_null() { return "—".into(); }
    let w = o.get("w").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let h = o.get("h").and_then(|x| x.as_f64()).unwrap_or(0.0);
    format!("{:.1} \u{00d7} {:.1}", w, h)
}

// ── Fetchers ─────────────────────────────────────────────────────────────────

async fn fetch_list(path: &str) -> Result<ListReply, String> {
    let url = format!("/api/files/list?path={}", url_encode(path));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<ListReply>().await.map_err(|e| e.to_string())
}

async fn fetch_meta(path: &str) -> Result<FileMeta, String> {
    let url = format!("/api/files/meta?path={}", url_encode(path));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<FileMeta>().await.map_err(|e| e.to_string())
}

