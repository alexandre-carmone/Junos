//! Ekos Scheduler tab — view the job queue, start/stop the scheduler,
//! remove jobs, and add new jobs with a visual sequence builder.
//!
//! Inbound:  `new_scheduler_state`, `scheduler_get_jobs`,
//!           `scheduler_get_all_settings`
//! Outbound: `scheduler_start_job`, `scheduler_remove_jobs`,
//!           `scheduler_set_all_settings` + `scheduler_add_jobs`,
//!           `scheduler_save_sequence_file`

use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::MouseEvent;

mod labels;
mod mapping;
mod view_add_job;
mod view_jobs;
mod view_scripts;
mod view_settings;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot, SchedulerSnapshot};
use crate::components::sequence_editor::{SeqFrame, build_esq_xml};
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{Lang, t};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;
use crate::SchedulerPrefillCtx;
use labels::{dec_to_dms, ra_to_hms, sanitize_name};
use mapping::{resolve_completion_condition, resolve_startup_condition};
use view_add_job::SchedulerAddJobSection;
use view_jobs::{SchedulerJobsSection, SchedulerToolbar};
use view_scripts::SchedulerScriptsSection;
use view_settings::SchedulerSettingsSection;

#[component]
pub fn SchedulerTab(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // ── Overlay state ───────────────────────────────────────────────────────
    let add_open      = RwSignal::new(false);
    let settings_open = RwSignal::new(false);

    // Escape closes whichever overlay is open.
    {
        let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
            move |e: web_sys::KeyboardEvent| {
                if e.key() == "Escape" {
                    if add_open.get_untracked()      { add_open.set(false); }
                    if settings_open.get_untracked() { settings_open.set(false); }
                }
            },
        );
        if let Some(win) = web_sys::window() {
            let _ = win.add_event_listener_with_callback(
                "keydown",
                cb.as_ref().unchecked_ref(),
            );
        }
        cb.forget();
    }

    let on_open_add: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || add_open.set(true));
    let on_open_settings: Arc<dyn Fn() + Send + Sync> =
        Arc::new(move || settings_open.set(true));

    let dso_catalog = use_context::<RwSignal<Option<std::sync::Arc<DsoCatalogData>>>>();

    // ── Observatory scripts ─────────────────────────────────────────────────
    let startup_enabled  = RwSignal::new(false);
    let pre_startup      = RwSignal::new(String::new());
    let post_startup     = RwSignal::new(String::new());
    let shutdown_enabled = RwSignal::new(false);
    let pre_shutdown     = RwSignal::new(String::new());
    let post_shutdown    = RwSignal::new(String::new());

    // ── Global scheduler settings ───────────────────────────────────────────
    let greedy         = RwSignal::new(false);
    let remember_prog  = RwSignal::new(true);
    let reschedule_err = RwSignal::new(false);

    // Pre-populate from KStars settings once they arrive
    let settings_populated = RwSignal::new(false);
    Effect::new(move |_| {
        if settings_populated.get_untracked() { return; }
        let s = scheduler.get().settings;
        if !s.is_object() { return; }
        if let Some(v) = s["schedulerStartupEnabled"].as_bool()           { startup_enabled.set(v); }
        if let Some(v) = s["schedulerPreStartupScript"].as_str()          { pre_startup.set(v.to_string()); }
        if let Some(v) = s["schedulerPostStartupScript"].as_str()         { post_startup.set(v.to_string()); }
        if let Some(v) = s["schedulerShutdownEnabled"].as_bool()          { shutdown_enabled.set(v); }
        if let Some(v) = s["schedulerPreShutdownScript"].as_str()         { pre_shutdown.set(v.to_string()); }
        if let Some(v) = s["schedulerPostShutdownScript"].as_str()        { post_shutdown.set(v.to_string()); }
        if let Some(v) = s["kcfg_GreedyScheduling"].as_bool()            { greedy.set(v); }
        if let Some(v) = s["kcfg_RememberJobProgress"].as_bool()         { remember_prog.set(v); }
        if let Some(v) = s["errorHandlingRescheduleErrorsCB"].as_bool()   { reschedule_err.set(v); }
        settings_populated.set(true);
    });

    // ── Add-job form state ──────────────────────────────────────────────────
    let f_target_name = RwSignal::new(String::new());
    let f_ra_h        = RwSignal::new(String::new());
    let f_dec_deg     = RwSignal::new(String::new());
    let f_min_alt     = RwSignal::new("30".to_string());
    let f_min_moon    = RwSignal::new("0".to_string());
    let f_pa          = RwSignal::new("0".to_string());
    let search_result = RwSignal::new(Option::<String>::None);
    let form_error    = RwSignal::new(Option::<String>::None);

    // Pre-fill from sky right-click "Add to Scheduler" action.
    let prefill_ctx = use_context::<SchedulerPrefillCtx>();
    Effect::new(move |_| {
        let Some(pctx) = prefill_ctx else { return };
        let Some((name, ra_deg, dec_deg)) = pctx.0.get() else { return };
        let ra_h = ra_deg / 15.0;
        f_target_name.set(name);
        f_ra_h.set(format!("{:.6}", ra_h));
        f_dec_deg.set(format!("{:.6}", dec_deg));
        pctx.0.set(None);  // consume
    });

    // Derived HMS/DMS hint shown after RA/Dec are populated
    let coords_hint = Signal::derive(move || {
        let ra  = f_ra_h.get().parse::<f64>().ok()?;
        let dec = f_dec_deg.get().parse::<f64>().ok()?;
        if ra < 0.0 || ra > 24.0 { return None; }
        if dec < -90.0 || dec > 90.0 { return None; }
        Some(format!("{} / {}", ra_to_hms(ra), dec_to_dms(dec)))
    });

    // Per-job step pipeline (default: Track + Guide)
    let step_track = RwSignal::new(true);
    let step_focus = RwSignal::new(false);
    let step_align = RwSignal::new(false);
    let step_guide = RwSignal::new(true);

    // Per-job startup condition: "asap" | "at"
    let startup_cond = RwSignal::new("asap".to_string());
    let startup_at   = RwSignal::new(String::new());

    // Per-job completion condition: "sequence" | "repeat" | "loop" | "at"
    let completion_cond  = RwSignal::new("sequence".to_string());
    let completion_count = RwSignal::new("1".to_string());
    let completion_at    = RwSignal::new(String::new());

    // Sequence frames — start with one default Light row
    let seq_frames: RwSignal<Vec<SeqFrame>> = RwSignal::new(vec![SeqFrame::default()]);

    // Derived: total exposure summary
    let seq_total_hint = Signal::derive(move || {
        let frames = seq_frames.get();
        let total_secs: f64 = frames.iter()
            .filter_map(|f| {
                let exp  = f.exposure.parse::<f64>().ok()?;
                let cnt  = f.count.parse::<f64>().ok()?;
                Some(exp * cnt)
            })
            .sum();
        if total_secs <= 0.0 { return String::new(); }
        if total_secs < 60.0 {
            format!("Total: {:.0} s", total_secs)
        } else if total_secs < 3600.0 {
            format!("Total: {:.1} min", total_secs / 60.0)
        } else {
            format!("Total: {:.2} h", total_secs / 3600.0)
        }
    });

    // ── Catalog lookup ──────────────────────────────────────────────────────
    let on_catalog_search = {
        let f_target_name2 = f_target_name;
        let f_ra_h2        = f_ra_h;
        let f_dec_deg2     = f_dec_deg;
        let search_result2 = search_result;
        move || {
            let name = f_target_name2.get_untracked().to_lowercase();
            let found = dso_catalog.and_then(|sig| {
                sig.get_untracked().and_then(|cat| {
                    cat.dsos.iter().find(|e| e.name.to_lowercase().contains(&name)).map(|e| {
                        let ra_h = e.ra_deg as f64 / 15.0;
                        let dec  = e.dec_deg as f64;
                        (e.name.clone(), ra_h, dec)
                    })
                })
            });
            match found {
                Some((name, ra_h, dec)) => {
                    f_ra_h2.set(format!("{:.4}", ra_h));
                    f_dec_deg2.set(format!("{:.4}", dec));
                    search_result2.set(Some(format!(
                        "{} → RA {:.3}h  Dec {:.2}°", name, ra_h, dec
                    )));
                }
                None => {
                    search_result2.set(Some(t(lang.get_untracked()).sched_not_found.to_string()));
                }
            }
        }
    };
    let on_catalog_search: Arc<dyn Fn() + Send + Sync> = Arc::new(on_catalog_search);

    // ── Apply observatory scripts ───────────────────────────────────────────
    let send_for_scripts = Arc::clone(&send);
    let on_apply_scripts = move || {
        send_cmd(&send_for_scripts, "scheduler_set_all_settings", serde_json::json!({
            "schedulerStartupEnabled":    startup_enabled.get_untracked(),
            "schedulerPreStartupScript":  pre_startup.get_untracked(),
            "schedulerPostStartupScript": post_startup.get_untracked(),
            "schedulerShutdownEnabled":   shutdown_enabled.get_untracked(),
            "schedulerPreShutdownScript": pre_shutdown.get_untracked(),
            "schedulerPostShutdownScript":post_shutdown.get_untracked(),
        }));
    };
    let on_apply_scripts: Arc<dyn Fn() + Send + Sync> = Arc::new(on_apply_scripts);

    // ── Clear form ──────────────────────────────────────────────────────────
    let on_clear_form = move || {
        f_target_name.set(String::new());
        f_ra_h.set(String::new());
        f_dec_deg.set(String::new());
        f_min_alt.set("30".to_string());
        f_min_moon.set("0".to_string());
        f_pa.set("0".to_string());
        search_result.set(None);
        form_error.set(None);
        step_track.set(true);
        step_focus.set(false);
        step_align.set(false);
        step_guide.set(true);
        startup_cond.set("asap".to_string());
        startup_at.set(String::new());
        completion_cond.set("sequence".to_string());
        completion_count.set("1".to_string());
        completion_at.set(String::new());
        seq_frames.set(vec![SeqFrame::default()]);
    };
    let on_clear_form: Arc<dyn Fn() + Send + Sync> = Arc::new(on_clear_form);

    // ── Submit new job ──────────────────────────────────────────────────────
    let send_for_add = Arc::clone(&send);
    let on_add_job = move || {
        let name      = f_target_name.get_untracked();
        let home      = scheduler.get_untracked().home_dir;
        let frames_raw = seq_frames.get_untracked();

        // Validate RA/Dec
        let ra_f = match f_ra_h.get_untracked().parse::<f64>() {
            Ok(v) if (0.0..=24.0).contains(&v) => v,
            _ => {
                form_error.set(Some(t(lang.get_untracked()).sched_err_ra.to_string()));
                return;
            }
        };
        let dec_f = match f_dec_deg.get_untracked().parse::<f64>() {
            Ok(v) if (-90.0..=90.0).contains(&v) => v,
            _ => {
                form_error.set(Some(t(lang.get_untracked()).sched_err_dec.to_string()));
                return;
            }
        };

        let frames: Vec<SeqFrame> = frames_raw.iter().filter(|f| {
            f.exposure.parse::<f64>().is_ok() && f.count.parse::<u32>().is_ok()
        }).cloned().collect();

        if frames.is_empty() {
            form_error.set(Some(t(lang.get_untracked()).sched_err_frames.to_string()));
            return;
        }

        form_error.set(None);

        let xml = build_esq_xml(&name, &frames);
        let safe_name = sanitize_name(if name.is_empty() { "sequence" } else { &name });
        let rel_path  = format!(".junos-sequences/{}.esq", safe_name);
        let abs_path  = if home.is_empty() {
            format!(".junos-sequences/{}.esq", safe_name)
        } else {
            format!("{}/.junos-sequences/{}.esq", home, safe_name)
        };

        let (seq_r, rep_r, rep_lim, loop_r, until_r, until_val) =
            resolve_completion_condition(
                completion_cond.get_untracked().as_str(),
                completion_count.get_untracked(),
                completion_at.get_untracked(),
            );

        let (asap_r, start_time_r, start_time_val) =
            resolve_startup_condition(startup_cond.get_untracked().as_str(), startup_at.get_untracked());

        // 1. Save the ESQ file to the KStars machine
        send_cmd(&send_for_add, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));

        // 2. Pre-fill the scheduler form (KStars processes msgs in order)
        send_cmd(&send_for_add, "scheduler_set_all_settings", serde_json::json!({
            "nameEdit":          name,
            "raBox":             format!("{:.6}", ra_f),
            "decBox":            format!("{:.6}", dec_f),
            "sequenceEdit":      abs_path,
            "minAltitude":       f_min_alt.get_untracked().parse::<f64>().unwrap_or(30.0),
            "minMoonSeparation": f_min_moon.get_untracked().parse::<f64>().unwrap_or(0.0),
            "positionAngleSpin": f_pa.get_untracked().parse::<f64>().unwrap_or(0.0),
            "schedulerTrackStep": step_track.get_untracked(),
            "schedulerFocusStep": step_focus.get_untracked(),
            "schedulerAlignStep": step_align.get_untracked(),
            "schedulerGuideStep": step_guide.get_untracked(),
            "asapConditionR":        asap_r,
            "startupTimeConditionR": start_time_r,
            "startupTimeEdit":       start_time_val,
            "schedulerCompleteSequences":    seq_r,
            "schedulerRepeatSequences":      rep_r,
            "schedulerRepeatSequencesLimit": rep_lim,
            "schedulerUntilTerminated":      loop_r,
            "schedulerUntil":                until_r,
            "schedulerUntilValue":           until_val,
        }));

        // 3. Add job
        send_cmd(&send_for_add, "scheduler_add_jobs", serde_json::json!({}));

        // Auto-close the overlay once the job has been submitted.
        add_open.set(false);

        // 4. Refresh job list
        let s = Arc::clone(&send_for_add);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(800).await;
            send_cmd(&s, "scheduler_get_jobs", serde_json::json!({}));
        });
    };
    let on_add_job: Arc<dyn Fn() + Send + Sync> = Arc::new(on_add_job);

    view! {
        <div class="sched-root">
            <SchedulerToolbar
                scheduler=scheduler
                send=Arc::clone(&send)
                lang=lang
                on_open_add=Arc::clone(&on_open_add)
                on_open_settings=Arc::clone(&on_open_settings)
            />

            // ── Body: jobs list only ─────────────────────────────────────────
            <div class="sched-body">
                <SchedulerJobsSection scheduler=scheduler send=Arc::clone(&send) lang=lang />
            </div>

            // ── Add-job overlay ──────────────────────────────────────────────
            <Show when=move || add_open.get()>
                <div
                    class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                    on:click=move |_| add_open.set(false)
                >
                    <div
                        class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                        on:click=|ev: MouseEvent| ev.stop_propagation()
                    >
                        <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                            <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                {move || tr().sched_add_job_section}
                            </h2>
                            <button
                                class="btn btn-ghost"
                                on:click=move |_| add_open.set(false)
                            >{move || tr().imaging_close}</button>
                        </div>
                        <div class="flex-1 min-h-0 overflow-y-auto p-sp-4">
                            <SchedulerAddJobSection
                                lang=lang
                                camera=camera
                                filter_wheel=filter_wheel
                                f_target_name=f_target_name
                                f_ra_h=f_ra_h
                                f_dec_deg=f_dec_deg
                                f_min_alt=f_min_alt
                                f_min_moon=f_min_moon
                                f_pa=f_pa
                                search_result=search_result
                                form_error=form_error
                                step_track=step_track
                                step_focus=step_focus
                                step_align=step_align
                                step_guide=step_guide
                                startup_cond=startup_cond
                                startup_at=startup_at
                                completion_cond=completion_cond
                                completion_count=completion_count
                                completion_at=completion_at
                                seq_frames=seq_frames
                                coords_hint=coords_hint
                                seq_total_hint=seq_total_hint
                                on_catalog_search=Arc::clone(&on_catalog_search)
                                on_add_job=Arc::clone(&on_add_job)
                                on_clear_form=Arc::clone(&on_clear_form)
                            />
                        </div>
                    </div>
                </div>
            </Show>

            // ── Settings overlay (scheduler toggles + observatory scripts) ──
            <Show when=move || settings_open.get()>
                <div
                    class="fixed inset-0 z-50 bg-[rgba(2,4,10,0.88)] backdrop-blur-sm flex items-stretch justify-center p-sp-4 max-[759px]:p-sp-2"
                    on:click=move |_| settings_open.set(false)
                >
                    <div
                        class="w-full max-w-[980px] bg-bg border border-border-base rounded-[4px] shadow-[0_24px_80px_rgba(0,0,0,0.45)] overflow-hidden flex flex-col"
                        on:click=|ev: MouseEvent| ev.stop_propagation()
                    >
                        <div class="flex items-center justify-between gap-sp-3 py-sp-3 px-sp-4 border-b border-border-base bg-[rgba(10,12,20,0.8)]">
                            <h2 class="text-text-blue text-sm uppercase tracking-[0.08em] m-0">
                                {move || tr().sched_settings_btn}
                            </h2>
                            <button
                                class="btn btn-ghost"
                                on:click=move |_| settings_open.set(false)
                            >{move || tr().imaging_close}</button>
                        </div>
                        <div class="flex-1 min-h-0 overflow-y-auto p-sp-4 flex flex-col gap-sp-4">
                            <SchedulerSettingsSection
                                lang=lang
                                send=Arc::clone(&send)
                                greedy=greedy
                                remember_prog=remember_prog
                                reschedule_err=reschedule_err
                            />
                            <SchedulerScriptsSection
                                lang=lang
                                startup_enabled=startup_enabled
                                pre_startup=pre_startup
                                post_startup=post_startup
                                shutdown_enabled=shutdown_enabled
                                pre_shutdown=pre_shutdown
                                post_shutdown=post_shutdown
                                on_apply_scripts=Arc::clone(&on_apply_scripts)
                            />
                        </div>
                    </div>
                </div>
            </Show>
        </div>
    }
}
