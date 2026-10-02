//! Imaging tab: the last frame, live progress, one-shot settings, cooling and
//! the capture queue.
//!
//! Layout (phone-first, like Focus): a header (camera · state · reveal in
//! Files), then the frame — pinned on phones with the cards scrolling beneath
//! it, frame | cards from `md` — and a pinned footer with Preview, Loop and
//! Start / Stop, so Stop is always one tap away. Tapping the frame opens it
//! full screen (`components/zoom.rs`); the sequence editor and a job's
//! details open as `sheet`s.
//!
//! Outbound (message.cpp::processCaptureCommands):
//!   - capture_preview, capture_loop, capture_start (runs the next pending
//!     job), capture_stop (stops whatever runs, framing included)
//!   - capture_set_all_settings {<widget>: value} — `fields.rs`
//!   - capture_load_sequence_file {filedata | filepath} (replaces the queue),
//!     capture_save_sequence_file {filepath}, capture_remove_sequence {index},
//!     capture_clear_sequences, capture_get_sequences
//!   - device_property_set: CCD_COOLER / CCD_TEMPERATURE on the camera,
//!     FILTER_SLOT on the filter wheel
//!
//! Inbound: new_capture_state (`status` is the untranslated `captureStates`
//! label, ekos.h:69; `log` is newest first), new_camera_state,
//! capture_get_all_settings, capture_get_sequences, new_preview_image — all
//! folded by `ws/store.rs`.

mod fields;
mod jobs;

use leptos::prelude::*;
use serde_json::json;

use crate::compat::{CameraSnapshot, CaptureSnapshot, FilterWheelSnapshot};
use crate::components::form::{sheet, CARD, CARD_TITLE, CHIP, FOOTER, LABEL, NUM};
use crate::components::sequence_editor::{build_esq_xml, SeqFrame, SequenceEditor};
use crate::components::tab_wheel_icons::tab_icon;
use crate::components::zoom::frame_zoom;
use crate::dom::event_target_value;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::{send_cmd, send_device_property_set};
use crate::{ActiveTabCtx, RevealInFilesCtx, Tab};

const DASH: &str = "\u{2014}";
const SAVE: &str = "capture_save_sequence_file";
const LOAD: &str = "capture_load_sequence_file";

/// The camera is working. Also the `camera_busy` interlock (`compat.rs`), so
/// the Sky's Goto and this tab agree.
pub(crate) fn status_is_active(status: &str) -> bool {
    let s = status.to_lowercase();
    s.contains("capturing")
        || s.contains("progress")
        || s.contains("dither")
        || s.contains("focus")
        || s.contains("filter")
        || s.contains("align")
        || s.contains("temperature")
        || s.contains("rotator")
        || s.contains("meridian")
        || s.contains("calibrat")
        || s.contains("waiting")
}

fn status_badge(status: &str) -> &'static str {
    match status {
        "" | "Idle" => "badge",
        "Complete" => "badge badge--ok",
        "Aborted" => "badge badge--err",
        "Paused" | "Pause Planned" | "Suspended" => "badge badge--warn",
        _ => "badge badge--info",
    }
}

/// A readout tile, with a thin bar along its bottom edge for `fill` (0–1).
fn tile(
    label: impl Fn() -> &'static str + Send + 'static,
    value: impl Fn() -> String + Send + 'static,
    fill: impl Fn() -> f64 + Send + 'static,
) -> impl IntoView {
    view! {
        <div class="relative min-w-0 overflow-hidden rounded-lg bg-bg-elev-2 border border-border-base px-2.5 py-1.5 flex flex-col">
            <span class="text-xs uppercase tracking-[0.06em] text-text-muted truncate">{move || label()}</span>
            <span class="font-mono tabular-nums text-base md:text-lg leading-tight truncate text-text-blue-bright">
                {move || value()}
            </span>
            <span class="absolute left-0 bottom-0 h-0.5 bg-accent-cyan transition-[width] duration-300"
                  style:width=move || format!("{:.1}%", fill().clamp(0.0, 1.0) * 100.0)></span>
        </div>
    }
}

