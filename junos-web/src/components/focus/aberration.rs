//! Aberration / tilt inspector — a faithful reproduction of Ekos'
//! `AberrationInspector`, orchestrated from the browser.
//!
//! KStars computes per-tile focus data internally and never sends it over Ekos
//! Live, so against a stock KStars we regenerate it ourselves: sweep the focuser
//! through N absolute positions, capture a real light frame at each, and ask the
//! server (`GET /api/files/tilt`) for the per-tile HFR of the resulting FITS. We
//! then fit a V-curve per tile and apply KStars' tilt/backfocus math (`abmath`).
//!
//! Caveats (surfaced in the UI): the run moves the focuser and **takes over the
//! capture queue** (replaces it with a temporary 1× Light job), and µm-per-step
//! is user-supplied because it is not on the wire.

use std::collections::HashMap;

use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen_futures::spawn_local;

use crate::compat::{CameraSnapshot, FocusSnapshot};
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::{dispatch_setting, send_cmd, send_device_property_set};

use super::abmath::{
    calc_backfocus, calc_tilt, fit_tile_min, BackfocusMode, Sample, TiltGeometry, TiltResult,
};

// ── Server response shapes ───────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct ListEntry {
    name: String,
    kind: String,
    #[serde(default)]
    ext: String,
    #[serde(default)]
    mtime: u64,
}

#[derive(Deserialize)]
struct ListReply {
    path: String,
    entries: Vec<ListEntry>,
}

#[derive(Deserialize)]
struct TileHfr {
    idx: usize,
    n_stars: usize,
    hfr: Option<f64>,
    cx: f64,
    cy: f64,
}

#[derive(Deserialize)]
struct TiltReply {
    naxis1: usize,
    naxis2: usize,
    tile_side_px: usize,
    tiles: Vec<TileHfr>,
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

async fn fetch_list(path: &str) -> Result<ListReply, String> {
    let url = format!("/api/files/list?path={}", enc(path));
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<ListReply>().await.map_err(|e| e.to_string())
}

async fn fetch_tilt(path: &str, tile_pct: f64) -> Result<TiltReply, String> {
    let url = format!("/api/files/tilt?path={}&tile_pct={}", enc(path), tile_pct);
    let resp = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() { return Err(format!("HTTP {}", resp.status())); }
    resp.json::<TiltReply>().await.map_err(|e| e.to_string())
}

/// Depth-limited walk of the captures sandbox collecting `(rel_path, mtime)` for
/// every FITS file. Bounded so a deep tree can't stall the sweep.
async fn collect_fits() -> Vec<(String, u64)> {
    const MAX_DIRS: usize = 96;
    const MAX_DEPTH: usize = 4;
    let mut out: Vec<(String, u64)> = Vec::new();
    let mut stack: Vec<(String, usize)> = vec![(String::new(), 0)];
    let mut visited = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if visited >= MAX_DIRS { break; }
        visited += 1;
        let Ok(reply) = fetch_list(&dir).await else { continue };
        let base = reply.path;
        for e in reply.entries {
            let rel = if base.is_empty() { e.name.clone() } else { format!("{}/{}", base, e.name) };
            if e.kind == "dir" {
                if depth < MAX_DEPTH { stack.push((rel, depth + 1)); }
            } else if matches!(e.ext.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts") {
                out.push((rel, e.mtime));
            }
        }
    }
    out
}

// ── Run state ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    Config,
    Running,
    Done,
    Aborted,
    Error,
}

#[derive(Clone)]
struct Outcome {
    tilt: TiltResult,
    backfocus: Option<f64>,
    hfr_at_best: [Option<f64>; 9],
    n_stars: [usize; 9],
    tile_side_px: usize,
    naxis: (usize, usize),
}

// ── Component ────────────────────────────────────────────────────────────────

