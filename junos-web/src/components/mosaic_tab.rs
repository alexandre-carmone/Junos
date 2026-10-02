//! Mosaic Planner tab.
//!
//! Layout (phone-first, like Focus / Polar Align): a header, the cards in one
//! scrolling column — Target, Grid (with a live tile diagram), Capture
//! sequence, Scheduler — and a pinned footer with the tile count, the total
//! capture time and Send to Scheduler. From `md` up the cards split into two
//! columns.
//!
//! Workflow:
//!   1. [Pick on Sky] → the Sky tab shows a pick banner; tapping the sky sets
//!      the center and brings this tab back. The tab stays mounted
//!      (`tabs.rs`), so the sequence and options survive the round trip.
//!   2. Set grid / overlap / PA (previewed live on the sky) and the capture
//!      sequence shared by every tile.
//!   3. [Send to Scheduler] saves the ESQ file and imports all tiles as jobs.

use std::sync::Arc;

use leptos::prelude::*;

use crate::astro;
use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::components::form::{CARD, CARD_TITLE, FOOTER, JobOptions, LABEL, NUM, ROW};
use crate::components::sequence_editor::{SeqFrame, SequenceEditor, build_esq_xml, fmt_duration};
use crate::components::sky::{fmt_dec, fmt_ra, mosaic_span_am};
use crate::components::tab_wheel_icons::tab_icon;
use crate::dom::event_target_value;
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::{ActiveTabCtx, MosaicPlannerCtx, Tab};

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// RA degrees → "HH MM SS.SS" (space-separated, for KStars dmsBox).
/// Takes degrees, unlike `mount.rs::fmt_hms`, which takes hours.
fn fmt_ra_hms_from_deg(ra_deg: f64) -> String {
    let ra_h = ra_deg.rem_euclid(360.0) / 15.0;
    let h = ra_h.floor() as u32;
    let rem = (ra_h - h as f64) * 60.0;
    let m = rem.floor() as u32;
    let s = (rem - m as f64) * 60.0;
    format!("{:02} {:02} {:05.2}", h, m, s)
}

/// Dec degrees → "+DD MM SS.SS" (space-separated, for KStars dmsBox)
fn fmt_dms(dec_deg: f64) -> String {
    let sign = if dec_deg < 0.0 { "-" } else { "+" };
    let a = dec_deg.abs();
    let d = a.floor() as u32;
    let rem = (a - d as f64) * 60.0;
    let m = rem.floor() as u32;
    let s = (rem - m as f64) * 60.0;
    format!("{}{:02} {:02} {:05.2}", sign, d, m, s)
}

/// Arcmin below 1°, degrees above: "52.1′", "2.41°".
fn fmt_arc(am: f64) -> String {
    if am < 60.0 { format!("{am:.1}\u{2032}") } else { format!("{:.2}\u{00b0}", am / 60.0) }
}

