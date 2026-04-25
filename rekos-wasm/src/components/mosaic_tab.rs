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
use crate::compat::CameraSnapshot;
use crate::components::scheduler::{SeqFrame, build_esq_xml};
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
    #[prop(into)] focal_length_mm: Signal<Option<f64>>,
    #[prop(into)] home_dir: Signal<String>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let _lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let _tr = move || t(_lang.get());

    let planner = use_context::<MosaicPlannerCtx>()
        .expect("MosaicPlannerCtx not provided")
        .0;
    let tab_ctx = use_context::<ActiveTabCtx>();

    // ── Sequence rows ──────────────────────────────────────────────────────
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);

    // ── Startup flags ──────────────────────────────────────────────────────
    let step_track = RwSignal::new(true);
    let step_focus = RwSignal::new(false);
    let step_align = RwSignal::new(false);
    let step_guide = RwSignal::new(true);

    // ── Error display ──────────────────────────────────────────────────────
    let form_error: RwSignal<Option<String>> = RwSignal::new(None);

    let send_s = Arc::clone(&send);

    let on_pick_sky = move |_| {
        planner.picking_center.set(true);
        if let Some(ctx) = tab_ctx { ctx.0.set(Tab::Sky); }
    };

    let on_send = move |_| {
        let Some((center_ra_deg, center_dec_deg)) = planner.center.get_untracked() else {
            form_error.set(Some("Pick a center on the sky first.".to_string()));
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
            form_error.set(Some("Add at least one sequence row with numeric exposure and count.".to_string()));
            return;
        }

        let (fl_mm, px_um, sw, sh) = match (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height) {
            (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
            _ => {
                form_error.set(Some("Camera FOV unknown — connect camera and wait for CCD_INFO.".to_string()));
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
        let center_ra_hms  = fmt_hms(center_ra_deg);
        let center_dec_dms = fmt_dms(center_dec_deg);
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
        let rel_path  = format!(".rekos-sequences/{}.esq", safe_name);
        let abs_path  = if home.is_empty() {
            rel_path.clone()
        } else {
            format!("{}/.rekos-sequences/{}.esq", home, safe_name)
        };

        let xml = build_esq_xml(&safe_name, &valid_frames);
        send_cmd(&send_s, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));

        send_cmd(&send_s, "scheduler_import_mosaic", serde_json::json!({
            "csv":      csv,
            "sequence": abs_path,
            "target":   safe_name,
            "directory": dir,
            "track":    step_track.get_untracked(),
            "focus":    step_focus.get_untracked(),
            "align":    step_align.get_untracked(),
            "guide":    step_guide.get_untracked(),
            "completionCondition":    "sequence",
            "completionConditionArg": "1",
        }));

        form_error.set(None);
        if let Some(ctx) = tab_ctx { ctx.0.set(Tab::Scheduler); }

        let send_refresh = Arc::clone(&send_s);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(1500).await;
            send_cmd(&send_refresh, "scheduler_get_jobs", serde_json::json!({}));
        });
    };

    // ── Common styles ──────────────────────────────────────────────────────
    let input_style = "background:#111; color:#ccc; border:1px solid #444; \
                       font-family:monospace; font-size:12px; padding:3px 5px; box-sizing:border-box;";
    let section_title = "font-size:12px; font-weight:bold; color:#88aaff; \
                         padding:6px 0 4px; border-bottom:1px solid #2a2a3a; margin-bottom:8px;";

    view! {
        <div style="position:absolute; inset:0; overflow-y:auto; background:#0a0a0f; \
                    color:#c0c0d0; font-family:monospace; padding:16px; box-sizing:border-box;">
        <div style="max-width:680px; margin:0 auto; display:flex; flex-direction:column; gap:18px;">

            // ── Header ──────────────────────────────────────────────────────
            <div style="font-size:15px; font-weight:bold; color:#cfe0ff; padding-bottom:6px; \
                         border-bottom:1px solid #333;">
                {"Mosaic Planner"}
            </div>

            // ── Target & Field ───────────────────────────────────────────────
            <div style="display:flex; flex-direction:column; gap:8px;">
                <div style=section_title>{"Target & Field"}</div>

                // Target name + Pick on Sky button
                <div style="display:flex; align-items:center; gap:8px; flex-wrap:wrap;">
                    <label style="font-size:12px; display:flex; align-items:center; gap:6px; flex:1; min-width:200px;">
                        {"Target:"}
                        <input type="text"
                               style=format!("{input_style} flex:1;")
                               placeholder="e.g. M31"
                               prop:value=move || planner.target.get()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   planner.target.set(v);
                               } />
                    </label>
                    <button
                        style=move || {
                            if planner.picking_center.get() {
                                "padding:5px 12px; background:#003030; color:#00ffff; \
                                 border:1px solid #00cccc; cursor:pointer; \
                                 font-family:monospace; font-size:12px; border-radius:3px; white-space:nowrap;"
                            } else {
                                "padding:5px 12px; background:#0a1a2a; color:#88aaff; \
                                 border:1px solid #446; cursor:pointer; \
                                 font-family:monospace; font-size:12px; border-radius:3px; white-space:nowrap;"
                            }
                        }
                        on:click=on_pick_sky>
                        {move || {
                            if planner.picking_center.get() {
                                "Picking\u{2026}"
                            } else if planner.center.get().is_some() {
                                "Re-pick on Sky"
                            } else {
                                "Pick on Sky"
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
                        <div style="font-size:11px; color:#88aaff; padding:2px 0;">
                            {format!("Center: {:02}h {:02}m  {}{}\u{00b0} {:02}\u{2019}",
                                     rah, ram, dec_s, decd, decm)}
                        </div>
                    }
                })}

                // Grid + Overlap + PA
                <div style="display:flex; align-items:center; gap:14px; flex-wrap:wrap;">
                    <label style="font-size:12px; display:flex; align-items:center; gap:4px;">
                        {"Grid:"}
                        <input type="number" min="1" max="10"
                               style=format!("{input_style} width:44px; text-align:center;")
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
                               style=format!("{input_style} width:44px; text-align:center;")
                               prop:value=move || planner.grid_h.get().to_string()
                               on:input=move |ev| {
                                   let v = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().value();
                                   if let Ok(n) = v.parse::<u32>() {
                                       planner.grid_h.set(n.clamp(1, 10));
                                   }
                               } />
                    </label>
                    <label style="font-size:12px; display:flex; align-items:center; gap:4px;">
                        {"Overlap:"}
                        <input type="number" min="0" max="50" step="1"
                               style=format!("{input_style} width:50px;")
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
                    <label style="font-size:12px; display:flex; align-items:center; gap:4px;">
                        {"PA:"}
                        <input type="number" min="-180" max="180" step="1"
                               style=format!("{input_style} width:56px;")
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

                // FOV hint from camera
                {move || {
                    let cam = camera.get();
                    let fl  = focal_length_mm.get();
                    if let (Some(fl_mm), Some(px_um), Some(sw), Some(sh)) =
                        (fl, cam.pixel_size_um, cam.sensor_width, cam.sensor_height)
                    {
                        let fw = astro::fov_deg(fl_mm, sw as f64, px_um) * 60.0;
                        let fh = astro::fov_deg(fl_mm, sh as f64, px_um) * 60.0;
                        let gw = planner.grid_w.get() as f64;
                        let gh = planner.grid_h.get() as f64;
                        view! {
                            <div style="font-size:11px; color:#557; padding:2px 0;">
                                {format!("Tile: {fw:.0}\u{2019}\u{00d7}{fh:.0}\u{2019}   \
                                          Full field: {:.0}\u{2019}\u{00d7}{:.0}\u{2019}",
                                          fw * gw, fh * gh)}
                            </div>
                        }
                    } else {
                        view! {
                            <div style="font-size:11px; color:#555; padding:2px 0;">
                                {format!("Camera not connected \u{2014} tile FOV unknown")}
                            </div>
                        }
                    }
                }}
            </div>

            // ── Capture Sequence ─────────────────────────────────────────────
            <div style="display:flex; flex-direction:column; gap:8px;">
                <div style=section_title>{"Capture Sequence (same for every tile)"}</div>

                // Column headers
                <div style="display:grid; grid-template-columns:1fr 80px 60px 28px; \
                            gap:4px; font-size:11px; color:#666; padding:0 2px 4px;">
                    <span>{"Filter"}</span>
                    <span>{"Exp (s)"}</span>
                    <span>{"Count"}</span>
                    <span></span>
                </div>

                // Rows — re-rendered when frames list changes (add/remove)
                {move || {
                    seq_frames.get().into_iter().enumerate().map(|(idx, frame)| {
                        let fi = frame.filter.clone();
                        let ex = frame.exposure.clone();
                        let co = frame.count.clone();
                        view! {
                            <div style="display:grid; grid-template-columns:1fr 80px 60px 28px; gap:4px; margin-bottom:2px;">
                                <input type="text"
                                       style=format!("{input_style} width:100%;")
                                       placeholder="Lum"
                                       prop:value=fi
                                       on:input=move |ev| {
                                           let v = ev.target().unwrap()
                                               .unchecked_into::<web_sys::HtmlInputElement>().value();
                                           seq_frames.update(|fs| {
                                               if let Some(f) = fs.get_mut(idx) { f.filter = v; }
                                           });
                                       } />
                                <input type="number" min="1" step="1"
                                       style=format!("{input_style} width:100%;")
                                       prop:value=ex
                                       on:input=move |ev| {
                                           let v = ev.target().unwrap()
                                               .unchecked_into::<web_sys::HtmlInputElement>().value();
                                           seq_frames.update(|fs| {
                                               if let Some(f) = fs.get_mut(idx) { f.exposure = v; }
                                           });
                                       } />
                                <input type="number" min="1"
                                       style=format!("{input_style} width:100%;")
                                       prop:value=co
                                       on:input=move |ev| {
                                           let v = ev.target().unwrap()
                                               .unchecked_into::<web_sys::HtmlInputElement>().value();
                                           seq_frames.update(|fs| {
                                               if let Some(f) = fs.get_mut(idx) { f.count = v; }
                                           });
                                       } />
                                <button
                                    style="background:#1a0a0a; color:#cc4444; border:1px solid #422; \
                                           cursor:pointer; font-family:monospace; font-size:13px; \
                                           border-radius:2px; padding:0 4px; line-height:1;"
                                    on:click=move |_| {
                                        seq_frames.update(|fs| {
                                            if fs.len() > 1 { fs.remove(idx); }
                                        });
                                    }>
                                    {"\u{00d7}"}
                                </button>
                            </div>
                        }
                    }).collect::<Vec<_>>()
                }}

                // Add filter row
                <button
                    style="align-self:flex-start; margin-top:4px; padding:3px 10px; \
                           background:#0a1a0a; color:#44cc44; border:1px solid #264; \
                           cursor:pointer; font-family:monospace; font-size:12px; border-radius:3px;"
                    on:click=move |_| {
                        seq_frames.update(|fs| fs.push(SeqFrame::default()));
                    }>
                    {"+ Add filter"}
                </button>

                // Startup flags
                <div style="display:flex; gap:16px; flex-wrap:wrap; font-size:12px; padding-top:4px;">
                    <label style="display:flex; align-items:center; gap:4px; cursor:pointer;">
                        <input type="checkbox"
                               prop:checked=move || step_track.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_track.set(c);
                               } />
                        {"Track"}
                    </label>
                    <label style="display:flex; align-items:center; gap:4px; cursor:pointer;">
                        <input type="checkbox"
                               prop:checked=move || step_focus.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_focus.set(c);
                               } />
                        {"Focus"}
                    </label>
                    <label style="display:flex; align-items:center; gap:4px; cursor:pointer;">
                        <input type="checkbox"
                               prop:checked=move || step_align.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_align.set(c);
                               } />
                        {"Align"}
                    </label>
                    <label style="display:flex; align-items:center; gap:4px; cursor:pointer;">
                        <input type="checkbox"
                               prop:checked=move || step_guide.get()
                               on:change=move |ev| {
                                   let c = ev.target().unwrap()
                                       .unchecked_into::<web_sys::HtmlInputElement>().checked();
                                   step_guide.set(c);
                               } />
                        {"Guide"}
                    </label>
                </div>
            </div>

            // ── Output dir ────────────────────────────────────────────────────
            <div style="display:flex; flex-direction:column; gap:6px;">
                <div style=section_title>{"Output"}</div>
                <label style="font-size:12px; display:flex; align-items:center; gap:6px;">
                    {"Output dir:"}
                    <input type="text"
                           style=format!("{input_style} flex:1;")
                           placeholder="~/observations"
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
                <div style="color:#ff6666; font-size:12px; padding:4px 0;">{e}</div>
            })}

            // ── Send button ────────────────────────────────────────────────────
            <div style="display:flex; justify-content:flex-end; padding-bottom:24px;">
                <button
                    style=move || {
                        if planner.center.get().is_some() {
                            "padding:10px 28px; background:#0a1a2a; color:#88aaff; \
                             border:1px solid #446; cursor:pointer; font-family:monospace; \
                             font-size:13px; font-weight:bold; border-radius:4px;"
                        } else {
                            "padding:10px 28px; background:#111; color:#445; \
                             border:1px solid #333; cursor:not-allowed; font-family:monospace; \
                             font-size:13px; font-weight:bold; border-radius:4px;"
                        }
                    }
                    disabled=move || planner.center.get().is_none()
                    on:click=on_send>
                    {"Send to Scheduler"}
                </button>
            </div>

        </div>
        </div>
    }
}