#[component]
pub fn AberrationInspector(
    #[prop(into)] focus: Signal<FocusSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let open = RwSignal::new(false);
    let phase = RwSignal::new(Phase::Config);
    let status_msg = RwSignal::new(String::new());
    let progress = RwSignal::new((0usize, 0usize)); // (current, total)
    let abort = RwSignal::new(false);
    let outcome = RwSignal::new(None::<Outcome>);

    // Config inputs.
    let n_positions = RwSignal::new(7_i64);
    let step = RwSignal::new(200_i64);
    let um_per_step = RwSignal::new(5.0_f64);
    let exposure_s = RwSignal::new(2.0_f64);
    let tile_pct = RwSignal::new(25.0_f64);
    let mode_all = RwSignal::new(false); // false = outer corners

    let send_sv = StoredValue::new(send);

    // ── The sweep ─────────────────────────────────────────────────────────
    let run = move || {
        let send = send_sv.get_value();
        let fsnap = focus.get_untracked();
        let csnap = camera.get_untracked();

        let focuser = fsnap.device.clone();
        let start_pos = fsnap.position;
        if focuser.is_empty() || start_pos.is_none() {
            phase.set(Phase::Error);
            status_msg.set(tr().ab_err_no_focuser.to_string());
            return;
        }
        let (pixel_um, sensor_w, sensor_h) = match (csnap.pixel_size_um, csnap.sensor_width, csnap.sensor_height) {
            (Some(p), Some(w), Some(h)) if p > 0.0 && w > 0 && h > 0 => (p, w as f64, h as f64),
            _ => {
                phase.set(Phase::Error);
                status_msg.set(tr().ab_err_no_camera.to_string());
                return;
            }
        };

        let start_pos = start_pos.unwrap();
        let n = n_positions.get_untracked().clamp(5, 25);
        let step_v = step.get_untracked().max(1);
        let um = um_per_step.get_untracked().max(0.0);
        let exp = exposure_s.get_untracked().max(0.001);
        let tpct = tile_pct.get_untracked().clamp(5.0, 33.0);
        let mode = if mode_all.get_untracked() { BackfocusMode::All } else { BackfocusMode::OuterCorners };

        // Positions centred on the current focuser position.
        let positions: Vec<i64> = (0..n)
            .map(|k| start_pos + (k - (n - 1) / 2) * step_v)
            .collect();

        phase.set(Phase::Running);
        abort.set(false);
        outcome.set(None);
        progress.set((0, positions.len()));

        spawn_local(async move {
            // Force each Start to reset capture counts to zero, so a freshly
            // added 1× job runs even though matching frames already exist on disk
            // (otherwise KStars logs "…already 1/1 captures and does not need to
            // run"). Restored to KStars' default (false) when the sweep ends.
            send_cmd(&send, "option_set", serde_json::json!({
                "options": [{ "name": "alwaysResetSequenceWhenStarting", "value": true }]
            }));

            // Prime the capture queue: 1× Light at our exposure. This takes over
            // the capture queue (documented in the UI).
            dispatch_setting(&send, "capture_set_all_settings", None, "captureTypeS",
                serde_json::Value::String("Light".into()));
            dispatch_setting(&send, "capture_set_all_settings", None, "captureCountN",
                serde_json::json!(1));
            if let Some(num) = serde_json::Number::from_f64(exp) {
                dispatch_setting(&send, "capture_set_all_settings", None, "captureExposureN",
                    serde_json::Value::Number(num));
            }
            TimeoutFuture::new(300).await;

            let mut minima = [None; 9];
            let mut hfr_at_best = [None; 9];
            let mut n_stars = [0usize; 9];
            let mut samples: [Vec<(f64, f64)>; 9] = Default::default();
            let mut tile_side_px = 0usize;
            let mut naxis = (0usize, 0usize);
            let mut tile_centers = [(0.0, 0.0); 9];
            let mut had_error: Option<String> = None;

            'sweep: for (i, &pos) in positions.iter().enumerate() {
                if abort.get_untracked() { break; }
                progress.set((i + 1, positions.len()));

                // 1. Move the focuser to the absolute position.
                status_msg.set(format!("{} {}/{} — {}", tr().ab_position, i + 1, positions.len(), tr().ab_status_moving));
                send_device_property_set(&send, &focuser, "ABS_FOCUS_POSITION",
                    serde_json::json!([{ "name": "FOCUS_ABSOLUTE_POSITION", "value": pos }]));
                if !wait_focuser(focus, pos, abort).await {
                    if abort.get_untracked() { break; }
                    // Move timed out — press on; the driver may still be close.
                }

                // 2. Capture one saved light frame. Clear the queue, then Start:
                //    on an empty queue `capture_start` auto-creates one fresh IDLE
                //    batch job from the current settings and runs it
                //    (`CameraProcess::startNextPendingJob`). We deliberately do NOT
                //    `capture_add_sequence` — adding a batch job itself triggers a
                //    start, which would race our explicit Start and stall the module
                //    on the second position. A completed (DONE) job is never
                //    re-picked, so clearing first is essential.
                status_msg.set(format!("{} {}/{} — {}", tr().ab_position, i + 1, positions.len(), tr().ab_status_capturing));
                let baseline: HashMap<String, u64> = collect_fits().await.into_iter().collect();
                send_cmd(&send, "capture_stop", serde_json::json!({}));
                send_cmd(&send, "capture_clear_sequences", serde_json::json!({}));
                TimeoutFuture::new(300).await;
                send_cmd(&send, "capture_start", serde_json::json!({}));

                let new_file = wait_new_fits(&baseline, exp, abort).await;
                send_cmd(&send, "capture_stop", serde_json::json!({}));
                let Some(rel) = new_file else {
                    if abort.get_untracked() { break; }
                    had_error = Some(tr().ab_err_no_file.to_string());
                    break 'sweep;
                };

                // 3. Analyze the FITS into per-tile HFR.
                status_msg.set(format!("{} {}/{} — {}", tr().ab_position, i + 1, positions.len(), tr().ab_status_analyzing));
                match fetch_tilt(&rel, tpct).await {
                    Ok(reply) => {
                        tile_side_px = reply.tile_side_px;
                        naxis = (reply.naxis1, reply.naxis2);
                        for tile in &reply.tiles {
                            if tile.idx >= 9 { continue; }
                            // Tile centre offset from sensor centre, in microns.
                            tile_centers[tile.idx] = (
                                (tile.cx - reply.naxis1 as f64 / 2.0) * pixel_um,
                                (tile.cy - reply.naxis2 as f64 / 2.0) * pixel_um,
                            );
                            if let Some(hfr) = tile.hfr {
                                samples[tile.idx].push((pos as f64, hfr));
                                n_stars[tile.idx] = n_stars[tile.idx].max(tile.n_stars);
                            }
                        }
                    }
                    Err(e) => { had_error = Some(e); break 'sweep; }
                }
            }

            // Restore the focuser to where it started and undo the capture-count
            // reset option we forced on.
            status_msg.set(tr().ab_status_restoring.to_string());
            send_device_property_set(&send, &focuser, "ABS_FOCUS_POSITION",
                serde_json::json!([{ "name": "FOCUS_ABSOLUTE_POSITION", "value": start_pos }]));
            send_cmd(&send, "capture_stop", serde_json::json!({}));
            send_cmd(&send, "option_set", serde_json::json!({
                "options": [{ "name": "alwaysResetSequenceWhenStarting", "value": false }]
            }));

            if let Some(err) = had_error {
                status_msg.set(err);
                phase.set(Phase::Error);
                return;
            }
            if abort.get_untracked() {
                status_msg.set(tr().ab_status_aborted.to_string());
                phase.set(Phase::Aborted);
                return;
            }

            // Fit each tile's V-curve → best-focus position.
            for t in 0..9 {
                let s: Vec<Sample> = samples[t].iter().map(|&(p, h)| Sample { pos: p, hfr: h }).collect();
                minima[t] = fit_tile_min(&s);
                // Report the tile's HFR at the position nearest its minimum.
                if let (Some(m), false) = (minima[t], samples[t].is_empty()) {
                    hfr_at_best[t] = samples[t]
                        .iter()
                        .min_by(|a, b| (a.0 - m).abs().partial_cmp(&(b.0 - m).abs()).unwrap())
                        .map(|&(_, h)| h);
                }
            }

            let geo = TiltGeometry {
                step_microns: um,
                sensor_w_px: sensor_w,
                sensor_h_px: sensor_h,
                tile_w_px: tile_side_px as f64,
                pixel_um,
            };
            match calc_tilt(&minima, geo) {
                Some(tilt) => {
                    let backfocus = calc_backfocus(&minima, um, mode, &tile_centers);
                    outcome.set(Some(Outcome {
                        tilt, backfocus, hfr_at_best, n_stars,
                        tile_side_px, naxis,
                    }));
                    status_msg.set(tr().ab_status_done.to_string());
                    phase.set(Phase::Done);
                }
                None => {
                    status_msg.set(tr().ab_err_fit.to_string());
                    phase.set(Phase::Error);
                }
            }
        });
    };

    view! {
        <button
            class="btn btn-ghost col-span-2 !border-accent-cyan text-accent-cyan"
            on:click=move |_| { phase.set(Phase::Config); outcome.set(None); status_msg.set(String::new()); open.set(true); }
        >
            {move || tr().ab_open_btn}
        </button>

        <Show when=move || open.get()>
            <div
                class="fixed inset-0 z-[60] bg-[rgba(2,4,10,0.9)] backdrop-blur-sm flex items-center justify-center p-sp-4 max-[759px]:p-sp-2"
                on:click=move |_| { if phase.get() != Phase::Running { open.set(false); } }
            >
                <div
                    class="w-full max-w-[860px] max-h-full bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.5)] overflow-hidden flex flex-col"
                    on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                >
                    // Header
                    <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.85)]">
                        <h2 class="min-w-0 truncate text-accent-cyan text-sm uppercase tracking-[0.08em] m-0">{move || tr().ab_title}</h2>
                        <button
                            class="btn btn-ghost shrink-0"
                            disabled=move || phase.get() == Phase::Running
                            on:click=move |_| open.set(false)
                        >{move || tr().ab_close}</button>
                    </div>

                    <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-sp-4 text-sm">
                        {move || match phase.get() {
                            Phase::Config => config_view(tr(), n_positions, step, um_per_step, exposure_s, tile_pct, mode_all, run.clone()).into_any(),
                            Phase::Running => running_view(tr(), status_msg, progress, abort).into_any(),
                            _ => result_view(tr(), phase, status_msg, outcome, um_per_step, || {
                                // "Run again" resets to config.
                            }).into_any(),
                        }}
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// Poll the focuser position until it settles at `target` (±2 steps) or times
/// out (~20 s). Returns `true` if it settled. Bails early on abort.
async fn wait_focuser(focus: Signal<FocusSnapshot>, target: i64, abort: RwSignal<bool>) -> bool {
    for _ in 0..100 {
        if abort.get_untracked() { return false; }
        if let Some(p) = focus.get_untracked().position {
            if (p - target).abs() <= 2 { return true; }
        }
        TimeoutFuture::new(200).await;
    }
    false
}

