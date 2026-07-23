//! Dedicated Mosaic Planner tab.
//!
//! Workflow:
//!   1. User clicks [Pick on Sky] → Sky tab activates with a pick-center banner.
//!   2. User clicks the sky → center is set and this tab is restored.
//!   3. User configures grid / overlap / PA and an inline capture sequence.
//!   4. [Send to Scheduler] saves the ESQ file and imports all tiles as jobs.

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::astro;
use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::components::sequence_editor::{SeqFrame, SequenceEditor, build_esq_xml};
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::{ActiveTabCtx, MosaicPlannerCtx, Tab};

fn send_cmd(send: &SendCmd, type_str: &str, payload: serde_json::Value) {
    let msg = serde_json::json!({"type": type_str, "payload": payload}).to_string();
    send(msg);
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// RA degrees → "HH MM SS.SS" (space-separated, for KStars dmsBox)
fn fmt_hms(ra_deg: f64) -> String {
    let ra_h = ((ra_deg % 360.0) + 360.0) % 360.0 / 15.0;
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
    let tab_ctx = use_context::<ActiveTabCtx>();

    // ── Sequence rows ──────────────────────────────────────────────────────
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);
    // Destination folder for captured .fits; defaults from CaptureDirCtx.
    let seq_fits_dir: RwSignal<String> = RwSignal::new(String::new());

    // ── Startup flags ──────────────────────────────────────────────────────
    let step_track = RwSignal::new(true);
    let step_focus = RwSignal::new(false);
    let step_align = RwSignal::new(false);
    let step_guide = RwSignal::new(true);

    // ── Per-job scheduler options ──────────────────────────────────────────
    // Start: "asap" | "at"
    let startup_cond = RwSignal::new("asap".to_string());
    let startup_at   = RwSignal::new(String::new());
    // Completion: "sequence" | "repeat" | "loop"
    // KStars' FramingAssistant::importMosaic only honors FinishSequence /
    // FinishRepeat / FinishLoop — no FinishAt — so we don't expose that option.
    let completion_cond  = RwSignal::new("sequence".to_string());
    let completion_count = RwSignal::new("1".to_string());
    // Constraints
    let use_alt      = RwSignal::new(true);
    let min_alt      = RwSignal::new("30".to_string());
    let use_moon     = RwSignal::new(false);
    let min_moon     = RwSignal::new("0".to_string());
    let use_moon_alt = RwSignal::new(false);
    let moon_max_alt = RwSignal::new("90".to_string());
    let twilight     = RwSignal::new(true);
    let horizon      = RwSignal::new(true);

    // ── Error display ──────────────────────────────────────────────────────
    let form_error: RwSignal<Option<String>> = RwSignal::new(None);

    let send_s = Arc::clone(&send);

    let on_pick_sky = move |_| {
        planner.picking_center.set(true);
        if let Some(ctx) = tab_ctx { ctx.0.set(Tab::Sky); }
    };

    let on_send = move |_| {
        let Some((center_ra_deg, center_dec_deg)) = planner.center.get_untracked() else {
            form_error.set(Some(t(lang.get_untracked()).mosaic_err_no_center.to_string()));
            return;
        };
        let cam = camera.get_untracked();
        let fl  = focal_length_mm.get_untracked();
        let gw  = planner.grid_w.get_untracked();
        let gh  = planner.grid_h.get_untracked();
        let overlap = planner.overlap.get_untracked();
        let pa  = planner.pa.get_untracked();
        let target = planner.target.get_untracked();
        let dir = planner.dir.get_untracked();
        let home = home_dir.get_untracked();

        let frames_raw = seq_frames.get_untracked();
        let valid_frames: Vec<SeqFrame> = frames_raw.iter()
            .filter(|f| f.exposure.parse::<f64>().is_ok() && f.count.parse::<u32>().is_ok())
            .cloned()
            .collect();
        if valid_frames.is_empty() {
            form_error.set(Some(t(lang.get_untracked()).mosaic_err_no_frames.to_string()));
            return;
        }

        let (fl_mm, px_um, sw, sh) = match (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => {
                form_error.set(Some(t(lang.get_untracked()).mosaic_err_no_fov.to_string()));
                return;
            }
        };

        // Build Telescopius-format mosaic CSV for scheduler_import_mosaic.
        // KStars' parseMosaicCSV reads: Center row sets RA/DEC/PA/FOV/overlap;
        // tile rows are only counted for grid W×H (Row→W axis, Column→H axis).
        let fov_w = astro::fov_deg(fl_mm, sw as f64, px_um);
        let fov_h = astro::fov_deg(fl_mm, sh as f64, px_um);
        let fov_w_arcmin = fov_w * 60.0;
        let fov_h_arcmin = fov_h * 60.0;
        // planner.center is JNow, but KStars' parseMosaicCSV reads the CSV
        // RA/DEC into RA0/Dec0 (J2000) and precesses it forward again. Convert
        // JNow→J2000 here so the round-trip lands on the intended position
        // instead of a doubly-precessed one (~0.4° off in 2026).
        let now = js_sys::Date::new_0();
        let jd = astro::julian_date(
            now.get_utc_full_year() as i32,
            now.get_utc_month() + 1,
            now.get_utc_date(),
            now.get_utc_hours(),
            now.get_utc_minutes(),
            now.get_utc_seconds() as f64 + now.get_utc_milliseconds() as f64 / 1000.0,
        );
        let j2000 = crate::coords::JNow::new(center_ra_deg, center_dec_deg).to_j2000(jd);
        let center_ra_hms  = fmt_hms(j2000.ra_deg);
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

        // Bake the sanitized name straight into the capture folder path rather
        // than adding it as the target/object name (%T). KStars would otherwise
        // derive the subfolder from %T at runtime; putting it in the path keeps
        // all tiles of this mosaic under one predictable directory.
        let fits_root = seq_fits_dir.get_untracked();
        let fits_root = fits_root.trim().trim_end_matches('/');
        let mosaic_fits_dir = if fits_root.is_empty() {
            safe_name.clone()
        } else {
            format!("{}/{}", fits_root, safe_name)
        };
        let xml = build_esq_xml("", &mosaic_fits_dir, &valid_frames, false);
        if !home.is_empty() {
            send_cmd(&send_s, "file_directory_operation", serde_json::json!({
                "operation": "create",
                "path": format!("{}/.junos-sequences", home),
            }));
        }
        send_cmd(&send_s, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));

        // Resolve start-condition & completion-condition fields.
        let sc = startup_cond.get_untracked();
        let (asap_r, start_time_r, start_time_val) = if sc == "at" {
            (false, true, startup_at.get_untracked())
        } else {
            (true, false, String::new())
        };
        let (cc_literal, cc_arg) = match completion_cond.get_untracked().as_str() {
            "repeat" => ("FinishRepeat", completion_count.get_untracked()),
            "loop"   => ("FinishLoop",   "1".to_string()),
            _        => ("FinishSequence", "1".to_string()),
        };

        // Pre-load fields not accepted by `scheduler_import_mosaic` directly:
        // start condition + altitude/moon constraints land in the form first,
        // then importMosaic snapshots them into each tile job.
        send_cmd(&send_s, "scheduler_set_all_settings", serde_json::json!({
            "asapConditionR":        asap_r,
            "startupTimeConditionR": start_time_r,
            "startupTimeEdit":       start_time_val,
            "schedulerAltitude":              use_alt.get_untracked(),
            "schedulerAltitudeValue":         min_alt.get_untracked().parse::<f64>().unwrap_or(30.0),
            "schedulerMoonSeparation":        use_moon.get_untracked(),
            "schedulerMoonSeparationValue":   min_moon.get_untracked().parse::<f64>().unwrap_or(0.0),
            "schedulerMoonAltitude":          use_moon_alt.get_untracked(),
            "schedulerMoonAltitudeMaxValue":  moon_max_alt.get_untracked().parse::<f64>().unwrap_or(90.0),
            "schedulerTwilight":              twilight.get_untracked(),
            "schedulerHorizon":               horizon.get_untracked(),
        }));

        // KStars' importMosaic builds the capture subfolder as
        // `{directory}/{sanitized target}` and mkpath()s it itself. If the user
        // left the Output dir blank, fall back to the sequence destination
        // folder (then home) so the subfolder lands somewhere predictable
        // instead of KStars' silent ~/ fallback.
        let import_dir = {
            let d = dir.trim();
            if !d.is_empty() {
                d.to_string()
            } else {
                let fits = seq_fits_dir.get_untracked();
                let fits = fits.trim();
                if !fits.is_empty() { fits.to_string() } else { home.clone() }
            }
        };

        send_cmd(&send_s, "scheduler_import_mosaic", serde_json::json!({
            "csv":      csv,
            "sequence": abs_path,
            "target":   safe_name,
            "directory": import_dir,
            "track":    step_track.get_untracked(),
            "focus":    step_focus.get_untracked(),
            "align":    step_align.get_untracked(),
            "guide":    step_guide.get_untracked(),
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

    const INPUT_BASE: &str = "input input--sm font-mono";
    const SECTION_TITLE: &str = "text-sm font-bold text-text-blue pt-[6px] pb-1 border-b border-border-strong mb-2";
    const PARAM_LABEL: &str = "text-sm flex items-center gap-1";
    const TARGET_LABEL: &str = "text-sm flex items-center gap-[6px] flex-1 min-w-[200px]";

    view! {
        <div class="absolute inset-0 overflow-y-auto bg-bg text-text font-mono p-4 box-border">
        <div class="max-w-[680px] mx-auto flex flex-col gap-[18px]">

            // ── Header ──────────────────────────────────────────────────────
            <div class="text-[15px] font-bold text-text-dim pb-[6px] border-b border-border-mid">
                {move || tr().mosaic_planner_title}
            </div>

            // ── Target & Field ───────────────────────────────────────────────
            <div class="flex flex-col gap-2">
                <div class=SECTION_TITLE>{move || tr().mosaic_target_field}</div>

                // Target name + Pick on Sky button
                <div class="flex items-center gap-2 flex-wrap">
                    <label class=TARGET_LABEL>
                        {move || tr().mosaic_target_label}
                        <input type="text"
                               class=format!("{INPUT_BASE} flex-1")
                               placeholder=move || tr().mosaic_target_placeholder
                               prop:value=move || planner.target.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   planner.target.set(v);
                               } />
                    </label>
                    <button
                        class=move || {
                            let base = "btn btn--sm whitespace-nowrap";
                            if planner.picking_center.get() {
                                format!("{base} btn--active")
                            } else {
                                format!("{base} btn-primary")
                            }
                        }
                        on:click=on_pick_sky>
                        {move || {
                            if planner.picking_center.get() {
                                tr().mosaic_picking
                            } else if planner.center.get().is_some() {
                                tr().mosaic_repick
                            } else {
                                tr().mosaic_pick_sky
                            }
                        }}
                    </button>
                </div>

                // Center display
                {move || planner.center.get().map(|(ra_deg, dec_deg)| {
                    let ra_h = ra_deg / 15.0;
                    let rah  = ra_h as u32;
                    let ram  = ((ra_h - rah as f64) * 60.0).abs() as u32;
                    let dec_s = if dec_deg < 0.0 { "\u{2212}" } else { "+" };
                    let dec_abs = dec_deg.abs();
                    let decd = dec_abs as u32;
                    let decm = ((dec_abs - decd as f64) * 60.0) as u32;
                    view! {
                        <div class="text-sm text-text-blue py-[2px]">
                            {format!("Center: {:02}h {:02}m  {}{}\u{00b0} {:02}\u{2019}",
                                     rah, ram, dec_s, decd, decm)}
                        </div>
                    }
                })}

                // Grid + Overlap + PA
                <div class="flex items-center gap-[14px] flex-wrap">
                    <label class=PARAM_LABEL>
                        {move || tr().mosaic_grid_label}
                        <input type="number" min="1" max="10"
                               class=format!("{INPUT_BASE} w-[44px] text-center")
                               prop:value=move || planner.grid_w.get().to_string()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   if let Ok(n) = v.parse::<u32>() {
                                       planner.grid_w.set(n.clamp(1, 10));
                                   }
                               } />
                        {"\u{00d7}"}
                        <input type="number" min="1" max="10"
                               class=format!("{INPUT_BASE} w-[44px] text-center")
                               prop:value=move || planner.grid_h.get().to_string()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   if let Ok(n) = v.parse::<u32>() {
                                       planner.grid_h.set(n.clamp(1, 10));
                                   }
                               } />
                    </label>
                    <label class=PARAM_LABEL>
                        {move || tr().mosaic_overlap_label}
                        <input type="number" min="0" max="50" step="1"
                               class=format!("{INPUT_BASE} w-[50px]")
                               prop:value=move || format!("{:.0}", planner.overlap.get())
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   if let Ok(n) = v.parse::<f64>() {
                                       planner.overlap.set(n.clamp(0.0, 50.0));
                                   }
                               } />
                        {"%"}
                    </label>
                    <label class=PARAM_LABEL>
                        {move || tr().mosaic_pa_label}
                        <input type="number" min="-180" max="180" step="1"
                               class=format!("{INPUT_BASE} w-[56px]")
                               prop:value=move || format!("{:.0}", planner.pa.get())
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   if let Ok(n) = v.parse::<f64>() {
                                       planner.pa.set(n);
                                   }
                               } />
                        {"\u{00b0}"}
                    </label>
                </div>

                // FOV hint from camera + KStars persisted-equipment caveat.
                // KStars' parseMosaicCSV ignores the FOV columns we send; the
                // framing assistant computes spacing from its own persisted
                // focal length/pixel size/sensor size. Surface our values so
                // the user can cross-check, and warn about the mismatch path.
                {move || {
                    let cam = camera.get();
                    let fl  = focal_length_mm.get();
                    let no_fov_msg = tr().mosaic_cam_no_fov.to_string();
                    let kstars_note = tr().mosaic_kstars_fov_note.to_string();
                    if let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) =
                        (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
                    {
                        let fw = astro::fov_deg(fl_mm, sw as f64, px_um) * 60.0;
                        let fh = astro::fov_deg(fl_mm, sh as f64, px_um) * 60.0;
                        let gw = planner.grid_w.get() as f64;
                        let gh = planner.grid_h.get() as f64;
                        view! {
                            <div class="flex flex-col gap-[2px] py-[2px]">
                                <div class="text-sm text-[#557]">
                                    {format!("Tile: {fw:.1}\u{2019}\u{00d7}{fh:.1}\u{2019}   \
                                              Full field: {:.0}\u{2019}\u{00d7}{:.0}\u{2019}",
                                              fw * gw, fh * gh)}
                                </div>
                                <div class="text-sm text-[#557]">
                                    {format!("FL {fl_mm:.0} mm  \u{00b7}  Sensor {sw}\u{00d7}{sh} px @ {px_um:.2} \u{00b5}m")}
                                </div>
                                <div class="text-[12px] text-[#777] leading-snug">
                                    {kstars_note}
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="text-sm text-[#555] py-[2px]">
                                {no_fov_msg}
                            </div>
                        }.into_any()
                    }
                }}
            </div>

            // ── Capture Sequence ─────────────────────────────────────────────
            <div class="flex flex-col gap-2">
                <div class=SECTION_TITLE>{move || tr().mosaic_capture_seq}</div>
                <SequenceEditor frames=seq_frames fits_dir=seq_fits_dir camera=camera filter_wheel=filter_wheel />

                // Startup flags
                <div class="flex gap-4 flex-wrap text-sm pt-1">
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || step_track.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_track.set(c);
                               } />
                        {move || tr().mosaic_step_track}
                    </label>
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || step_focus.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_focus.set(c);
                               } />
                        {move || tr().mosaic_step_focus}
                    </label>
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || step_align.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_align.set(c);
                               } />
                        {move || tr().mosaic_step_align}
                    </label>
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || step_guide.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_guide.set(c);
                               } />
                        {move || tr().mosaic_step_guide}
                    </label>
                </div>
            </div>

            // ── Scheduler options ────────────────────────────────────────────
            <div class="flex flex-col gap-2">
                <div class=SECTION_TITLE>{move || tr().mosaic_scheduler_opts}</div>

                // Start when
                <div class="flex items-center gap-[14px] flex-wrap">
                    <label class=PARAM_LABEL>
                        {move || tr().sched_start_when}
                        <select class=format!("{INPUT_BASE} w-[140px]")
                                prop:value=move || startup_cond.get()
                                on:change=move |ev| {
                                    let v = ev.target().unwrap()
                                        .unchecked_into::<web_sys::HtmlSelectElement>().value();
                                    startup_cond.set(v);
                                }>
                            <option value="asap" selected=move || startup_cond.get() == "asap">
                                {move || tr().sched_cond_asap}
                            </option>
                            <option value="at" selected=move || startup_cond.get() == "at">
                                {move || tr().sched_cond_at_time}
                            </option>
                        </select>
                    </label>
                    {move || (startup_cond.get() == "at").then(|| view! {
                        <input type="datetime-local"
                               class=format!("{INPUT_BASE} w-[200px]")
                               prop:value=move || startup_at.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   startup_at.set(v);
                               } />
                    })}
                </div>

                // Complete when
                <div class="flex items-center gap-[14px] flex-wrap">
                    <label class=PARAM_LABEL>
                        {move || tr().sched_complete_when}
                        <select class=format!("{INPUT_BASE} w-[160px]")
                                prop:value=move || completion_cond.get()
                                on:change=move |ev| {
                                    let v = ev.target().unwrap()
                                        .unchecked_into::<web_sys::HtmlSelectElement>().value();
                                    completion_cond.set(v);
                                }>
                            <option value="sequence" selected=move || completion_cond.get() == "sequence">
                                {move || tr().sched_cond_seq}
                            </option>
                            <option value="repeat" selected=move || completion_cond.get() == "repeat">
                                {move || tr().sched_cond_repeat}
                            </option>
                            <option value="loop" selected=move || completion_cond.get() == "loop">
                                {move || tr().sched_cond_loop}
                            </option>
                        </select>
                    </label>
                    {move || (completion_cond.get() == "repeat").then(|| view! {
                        <label class=PARAM_LABEL>
                            <input type="number" min="1" step="1"
                                   class=format!("{INPUT_BASE} w-[60px]")
                                   prop:value=move || completion_count.get()
                                   on:input=move |ev| {
                                       let v = ev.target().unwrap()
                                           .unchecked_into::<web_sys::HtmlInputElement>().value();
                                       completion_count.set(v);
                                   } />
                            {move || tr().sched_times_unit}
                        </label>
                    })}
                </div>

                // Constraints
                <div class="flex items-center gap-[14px] flex-wrap">
                    <label class=PARAM_LABEL>
                        <input type="checkbox"
                               prop:checked=move || use_alt.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   use_alt.set(c);
                               } />
                        {move || tr().sched_min_alt}
                        <input type="number" min="0" max="90" step="1"
                               class=format!("{INPUT_BASE} w-[56px]")
                               prop:disabled=move || !use_alt.get()
                               prop:value=move || min_alt.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   min_alt.set(v);
                               } />
                        {"\u{00b0}"}
                    </label>
                    <label class=PARAM_LABEL>
                        <input type="checkbox"
                               prop:checked=move || use_moon.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   use_moon.set(c);
                               } />
                        {move || tr().sched_moon_sep}
                        <input type="number" min="0" max="180" step="1"
                               class=format!("{INPUT_BASE} w-[56px]")
                               prop:disabled=move || !use_moon.get()
                               prop:value=move || min_moon.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   min_moon.set(v);
                               } />
                        {"\u{00b0}"}
                    </label>
                    <label class=PARAM_LABEL>
                        <input type="checkbox"
                               prop:checked=move || use_moon_alt.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   use_moon_alt.set(c);
                               } />
                        {move || tr().sched_moon_max_alt}
                        <input type="number" min="0" max="90" step="1"
                               class=format!("{INPUT_BASE} w-[56px]")
                               prop:disabled=move || !use_moon_alt.get()
                               prop:value=move || moon_max_alt.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   moon_max_alt.set(v);
                               } />
                        {"\u{00b0}"}
                    </label>
                </div>

                // Constraints (toggles)
                <div class="flex items-center gap-4 flex-wrap text-sm">
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || twilight.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   twilight.set(c);
                               } />
                        {move || tr().sched_twilight}
                    </label>
                    <label class="flex items-center gap-1 cursor-pointer">
                        <input type="checkbox"
                               prop:checked=move || horizon.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   horizon.set(c);
                               } />
                        {move || tr().sched_horizon}
                    </label>
                </div>
            </div>

            // ── Output dir ────────────────────────────────────────────────────
            <div class="flex flex-col gap-2">
                <div class=SECTION_TITLE>{move || tr().mosaic_output}</div>
                <label class=TARGET_LABEL>
                    {move || tr().mosaic_output_dir}
                    <input type="text"
                           class=format!("{INPUT_BASE} flex-1")
                           placeholder=move || tr().mosaic_output_placeholder
                           prop:value=move || planner.dir.get()
                           on:input=move |ev| {
                               let v = ev.target().unwrap()
                                   .unchecked_into::<web_sys::HtmlInputElement>().value();
                               planner.dir.set(v);
                           } />
                </label>
            </div>

            // ── Error ──────────────────────────────────────────────────────────
            {move || form_error.get().map(|e| view! {
                <div class="text-state-err text-sm py-1">{e}</div>
            })}

            // ── Send button ────────────────────────────────────────────────────
            <div class="flex justify-end pb-6">
                <button
                    class="btn btn-primary px-7 font-bold"
                    disabled=move || planner.center.get().is_none()
                    on:click=on_send>
                    {move || tr().mosaic_send_scheduler}
                </button>
            </div>

        </div>
        </div>
    }
}