#[component]
pub fn MosaicTab(
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    #[prop(into)] home_dir: Signal<String>,
    mosaic_tiles: RwSignal<Option<serde_json::Value>>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let planner = use_context::<MosaicPlannerCtx>()
        .expect("MosaicPlannerCtx not provided")
        .0;
    let p = planner.params;
    let tab_ctx = use_context::<ActiveTabCtx>();

    // ── Sequence rows ──────────────────────────────────────────────────────
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);
    // Capture folder (defaults from CaptureDirCtx); also the base directory
    // KStars puts each tile's folder under.
    let seq_fits_dir: RwSignal<String> = RwSignal::new(String::new());

    // Steps, start / completion and constraints, copied into every tile job.
    // KStars' FramingAssistant::importMosaic only honors FinishSequence /
    // FinishRepeat / FinishLoop — no FinishAt — so the view hides that option.
    let opts = JobOptions::new();

    let form_error: RwSignal<Option<String>> = RwSignal::new(None);

    // Tile FOV in arcmin, once the scope focal length and CCD_INFO are known.
    let tile_am = Memo::new(move |_| {
        let cam = camera.get();
        let (fl, px, sw, sh) =
            (focal_length_mm.get()?, cam.pixel_size_um?, cam.sensor_width?, cam.sensor_height?);
        Some((astro::fov_deg(fl, sw as f64, px) * 60.0, astro::fov_deg(fl, sh as f64, px) * 60.0))
    });

    // An error goes away once the inputs it was about change.
    Effect::new(move |_| {
        p.center.track();
        seq_frames.track();
        tile_am.track();
        form_error.set(None);
    });

    let on_pick_sky = move |_| {
        planner.picking_center.set(true);
        if let Some(ctx) = tab_ctx { ctx.0.set(Tab::Sky); }
    };
    let clear_center = move |_| {
        p.center.set(None);
        planner.planning.set(false);
    };

    let send_s = Arc::clone(&send);
    let on_send = move |_| {
        let tr = t(lang.get_untracked());
        let fail = |msg: &str| form_error.set(Some(msg.to_string()));
        let Some((center_ra_deg, center_dec_deg)) = p.center.get_untracked() else {
            return fail(tr.mosaic_err_no_center);
        };
        let valid_frames: Vec<SeqFrame> =
            seq_frames.with_untracked(|fs| fs.iter().filter(|f| f.is_valid()).cloned().collect());
        if valid_frames.is_empty() {
            return fail(tr.mosaic_err_no_frames);
        }
        let Some((fov_w_arcmin, fov_h_arcmin)) = tile_am.get_untracked() else {
            return fail(tr.mosaic_err_no_fov);
        };
        let gw  = p.grid_w.get_untracked();
        let gh  = p.grid_h.get_untracked();
        let overlap = p.overlap.get_untracked();
        let pa  = p.pa.get_untracked();
        let target = p.target.get_untracked();
        let home = home_dir.get_untracked();

        // Build Telescopius-format mosaic CSV for scheduler_import_mosaic.
        // KStars' parseMosaicCSV reads: Center row sets RA/DEC/PA/FOV/overlap;
        // tile rows are only counted for grid W×H (Row→W axis, Column→H axis).
        // p.center is JNow, but KStars' parseMosaicCSV reads the CSV RA/DEC
        // into RA0/Dec0 (J2000) and precesses it forward again. Convert
        // JNow→J2000 here so the round-trip lands on the intended position
        // instead of a doubly-precessed one (~0.4° off in 2026).
        let jd = astro::now_jd();
        let j2000 = crate::coords::JNow::new(center_ra_deg, center_dec_deg).to_j2000(jd);
        let center_ra_hms  = fmt_ra_hms_from_deg(j2000.ra_deg);
        let center_dec_dms = fmt_dms(j2000.dec_deg);
        let overlap_str = format!("{:.0}%", overlap);

        let mut csv = String::from(
            "Pane,RA,DEC,Position Angle (East),Pane width (arcmins),Pane height (arcmins),Overlap,Row,Column\n"
        );
        csv.push_str(&format!(
            "Center,{},{},{:.1},{:.2},{:.2},{},0,0\n",
            center_ra_hms, center_dec_dms, pa, fov_w_arcmin, fov_h_arcmin, overlap_str
        ));
        let mut pane_num = 1u32;
        for row in 0..gh {
            for col in 0..gw {
                // Row index maps to mosaic W axis, Column index to H axis.
                csv.push_str(&format!(
                    "Panel {},{},{},{:.1},{:.2},{:.2},{},{},{}\n",
                    pane_num, center_ra_hms, center_dec_dms, pa,
                    fov_w_arcmin, fov_h_arcmin, overlap_str,
                    col + 1, row + 1
                ));
                pane_num += 1;
            }
        }

        let safe_name = sanitize_name(if target.is_empty() { "mosaic" } else { &target });
        let rel_path  = format!(".junos-sequences/{}.esq", safe_name);
        let abs_path  = if home.is_empty() {
            rel_path.clone()
        } else {
            format!("{}/.junos-sequences/{}.esq", home, safe_name)
        };

        // Base capture directory (sequence destination → home). Each tile job
        // KStars generates is named `<safe_name>-Part_<N>`, and the `%T`
        // placeholder resolves to that job name at capture time, so the frames
        // land under `<base>/<safe_name>-Part_<N>/...`. We therefore leave the
        // base bare here (no `safe_name`) and let `%T` supply the per-tile
        // leaf folder.
        let import_base = {
            let fits = seq_fits_dir.get_untracked();
            let fits = fits.trim();
            if fits.is_empty() { home.clone() } else { fits.to_string() }
        };
        let import_base = import_base.trim_end_matches('/').to_string();
        // Emit a non-empty <TargetName> so KStars' createJobSequence rewrites it
        // per tile to `<safe_name>-Part_<N>`; the `%T` placeholder then resolves
        // to that job name at capture time. Leaving it empty drops the element,
        // so `%T` would resolve to nothing and every frame lands flat in the base.
        let xml = build_esq_xml(&safe_name, &import_base, &valid_frames, true);
        if !home.is_empty() {
            send_cmd(&send_s, "file_directory_operation", serde_json::json!({
                "operation": "create",
                "path": format!("{}/.junos-sequences", home),
            }));
        }
        send_cmd(&send_s, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));

        let (cc_literal, cc_arg) = match opts.complete_cond.get_untracked().as_str() {
            "repeat" => ("FinishRepeat", opts.complete_count.get_untracked()),
            "loop"   => ("FinishLoop",   "1".to_string()),
            _        => ("FinishSequence", "1".to_string()),
        };

        // Pre-load fields not accepted by `scheduler_import_mosaic` directly:
        // start condition + altitude/moon constraints land in the form first,
        // then importMosaic snapshots them into each tile job.
        send_cmd(&send_s, "scheduler_set_all_settings", opts.settings_json());

        // Hand KStars the bare base directory; the `<safe_name>-Part_<N>` leaf
        // comes from the `%T` placeholder baked into the sequence above.
        send_cmd(&send_s, "scheduler_import_mosaic", serde_json::json!({
            "csv":      csv,
            "sequence": abs_path,
            "target":   safe_name,
            "directory": import_base,
            "track":    opts.track.get_untracked(),
            "focus":    opts.focus.get_untracked(),
            "align":    opts.align.get_untracked(),
            "guide":    opts.guide.get_untracked(),
            "completionCondition":    cc_literal,
            "completionConditionArg": cc_arg,
        }));

        form_error.set(None);
        planner.planning.set(false);
        mosaic_tiles.set(None);
        if let Some(ctx) = tab_ctx { ctx.0.set(Tab::Scheduler); }

        let send_refresh = Arc::clone(&send_s);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(1500).await;
            send_cmd(&send_refresh, "scheduler_get_jobs", serde_json::json!({}));
            // KStars re-emits `new_mosaic_tiles` while processing import; drop
            // it again once the dust has settled so the planetarium overlay
            // doesn't come back.
            mosaic_tiles.set(None);
        });
    };

    // "9 tiles · 10 h 48 min" — every tile runs the whole sequence. On a
    // narrow footer it may only wrap at the "·" (non-breaking spaces).
    let summary = move || {
        let tr = tr();
        let n = p.grid_w.get() * p.grid_h.get();
        let secs: f64 = seq_frames.with(|fs| fs.iter().filter_map(SeqFrame::duration_secs).sum());
        let unit = if n == 1 { tr.mosaic_tile_unit } else { tr.mosaic_tiles_unit };
        let tiles = format!("{n}\u{00a0}{unit}");
        if secs > 0.0 {
            format!("{tiles} \u{00b7} {}", fmt_duration(secs * n as f64).replace(' ', "\u{00a0}"))
        } else {
            tiles
        }
    };

    // Tile / total field and the equipment they come from. KStars'
    // parseMosaicCSV ignores the FOV columns we send: its framing assistant
    // spaces tiles from its own persisted equipment, so show ours to
    // cross-check, plus the note on how to resync KStars.
    let fov_info = move || {
        let tr = tr();
        let cam = camera.get();
        let (Some((fw, fh)), Some(fl), Some(px), Some(sw), Some(sh)) = (
            tile_am.get(), focal_length_mm.get(),
            cam.pixel_size_um, cam.sensor_width, cam.sensor_height,
        ) else {
            return view! { <div class="text-xs text-state-warn">{tr.mosaic_cam_no_fov}</div> }.into_any();
        };
        let (tw, th) = mosaic_span_am(fw / 60.0, fh / 60.0, p.grid_w.get(), p.grid_h.get(), p.overlap.get());
        view! {
            <div class="flex flex-col gap-1 pt-2 border-t border-border-base">
                <div class="flex flex-wrap gap-x-4 font-mono text-sm text-text-dim">
                    <span>{format!("{} {} \u{00d7} {}", tr.framing_tile_fov, fmt_arc(fw), fmt_arc(fh))}</span>
                    <span>{format!("{} {} \u{00d7} {}", tr.framing_total_fov, fmt_arc(tw), fmt_arc(th))}</span>
                </div>
                <span class="font-mono text-xs text-text-muted">
                    {format!("FL {fl:.0} mm \u{00b7} {sw}\u{00d7}{sh} px @ {px:.2} \u{00b5}m")}
                </span>
                <span class="text-xs text-text-faint leading-snug">{tr.mosaic_kstars_fov_note}</span>
            </div>
        }.into_any()
    };

    view! {
        <div class="absolute inset-0 bg-bg text-text flex flex-col overflow-hidden">
            // Header
            <div class="shrink-0 flex items-center gap-2 min-h-[48px] px-3 md:pl-4 md:pr-6 pb-1.5 \
                        pt-[max(0.375rem,env(safe-area-inset-top))] border-b border-border-base bg-bg-elev-1">
                <span class="inline-block w-5 h-5 shrink-0 text-accent-cyan" inner_html=tab_icon(Tab::Mosaic)></span>
                <span class="min-w-0 truncate font-semibold text-text-blue-bright">{move || tr().mosaic_planner_title}</span>
            </div>

            // Cards — one column on phones, two from md.
            <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 md:pl-4 md:pr-6">
                <div class="max-w-[1100px] mx-auto grid gap-3 md:grid-cols-2 md:items-start">
                    <div class="min-w-0 flex flex-col gap-3">
                        // Target: name, center, pick on sky.
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().mosaic_target_label}</span>
                            <input type="text" class="input w-full min-w-0"
                                   placeholder=move || tr().mosaic_target_placeholder
                                   prop:value=move || p.target.get()
                                   on:input=move |ev| p.target.set(event_target_value(&ev)) />
                            <div class=ROW>
                                {move || match p.center.get() {
                                    Some((ra, dec)) => view! {
                                        <span class="flex-1 min-w-0 truncate font-mono text-sm text-text-blue">
                                            {format!("{}  {}", fmt_ra(ra), fmt_dec(dec))}
                                        </span>
                                        <button class="btn-icon shrink-0 text-text-muted"
                                                title=tr().mosaic_clear_center
                                                on:click=clear_center>"\u{2716}"</button>
                                    }.into_any(),
                                    None => view! {
                                        <span class="flex-1 text-sm text-text-faint">{tr().mosaic_no_center}</span>
                                    }.into_any(),
                                }}
                            </div>
                            <button
                                class=move || if p.center.get().is_some() {
                                    "btn btn-ghost w-full h-11"
                                } else {
                                    "btn btn-primary w-full h-11"
                                }
                                on:click=on_pick_sky>
                                {move || if p.center.get().is_some() { tr().mosaic_repick } else { tr().mosaic_pick_sky }}
                            </button>
                        </div>

                        // Grid: live diagram, size, overlap, PA, field.
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().mosaic_grid_label}</span>
                            <div class="relative h-40 md:h-48 p-2 rounded-md bg-bg-input-deep border border-border-base">
                                {move || tile_diagram(tile_am.get(), p.grid_w.get(), p.grid_h.get(), p.overlap.get(), p.pa.get())}
                                <span class="absolute top-1.5 left-2 font-mono text-[10px] text-text-faint">
                                    "N \u{2191}  E \u{2190}"
                                </span>
                            </div>
                            {stepper(move || tr().mosaic_cols, p.grid_w)}
                            {stepper(move || tr().mosaic_rows, p.grid_h)}
                            {slider_row(move || tr().mosaic_overlap_label, p.overlap, 0.0, 50.0, "%")}
                            {slider_row(move || tr().mosaic_pa_label, p.pa, -180.0, 180.0, "\u{00b0}")}
                            {fov_info}
                        </div>
                    </div>

                    <div class="min-w-0 flex flex-col gap-3">
                        // Capture sequence, run on every tile.
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().mosaic_capture_seq}</span>
                            <SequenceEditor frames=seq_frames fits_dir=seq_fits_dir camera=camera filter_wheel=filter_wheel />
                            <span class=format!("{CARD_TITLE} pt-1")>{move || tr().sched_steps_legend}</span>
                            {opts.steps_view(lang)}
                        </div>

                        // Scheduler options, copied into every tile job.
                        <div class=CARD>
                            <span class=CARD_TITLE>{move || tr().mosaic_scheduler_opts}</span>
                            {opts.conditions_view(lang, false)}
                            <span class=format!("{CARD_TITLE} pt-1")>{move || tr().sched_constraints_legend}</span>
                            {opts.constraints_view(lang)}
                        </div>
                    </div>
                </div>
            </div>

            // Footer: summary (or the last error) and the send button.
            <div class=format!("{FOOTER} md:pl-4 md:pr-6")>
                <div class="flex-1 min-w-0 text-sm leading-snug">
                    {move || match form_error.get() {
                        Some(e) => view! { <span class="text-state-err">{e}</span> }.into_any(),
                        None => view! { <span class="font-mono text-text-muted">{summary()}</span> }.into_any(),
                    }}
                </div>
                <button class="btn btn-primary shrink-0 h-11 px-5 font-semibold"
                        disabled=move || p.center.get().is_none()
                        on:click=on_send>
                    {move || tr().mosaic_send_scheduler}
                </button>
            </div>
        </div>
    }
}