/// Poll the captures sandbox for a FITS that is new relative to `baseline` —
/// either a path we hadn't seen, or a known path whose mtime advanced (guards
/// against same-name rewrites). Timeout scales with the exposure. Returns the
/// newest such file's relative path.
async fn wait_new_fits(baseline: &HashMap<String, u64>, exposure_s: f64, abort: RwSignal<bool>) -> Option<String> {
    let deadline_polls = ((exposure_s * 1000.0 + 30_000.0) / 500.0).ceil() as usize;
    for _ in 0..deadline_polls.max(20) {
        if abort.get_untracked() { return None; }
        TimeoutFuture::new(500).await;
        let mut fresh: Vec<(String, u64)> = collect_fits()
            .await
            .into_iter()
            .filter(|(p, m)| baseline.get(p).map_or(true, |&bm| *m > bm))
            .collect();
        if !fresh.is_empty() {
            fresh.sort_by_key(|(_, m)| *m);
            return fresh.pop().map(|(p, _)| p);
        }
    }
    None
}

// ── Views ────────────────────────────────────────────────────────────────────

fn num_field(
    label: &'static str,
    sig: RwSignal<i64>,
    min: i64,
) -> impl IntoView {
    view! {
        <label class="flex items-center justify-between gap-sp-2">
            <span class="text-text-blue min-w-0 truncate">{label}</span>
            <input type="number" min=min.to_string()
                class="input input--sm w-[104px] shrink-0 font-mono"
                prop:value=move || sig.get().to_string()
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).trim().parse::<i64>() { sig.set(v.max(min)); }
                }
            />
        </label>
    }
}