/// Ask for the queue once KStars has had time to load a sequence file.
fn refresh_queue_soon(send: SendCmd) {
    wasm_bindgen_futures::spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(500).await;
        send_cmd(&send, "capture_get_sequences", json!({}));
    });
}

#[component]
pub fn ImagingTab(
    #[prop(into)] capture: Signal<CaptureSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // `capture` changes on every countdown tick: each part gets its own memo,
    // so a tick only redraws the progress tiles.
    let status = Memo::new(move |_| capture.with(|c| c.status.clone()));
    let preview = Memo::new(move |_| capture.with(|c| c.preview_url.clone()));
    let queue = Memo::new(move |_| capture.with(|c| c.sequence.as_array().cloned().unwrap_or_default()));
    let live = Memo::new(move |_| capture.with(|c| (c.seq_current, c.seq_total)));
    let device = Memo::new(move |_| camera.with(|c| c.device.clone()));
    let temperature = Memo::new(move |_| camera.with(|c| c.temperature));
    let cooler_on = Memo::new(move |_| camera.with(|c| c.cooler_on == Some(true)));
    let settings = fields::Settings::new(capture, send.clone());
    // KStars rests on "Image Received" between frames and after a lone
    // preview, so a running queue job counts as busy too.
    let busy = Memo::new(move |_| {
        status.with(|s| status_is_active(s)) || queue.with(|q| q.iter().any(|j| j["Status"] == "In Progress"))
    });

    let zoom_open = RwSignal::new(false);
    let editor_open = RwSignal::new(false);
    let detail = RwSignal::new(None::<usize>);
    let esc = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() == "Escape" {
            zoom_open.set(false);
            editor_open.set(false);
            detail.set(None);
        }
    });
    on_cleanup(move || esc.remove());
    // The detailed job left the queue.
    Effect::new(move |_| {
        if detail.get().is_some_and(|i| i >= queue.with(Vec::len)) {
            detail.set(None);
        }
    });

    // ── Actions ──────────────────────────────────────────────────────────
    let s_plain = send.clone();
    let plain = move |ty: &'static str| {
        let s = s_plain.clone();
        move |_| send_cmd(&s, ty, json!({}))
    };
    let s_run = send.clone();
    let on_run = move |_| {
        let ty = if busy.get_untracked() { "capture_stop" } else { "capture_start" };
        send_cmd(&s_run, ty, json!({}));
    };

    let reveal = use_context::<RevealInFilesCtx>();
    let active_tab = use_context::<ActiveTabCtx>();
    let on_reveal = move |_| {
        let dir = settings.str("fileDirectoryT").trim().to_string();
        if let Some(r) = reveal { r.0.set((!dir.is_empty()).then_some(dir)); }
        if let Some(a) = active_tab { a.0.set(Tab::Files); }
    };

    // Cooling, straight to the camera's INDI properties.
    let target_temp = RwSignal::new("-10".to_string());
    let s_cool = send.clone();
    let on_cooler = move |_| {
        let dev = device.get_untracked();
        let on = !cooler_on.get_untracked();
        if !dev.is_empty() {
            send_device_property_set(&s_cool, &dev, "CCD_COOLER", json!([
                { "name": "COOLER_ON",  "state": i32::from(on) },
                { "name": "COOLER_OFF", "state": i32::from(!on) },
            ]));
        }
    };
    let s_temp = send.clone();
    let on_set_temp = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let dev = device.get_untracked();
        if let (false, Ok(v)) = (dev.is_empty(), target_temp.get_untracked().trim().parse::<f64>()) {
            send_device_property_set(&s_temp, &dev, "CCD_TEMPERATURE",
                                     json!([{ "name": "CCD_TEMPERATURE_VALUE", "value": v }]));
        }
    };

    // Queue. The editor's draft lives here, so it survives closing the sheet.
    let frames = RwSignal::new(vec![SeqFrame::default()]);
    let fits_dir = RwSignal::new(String::new());
    let s_seq = send.clone();
    let on_send_seq = move |_| {
        let xml = build_esq_xml("", &fits_dir.get_untracked(), &frames.get_untracked(), true);
        send_cmd(&s_seq, LOAD, json!({ "filedata": xml }));
        refresh_queue_soon(s_seq.clone());
        editor_open.set(false);
    };
    // The Save / Load path row: the command it runs, "" while hidden.
    let file_cmd = RwSignal::new("");
    let file_path = RwSignal::new(String::new());
    let s_file = send.clone();
    let on_file = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let path = file_path.get_untracked().trim().to_string();
        if !path.is_empty() {
            send_cmd(&s_file, file_cmd.get_untracked(), json!({ "filepath": path }));
            refresh_queue_soon(s_file.clone());
            file_cmd.set("");
        }
    };
    let toggle_file = move |cmd: &'static str| file_cmd.update(|c| *c = if *c == cmd { "" } else { cmd });
    let s_rm = send.clone();
    let remove = Callback::new(move |i: usize| send_cmd(&s_rm, "capture_remove_sequence", json!({ "index": i })));
    let open_job = Callback::new(move |i: usize| detail.set(Some(i)));

    let editor_body = move || {
        let on_send = on_send_seq.clone();
        view! {
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3">
                <SequenceEditor frames=frames fits_dir=fits_dir camera=camera filter_wheel=filter_wheel />
            </div>
            <div class=FOOTER>
                <button class="btn btn-primary h-11 px-5 ml-auto font-semibold"
                        disabled=move || frames.with(|f| f.is_empty() || !f.iter().all(SeqFrame::is_valid))
                        on:click=on_send>
                    {move || tr().imaging_send_sequence}
                </button>
            </div>
        }
    };
    let detail_body = move || view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3">
            {move || detail.get()
                .and_then(|i| queue.with(|q| q.get(i).cloned()))
                .map(|job| jobs::job_details(&job, tr()))}
        </div>
        <div class=FOOTER>
            <button class="btn btn-danger h-11 px-5 ml-auto"
                    on:click=move |_| {
                        if let Some(i) = detail.get_untracked() { remove.run(i); }
                        detail.set(None);
                    }>
                {move || tr().imaging_remove_job}
            </button>
        </div>
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Imaging)></span>
                <span class="shrink-0 font-semibold text-text-blue-bright">{move || tr().tab_imaging}</span>
                <span class="min-w-0 truncate text-sm text-text-muted">{move || device.get()}</span>
                <span class=move || format!("{} ml-auto shrink-0", status_badge(&status.get()))>
                    {move || { let s = status.get(); if s.is_empty() { tr().idle.to_string() } else { s } }}
                </span>
                <button class="btn-icon shrink-0 text-text-muted" title=move || tr().files_open_in_files on:click=on_reveal>
                    <span class="inline-block w-5 h-5" inner_html=tab_icon(Tab::Files)></span>
                </button>
            </div>

            // Body — a column on phones, frame | cards on md+.
            <div class="flex-1 min-h-0 flex flex-col md:grid md:grid-cols-[minmax(0,1fr)_360px] \
                        lg:grid-cols-[minmax(0,1fr)_400px] md:grid-rows-[minmax(0,1fr)] md:gap-3 md:p-3 md:pl-4 md:pr-6">
                // Frame — pinned on phones; a tap opens it full screen.
                <div class="relative shrink-0 h-[36dvh] min-h-[160px] overflow-hidden flex items-center justify-center \
                            bg-bg-input-deep border-b border-border-base md:h-auto md:min-h-0 md:border md:rounded-lg">
                    <Show when=move || preview.with(Option::is_some)
                          fallback=move || view! {
                              <div class="text-text-faint text-sm text-center px-6">{move || tr().imaging_no_frame}</div>
                          }>
                        <img src=move || preview.get().unwrap_or_default() title=move || tr().imaging_view_fullres
                             class="max-w-full max-h-full object-contain cursor-zoom-in [image-rendering:pixelated]"
                             on:click=move |_| zoom_open.set(true) />
                    </Show>
                </div>

                // Cards
                <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] flex flex-col gap-3 p-3 md:p-0">
                    // Progress: this frame, this job, the whole queue.
                    <div class=CARD>
                        <div class="grid grid-cols-3 gap-2">
                            {tile(move || tr().imaging_exposure,
                                move || capture.with(|c| c.exposure_left.filter(|_| busy.get())
                                    .map_or_else(|| DASH.into(), |v| format!("{:.1} s", v.max(0.0)))),
                                move || capture.with(|c| match (c.exposure_left, c.exposure_total) {
                                    (Some(left), Some(total)) if total > 0.0 && busy.get() => 1.0 - left / total,
                                    _ => 0.0,
                                }))}
                            {tile(move || tr().imaging_frames,
                                move || match live.get() {
                                    (Some(a), Some(b)) => format!("{a} / {b}"),
                                    _ => DASH.into(),
                                },
                                move || match live.get() {
                                    (Some(a), Some(b)) if b > 0 => a as f64 / b as f64,
                                    _ => 0.0,
                                })}
                            {tile(move || tr().imaging_remaining,
                                move || capture.with(|c| if c.overall_remaining_time.is_empty() {
                                    DASH.into()
                                } else {
                                    c.overall_remaining_time.clone()
                                }),
                                move || capture.with(|c| c.progress.unwrap_or(0.0) / 100.0))}
                        </div>
                        {move || capture.with(|c| c.log.lines().map(str::trim).find(|l| !l.is_empty()).map(|l| {
                            let (line, title) = (l.to_string(), l.to_string());
                            view! { <div class="font-mono text-xs text-text-muted truncate" title=title>{line}</div> }
                        }))}
                    </div>

                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().imaging_one_shot}</span>
                        {fields::capture_form(settings, camera, filter_wheel, lang)}
                    </div>

                    // Cooling — for cameras that report a temperature.
                    <Show when=move || temperature.get().is_some()>
                        <div class=CARD>
                            <div class="flex items-center gap-2">
                                <span class=format!("{CARD_TITLE} flex-1")>{move || tr().imaging_cooling}</span>
                                <button type="button"
                                        class=move || if cooler_on.get() { format!("{CHIP} gap-2 btn--active") } else { format!("{CHIP} gap-2") }
                                        aria-pressed=move || cooler_on.get().to_string()
                                        on:click=on_cooler.clone()>
                                    <span class=move || if cooler_on.get() {
                                        "w-2 h-2 rounded-full bg-state-ok"
                                    } else {
                                        "w-2 h-2 rounded-full bg-text-faint"
                                    }></span>
                                    {move || tr().imaging_cooler}
                                </button>
                            </div>
                            <form class="flex items-center gap-2" on:submit=on_set_temp.clone()>
                                <span class="flex-1 min-w-0 font-mono tabular-nums text-xl text-text-blue-bright">
                                    {move || temperature.get().map_or_else(|| DASH.into(), |v| format!("{v:.1}\u{00b0}C"))}
                                </span>
                                <span class=LABEL>{move || tr().imaging_target}</span>
                                // No inputmode: iOS' decimal pad has no minus key.
                                <input type="number" step="0.5" class=NUM
                                       prop:value=move || target_temp.get()
                                       on:input=move |ev| target_temp.set(event_target_value(&ev)) />
                                <span class="text-sm text-text-muted">"\u{00b0}C"</span>
                                <button type="submit" class="btn btn-ghost h-9 md:h-7 px-3">{move || tr().imaging_set}</button>
                            </form>
                        </div>
                    </Show>

                    <div class=CARD>
                        <span class=CARD_TITLE>
                            {move || format!("{} \u{00b7} {}", tr().imaging_sequence_queue, queue.with(Vec::len))}
                        </span>
                        {move || {
                            let (tr, live) = (tr(), live.get());
                            queue.with(|q| if q.is_empty() {
                                view! {
                                    <div class="py-6 px-4 text-center text-sm text-text-faint">{tr.imaging_empty_queue}</div>
                                }.into_any()
                            } else {
                                q.iter().enumerate()
                                    .map(|(i, job)| jobs::job_card(i, job, live, tr, open_job, remove))
                                    .collect::<Vec<_>>()
                                    .into_any()
                            })
                        }}
                        <div class="grid grid-cols-4 gap-2">
                            <button class="btn btn-ghost h-11 md:h-9 px-1" on:click=move |_| editor_open.set(true)>
                                {move || tr().imaging_editor}
                            </button>
                            <button class=move || if file_cmd.get() == LOAD { "btn btn-ghost btn--active h-11 md:h-9 px-1" } else { "btn btn-ghost h-11 md:h-9 px-1" }
                                    on:click=move |_| toggle_file(LOAD)>
                                {move || tr().imaging_load}
                            </button>
                            <button class=move || if file_cmd.get() == SAVE { "btn btn-ghost btn--active h-11 md:h-9 px-1" } else { "btn btn-ghost h-11 md:h-9 px-1" }
                                    disabled=move || queue.with(Vec::is_empty)
                                    on:click=move |_| toggle_file(SAVE)>
                                {move || tr().imaging_save}
                            </button>
                            <button class="btn btn-ghost h-11 md:h-9 px-1 text-state-err"
                                    disabled=move || queue.with(Vec::is_empty)
                                    on:click=plain("capture_clear_sequences")>
                                {move || tr().seq_clear}
                            </button>
                        </div>
                        // Paths are on the KStars host.
                        <Show when=move || !file_cmd.get().is_empty()>
                            <form class="flex items-center gap-2" on:submit=on_file.clone()>
                                <input type="text" class="input flex-1 min-w-0 h-11 md:h-9 font-mono text-sm"
                                       placeholder="/home/user/sequence.esq"
                                       prop:value=move || file_path.get()
                                       on:input=move |ev| file_path.set(event_target_value(&ev)) />
                                <button type="submit" class="btn btn-primary h-11 md:h-9 px-4"
                                        disabled=move || file_path.with(|p| p.trim().is_empty())>
                                    {move || if file_cmd.get() == SAVE { tr().imaging_save } else { tr().imaging_load }}
                                </button>
                            </form>
                        </Show>
                    </div>
                </div>
            </div>

            // Footer: Preview, Loop, Start / Stop.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <button class="btn btn-ghost h-11 px-4 max-md:flex-1 md:ml-auto" disabled=move || busy.get()
                        on:click=plain("capture_preview")>
                    {move || tr().preview}
                </button>
                <button class="btn btn-ghost h-11 px-4 max-md:flex-1" disabled=move || busy.get()
                        on:click=plain("capture_loop")>
                    {move || tr().focus_loop_btn}
                </button>
                <button class=move || if busy.get() {
                            "btn btn-danger h-11 px-5 font-semibold max-md:flex-1"
                        } else {
                            "btn btn-primary h-11 px-5 font-semibold max-md:flex-1"
                        }
                        disabled=move || !busy.get() && queue.with(Vec::is_empty)
                        on:click=on_run>
                    {move || if busy.get() {
                        format!("\u{25A0} {}", tr().imaging_stop)
                    } else {
                        format!("\u{25B6}\u{FE0E} {}", tr().imaging_start)
                    }}
                </button>
            </div>

            <Show when=move || editor_open.get()>
                {sheet(move || tr().imaging_sequence_editor, move || editor_open.set(false), editor_body())}
            </Show>
            <Show when=move || detail.get().is_some()>
                {sheet(move || tr().imaging_job_detail, move || detail.set(None), detail_body())}
            </Show>
            {frame_zoom(preview.into(), zoom_open, lang)}
        </div>
    }
}