/// − n + stepper for a grid dimension (1–10).
fn stepper(label: impl Fn() -> &'static str + Send + 'static, n: RwSignal<u32>) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            <button type="button" class="btn-icon" disabled=move || { n.get() <= 1 }
                    on:click=move |_| n.update(|v| *v = v.saturating_sub(1).max(1))>"\u{2212}"</button>
            <span class="w-7 text-center font-mono text-base">{move || n.get()}</span>
            <button type="button" class="btn-icon" disabled=move || { n.get() >= 10 }
                    on:click=move |_| n.update(|v| *v = (*v + 1).min(10))>"+"</button>
        </div>
    }
}

/// Label · slider · number input, both bound to one value (overlap %, PA °).
fn slider_row(
    label: impl Fn() -> &'static str + Send + 'static,
    v: RwSignal<f64>,
    min: f64,
    max: f64,
    unit: &'static str,
) -> impl IntoView {
    let set = move |ev: web_sys::Event| {
        if let Ok(x) = event_target_value(&ev).parse::<f64>() {
            v.set(x.clamp(min, max));
        }
    };
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} w-24 shrink-0")>{move || label()}</span>
            <input type="range" min=min.to_string() max=max.to_string() step="1"
                   class="flex-1 min-w-0 accent-accent-cyan"
                   prop:value=move || v.get().to_string()
                   on:input=set />
            <input type="number" min=min.to_string() max=max.to_string() step="1" class=NUM
                   prop:value=move || format!("{:.0}", v.get())
                   on:input=set />
            <span class="w-3 text-sm text-text-muted">{unit}</span>
        </div>
    }
}