fn float_field(
    label: &'static str,
    sig: RwSignal<f64>,
    min: f64,
) -> impl IntoView {
    view! {
        <label class="flex items-center justify-between gap-sp-2">
            <span class="text-text-blue min-w-0 truncate">{label}</span>
            <input type="number" step="any" min=min.to_string()
                class="input input--sm w-[104px] shrink-0 font-mono"
                prop:value=move || sig.get().to_string()
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).trim().parse::<f64>() { sig.set(v.max(min)); }
                }
            />
        </label>
    }
}

fn config_view(
    tr: &'static crate::i18n::Translations,
    n_positions: RwSignal<i64>,
    step: RwSignal<i64>,
    um_per_step: RwSignal<f64>,
    exposure_s: RwSignal<f64>,
    tile_pct: RwSignal<f64>,
    mode_all: RwSignal<bool>,
    run: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <div class="grid grid-cols-2 gap-sp-3 max-[520px]:grid-cols-1">
            {num_field(tr.ab_config_positions, n_positions, 5)}
            {num_field(tr.ab_config_step, step, 1)}
            {float_field(tr.ab_config_um_per_step, um_per_step, 0.0)}
            {float_field(tr.ab_config_exposure, exposure_s, 0.001)}
            {float_field(tr.ab_config_tile_pct, tile_pct, 5.0)}
            <label class="flex items-center justify-between gap-sp-2">
                <span class="text-text-blue min-w-0 truncate">{tr.ab_config_backfocus_mode}</span>
                <select class="input input--sm shrink-0 w-[104px]"
                    on:change=move |ev| mode_all.set(event_target_value(&ev) == "all")
                >
                    <option value="corners" selected=move || !mode_all.get()>{tr.ab_mode_corners}</option>
                    <option value="all" selected=move || mode_all.get()>{tr.ab_mode_all}</option>
                </select>
            </label>
        </div>
        <p class="text-xs text-state-warn leading-snug">{tr.ab_note}</p>
        <button class="btn btn-primary" on:click=move |_| run()>{tr.ab_run}</button>
    }
}

fn running_view(
    tr: &'static crate::i18n::Translations,
    status_msg: RwSignal<String>,
    progress: RwSignal<(usize, usize)>,
    abort: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-sp-3 items-center py-sp-4">
            <div class="text-text-blue">{move || status_msg.get()}</div>
            <div class="w-full h-[8px] bg-bg-input-deep rounded-full overflow-hidden">
                <div class="h-full bg-accent-cyan transition-[width]"
                    style=move || {
                        let (c, t) = progress.get();
                        let pct = if t == 0 { 0.0 } else { c as f64 / t as f64 * 100.0 };
                        format!("width:{:.0}%", pct)
                    }
                ></div>
            </div>
            <div class="text-xs text-text-muted">{move || { let (c, t) = progress.get(); format!("{}/{}", c, t) }}</div>
            <button class="btn btn-danger" on:click=move |_| abort.set(true)>{tr.ab_abort}</button>
        </div>
    }
}

/// Blue→neutral→red heatmap colour for a per-tile delta in microns.
fn delta_color(delta: Option<f64>, max_abs: f64) -> String {
    match delta {
        None => "background:var(--bg-input-deep);color:#555".into(),
        Some(d) => {
            let t = if max_abs > 1e-6 { (d / max_abs).clamp(-1.0, 1.0) } else { 0.0 };
            // Positive (tile focuses inside centre) → warm; negative → cool.
            let (r, g, b) = if t >= 0.0 {
                (200.0, 200.0 - 140.0 * t, 200.0 - 180.0 * t)
            } else {
                (200.0 + 40.0 * t, 200.0 + 20.0 * t, 210.0)
            };
            format!("background:rgb({:.0},{:.0},{:.0});color:#08101c", r, g, b)
        }
    }
}