/// Schematic tile layout, north up and east left: one translucent rectangle
/// per tile (overlaps read brighter), rotated by PA (east of north, so
/// counter-clockwise on screen). `tile_am` is the tile size in arcmin; a 3:2
/// placeholder stands in until the camera FOV is known.
fn tile_diagram(tile_am: Option<(f64, f64)>, gw: u32, gh: u32, overlap: f64, pa: f64) -> impl IntoView {
    let (fw, fh) = tile_am.unwrap_or((3.0, 2.0));
    let (w, h) = mosaic_span_am(fw / 60.0, fh / 60.0, gw, gh, overlap);
    let (dx, dy) = (fw * (1.0 - overlap / 100.0), fh * (1.0 - overlap / 100.0));
    // Bounding box of the rotated mosaic, with a margin.
    let (s, c) = pa.to_radians().sin_cos();
    let bw = (w * c.abs() + h * s.abs()) * 1.08;
    let bh = (w * s.abs() + h * c.abs()) * 1.08;
    let tiles = (0..gw)
        .flat_map(|i| (0..gh).map(move |j| (i, j)))
        .map(|(i, j)| view! {
            <rect x=format!("{:.3}", -w / 2.0 + i as f64 * dx)
                  y=format!("{:.3}", -h / 2.0 + j as f64 * dy)
                  width=format!("{fw:.3}") height=format!("{fh:.3}")
                  vector-effect="non-scaling-stroke" />
        })
        .collect::<Vec<_>>();
    view! {
        <svg viewBox=format!("{:.3} {:.3} {:.3} {:.3}", -bw / 2.0, -bh / 2.0, bw, bh)
             class="block w-full h-full">
            <g transform=format!("rotate({:.1})", -pa)
               fill="var(--accent-cyan)" fill-opacity="0.12"
               stroke="var(--accent-cyan)" stroke-width="1.2">
                {tiles}
            </g>
        </svg>
    }
}