fn result_view(
    tr: &'static crate::i18n::Translations,
    phase: RwSignal<Phase>,
    status_msg: RwSignal<String>,
    outcome: RwSignal<Option<Outcome>>,
    um_per_step: RwSignal<f64>,
    _reset: impl Fn() + 'static,
) -> impl IntoView {
    view! {
        <div class="flex flex-col gap-sp-4">
            <div class=move || {
                let base = "text-sm px-sp-3 py-sp-2 rounded ";
                match phase.get() {
                    Phase::Done => format!("{base} text-state-ok bg-[rgba(40,180,90,0.12)]"),
                    Phase::Aborted => format!("{base} text-state-warn bg-[rgba(200,160,40,0.12)]"),
                    _ => format!("{base} text-state-err bg-[rgba(200,60,60,0.12)]"),
                }
            }>
                {move || status_msg.get()}
            </div>

            {move || match outcome.get() {
                None => view! { <div></div> }.into_any(),
                Some(o) => {
                    let max_abs = o.tilt.deltas.iter().filter_map(|d| d.map(f64::abs)).fold(0.0_f64, f64::max);
                    // 3×3 heatmap.
                    let cells = (0..9).map(|idx| {
                        let d = o.tilt.deltas[idx];
                        let hfr = o.hfr_at_best[idx];
                        let ns = o.n_stars[idx];
                        let style = delta_color(d, max_abs);
                        view! {
                            <div class="aspect-square rounded flex flex-col items-center justify-center gap-[2px] text-center" style=style>
                                <span class="text-sm font-semibold">
                                    {d.map(|v| format!("{:+.1}", v)).unwrap_or_else(|| "—".into())}
                                </span>
                                <span class="text-[10px] opacity-80">
                                    {hfr.map(|h| format!("HFR {:.2}", h)).unwrap_or_default()}
                                </span>
                                <span class="text-[10px] opacity-70">{format!("{} ★", ns)}</span>
                            </div>
                        }
                    }).collect::<Vec<_>>();

                    let t = &o.tilt;
                    let bf = o.backfocus;
                    let bf_hint = bf.map(|v| if v >= 0.0 { tr.ab_move_sensor_in } else { tr.ab_move_sensor_out });
                    let naxis = o.naxis;
                    let side = o.tile_side_px;
                    let um = um_per_step.get();

                    view! {
                        <div class="grid grid-cols-3 gap-sp-2 max-w-[320px] mx-auto w-full">{cells}</div>
                        <div class="text-[11px] text-text-muted text-center break-words">
                            {format!("{}×{} px · tile {} px · {:.1} µm/step", naxis.0, naxis.1, side, um)}
                        </div>
                        <div class="grid grid-cols-2 gap-sp-3 max-[520px]:grid-cols-1">
                            {summary_row(tr.ab_result_tilt_lr, format!("{:+.1} µm / {:+.2}%", t.lr_microns, t.lr_pct))}
                            {summary_row(tr.ab_result_tilt_tb, format!("{:+.1} µm / {:+.2}%", t.tb_microns, t.tb_pct))}
                            {summary_row(tr.ab_result_diagonal, format!("{:.1} µm / {:.2}%", t.diag_microns, t.diag_pct))}
                            {summary_row(tr.ab_result_backfocus,
                                bf.map(|v| format!("{:+.1} µm ({})", v, bf_hint.unwrap_or("")))
                                  .unwrap_or_else(|| "—".into()))}
                        </div>
                        <button class="btn btn-ghost" on:click=move |_| phase.set(Phase::Config)>{tr.ab_run_again}</button>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn summary_row(label: &'static str, value: String) -> impl IntoView {
    view! {
        <div class="flex items-center justify-between gap-sp-2 px-sp-3 py-sp-2 bg-bg-input-deep rounded">
            <span class="text-text-blue shrink-0">{label}</span>
            <span class="font-mono text-text text-right break-words min-w-0">{value}</span>
        </div>
    }
}

fn event_target_value(ev: &web_sys::Event) -> String {
    use wasm_bindgen::JsCast;
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.value())
        .or_else(|| ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok()).map(|el| el.value()))
        .unwrap_or_default()
}
