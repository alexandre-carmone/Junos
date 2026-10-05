//! Scheduler tab: the add-job sheet — target, capture sequence, conditions —
//! submitted as `scheduler_save_sequence_file` + `scheduler_set_all_settings`
//! + `scheduler_add_jobs` (KStars processes them in order).

use std::sync::Arc;

use leptos::prelude::*;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot, SiteSnapshot};
use crate::components::coord_input::{
    degrees_to_dms_string, dms_string_to_degrees, hms_string_to_hours, hours_to_hms_string,
    parse_canonical, CoordInput, CoordMode,
};
use crate::components::form::{JobOptions, CARD, CARD_TITLE, FOOTER, LABEL, NUM, ROW};
use crate::components::sequence_editor::{build_esq_xml, fmt_duration, SeqFrame, SequenceEditor};
use crate::dom::event_target_value;
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::altitude::{self, altitude, altitude_chart, hhmm, samples, Night};
use super::labels::sanitize_name;
use super::mapping::resolve_completion_condition;

const SEARCH_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="10.5" cy="10.5" r="6.5"/><path d="M15.5 15.5 L21 21"/></svg>"##;

/// The add-job form. Owned by the tab, so it survives closing the sheet.
#[derive(Clone, Copy)]
pub struct AddJobForm {
    pub name: RwSignal<String>,
    /// Canonical "HH MM SS" / "+DD MM SS" strings, as `CoordInput` edits them.
    pub ra: RwSignal<String>,
    pub dec: RwSignal<String>,
    pub pa: RwSignal<String>,
    /// Last catalog lookup: `Some(Some(name))` found, `Some(None)` not found.
    pub found: RwSignal<Option<Option<String>>>,
    pub error: RwSignal<Option<String>>,
    pub opts: JobOptions,
    pub frames: RwSignal<Vec<SeqFrame>>,
    /// Capture folder; defaults from CaptureDirCtx and is kept by `reset`.
    pub fits_dir: RwSignal<String>,
}

impl AddJobForm {
    pub fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            ra: RwSignal::new(String::new()),
            dec: RwSignal::new(String::new()),
            pa: RwSignal::new("0".into()),
            found: RwSignal::new(None),
            error: RwSignal::new(None),
            opts: JobOptions::new(),
            frames: RwSignal::new(vec![SeqFrame::default()]),
            fits_dir: RwSignal::new(String::new()),
        }
    }

    pub fn reset(&self) {
        self.name.set(String::new());
        self.ra.set(String::new());
        self.dec.set(String::new());
        self.pa.set("0".into());
        self.found.set(None);
        self.error.set(None);
        self.opts.reset();
        self.frames.set(vec![SeqFrame::default()]);
    }

    pub fn set_target(&self, name: String, ra_deg: f64, dec_deg: f64) {
        self.name.set(name);
        self.ra.set(hours_to_hms_string(ra_deg / 15.0));
        self.dec.set(degrees_to_dms_string(dec_deg));
    }
}

/// Minutes and seconds under 60 (the inputs take any two digits).
fn sexagesimal_ok(s: &str) -> bool {
    let (_, _, m, sec) = parse_canonical(s);
    m < 60 && sec < 60
}

/// The target as J2000 degrees, once the coordinates are valid. 0h / 0°
/// counts as none: that's what `CoordInput` holds while its fields are empty.
fn target_deg(ra: &str, dec: &str) -> Option<(f64, f64)> {
    let (h, d) = (hms_string_to_hours(ra), dms_string_to_degrees(dec));
    let ok = sexagesimal_ok(ra) && sexagesimal_ok(dec) && (0.0..24.0).contains(&h) && (-90.0..=90.0).contains(&d)
        && (h, d) != (0.0, 0.0);
    ok.then_some((h * 15.0, d))
}

/// A `datetime-local` value as Unix ms.
fn local_ms(s: &str) -> Option<f64> {
    Some(js_sys::Date::new(&s.into()).get_time()).filter(|t| t.is_finite())
}

const PLAN_STEP_MS: f64 = 300_000.0;

/// Tonight's altitude of the target with this job's session on it. The
/// session is an estimate: ASAP starts once the target clears the minimum
/// altitude (from dusk when the twilight constraint is on), and it lasts the
/// sequence's exposures × repeats — delays, slews, focus and align aside.
fn altitude_card(f: AddJobForm, site: Signal<SiteSnapshot>, lang: RwSignal<Lang>) -> impl IntoView {
    move || {
        let tr = t(lang.get());
        let Some((ra, dec)) = target_deg(&f.ra.get(), &f.dec.get()) else {
            return view! {
                <div class=CARD>
                    <span class=CARD_TITLE>{tr.sched_alt_title}</span>
                    <span class="text-sm text-text-faint">{tr.sched_alt_hint}</span>
                </div>
            }.into_any();
        };
        let site = site.get();
        let night = Night::tonight(&site);
        let alt = |t: f64| altitude(ra, dec, t, &site);
        let o = f.opts;
        let min_alt = if o.use_alt.get() { o.min_alt.get().trim().parse::<f64>().ok() } else { None };

        let asap = o.start_cond.get() != "at";
        let start = if asap {
            let from = js_sys::Date::now().max(if o.twilight.get() { night.dusk.unwrap_or(night.start) } else { night.start });
            samples(from, night.end, PLAN_STEP_MS).find(|t| min_alt.is_none_or(|m| alt(*t) >= m))
        } else {
            local_ms(&o.start_at.get())
        };
        let secs: f64 = f.frames.with(|fs| fs.iter().filter_map(SeqFrame::duration_secs).sum());
        let session = start.and_then(|a| {
            let b = match o.complete_cond.get().as_str() {
                "loop" => night.dawn.unwrap_or(night.end),
                "at" => local_ms(&o.complete_at.get())?,
                "repeat" => a + secs * 1000.0 * o.complete_count.get().trim().parse::<f64>().unwrap_or(1.0).max(1.0),
                _ => a + secs * 1000.0,
            };
            (b > a).then_some((a, b))
        });

        let warn = |s: String| view! { <span class="text-state-warn">{format!("\u{26a0} {s}")}</span> };
        let no_slot = (asap && start.is_none()).then(|| min_alt.map(|m| warn(format!("{} {m:.0}\u{00b0}", tr.sched_alt_no_slot))));
        let below = session.and_then(|(a, b)| {
            let m = min_alt?;
            let t = samples(a, b, PLAN_STEP_MS).find(|t| alt(*t) < m)?;
            Some(warn(format!("{} {m:.0}\u{00b0} \u{00b7} {}", tr.sched_alt_below, hhmm(t))))
        });
        let past_dawn = session.and_then(|(_, b)| {
            let dawn = night.dawn.filter(|d| b > *d)?;
            Some(warn(format!("{} \u{00b7} {}", tr.sched_alt_dawn, hhmm(dawn))))
        });
        let session_line = session.map(|(a, b)| view! {
            <span class="text-text">
                <span class="text-accent-cyan">"\u{25ac} "</span>
                {format!("{} ({:.0}\u{00b0}) \u{2192} {} ({:.0}\u{00b0}) \u{00b7} \u{2248}{}",
                         hhmm(a), alt(a), hhmm(b), alt(b), fmt_duration((b - a) / 1000.0))}
            </span>
        });
        let peak = samples(night.start, night.end, PLAN_STEP_MS)
            .map(|t| (t, alt(t)))
            .max_by(|p, q| p.1.total_cmp(&q.1))
            .map(|(t, a)| format!("{} {a:.0}\u{00b0} \u{00b7} {}", tr.sched_alt_peak, hhmm(t)));

        let track = altitude::Track { ra_deg: ra, dec_deg: dec, window: session, label: None };
        view! {
            <div class=CARD>
                {altitude_chart(night, site.clone(), vec![track], min_alt, tr)}
                <div class="flex flex-col gap-1 font-mono text-xs text-text-muted">
                    {session_line}
                    <span>{peak}</span>
                    {no_slot}
                    {below}
                    {past_dawn}
                </div>
            </div>
        }.into_any()
    }
}

#[component]
pub fn AddJobSheet(
    form: AddJobForm,
    #[prop(into)] site: Signal<SiteSnapshot>,
    #[prop(into)] camera: Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
    #[prop(into)] home_dir: Signal<String>,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
    open: RwSignal<bool>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let f = form;
    let dso_catalog = use_context::<RwSignal<Option<Arc<DsoCatalogData>>>>();

    // An error goes away once the inputs it was about change.
    Effect::new(move |_| {
        f.name.track();
        f.ra.track();
        f.dec.track();
        f.frames.track();
        f.error.set(None);
    });

    // Catalog lookup: an exact name first, else the first one containing it
    // — but not as a longer number ("NGC 224" must not pick NGC 2247).
    let search = move || {
        let q = f.name.get_untracked().trim().to_lowercase();
        if q.is_empty() { return; }
        let hit = dso_catalog.and_then(|c| c.get_untracked()).and_then(|cat| {
            let find = |exact: bool| cat.dsos.iter().find(|e| {
                let n = e.name.to_lowercase();
                if exact {
                    n == q
                } else {
                    n.match_indices(&q).any(|(i, _)| !n[i + q.len()..].starts_with(|c: char| c.is_ascii_digit()))
                }
            });
            find(true).or_else(|| find(false)).map(|e| (e.name.clone(), e.ra_deg as f64, e.dec_deg as f64))
        });
        if let Some((_, ra, dec)) = &hit {
            f.ra.set(hours_to_hms_string(ra / 15.0));
            f.dec.set(degrees_to_dms_string(*dec));
        }
        f.found.set(Some(hit.map(|(name, ..)| name)));
    };

    let on_add = move |_| {
        let tr = t(lang.get_untracked());
        let fail = |msg: &str| f.error.set(Some(msg.to_string()));
        let name = f.name.get_untracked().trim().to_string();
        if name.is_empty() {
            return fail(tr.sched_err_name);
        }
        let (ra, dec) = (f.ra.get_untracked(), f.dec.get_untracked());
        let ra_h = hms_string_to_hours(&ra);
        if !sexagesimal_ok(&ra) || !(0.0..24.0).contains(&ra_h) {
            return fail(tr.sched_err_ra);
        }
        let dec_d = dms_string_to_degrees(&dec);
        if !sexagesimal_ok(&dec) || !(-90.0..=90.0).contains(&dec_d) {
            return fail(tr.sched_err_dec);
        }
        let frames: Vec<SeqFrame> = f.frames.with_untracked(|fs| {
            fs.iter().filter(|fr| fr.duration_secs().is_some()).cloned().collect()
        });
        if frames.is_empty() {
            return fail(tr.sched_err_frames);
        }
        if frames.iter().any(|fr| !fr.values_ok()) {
            return fail(tr.seq_err_values);
        }
        // ADU flats: KStars skips the calibration for a target ≤ 0 and aborts
        // the capture on non-FITS/XISF encodings (cameraprocess.cpp) — catch
        // both here rather than mid-run.
        if frames.iter().any(|fr| fr.is_adu_flat() && fr.flat_adu_target().is_none()) {
            return fail(tr.sched_err_flat_adu);
        }
        if frames.iter().any(|fr| fr.is_adu_flat() && !matches!(fr.encoding.as_str(), "" | "FITS" | "XISF")) {
            return fail(tr.sched_err_flat_encoding);
        }

        let safe_name = sanitize_name(&name);
        let home = home_dir.get_untracked();

        // Bake the sanitized name straight into the capture folder path rather
        // than deriving the subfolder from the object name (%t) at runtime.
        // KStars would otherwise build the subfolder from the job's target name;
        // joining it into the path keeps all frames under one predictable
        // directory. (Mirrors the mosaic import path in `mosaic_tab.rs`.)
        let fits_root = f.fits_dir.get_untracked();
        let fits_root = fits_root.trim().trim_end_matches('/');
        let seq_fits_path = if fits_root.is_empty() {
            safe_name.clone()
        } else {
            format!("{fits_root}/{safe_name}")
        };
        let xml = camera.with_untracked(|cam| build_esq_xml("", &seq_fits_path, &frames, false, cam));
        let rel_path = format!(".junos-sequences/{safe_name}.esq");
        let abs_path = if home.is_empty() { rel_path.clone() } else { format!("{home}/{rel_path}") };

        let o = f.opts;
        let (seq_r, rep_r, rep_lim, loop_r, until_r, until_val) = resolve_completion_condition(
            o.complete_cond.get_untracked().as_str(),
            o.complete_count.get_untracked(),
            o.complete_at.get_untracked(),
        );

        // 1. Save the ESQ file on the KStars machine.
        send_cmd(&send, "scheduler_save_sequence_file",
            serde_json::json!({"path": rel_path, "filedata": xml}));
        // 2. Fill KStars' job form: shared options, then this job's fields.
        send_cmd(&send, "scheduler_set_all_settings", o.settings_json());
        send_cmd(&send, "scheduler_set_all_settings", serde_json::json!({
            "nameEdit":          name,
            "raBox":             format!("{ra_h:.6}"),
            "decBox":            format!("{dec_d:.6}"),
            "sequenceEdit":      abs_path,
            "positionAngleSpin": f.pa.get_untracked().trim().parse::<f64>().unwrap_or(0.0),
            "schedulerTrackStep": o.track.get_untracked(),
            "schedulerFocusStep": o.focus.get_untracked(),
            "schedulerAlignStep": o.align.get_untracked(),
            "schedulerGuideStep": o.guide.get_untracked(),
            "schedulerCompleteSequences":    seq_r,
            "schedulerRepeatSequences":      rep_r,
            "schedulerRepeatSequencesLimit": rep_lim,
            "schedulerUntilTerminated":      loop_r,
            "schedulerUntil":                until_r,
            "schedulerUntilValue":           until_val,
        }));
        // 3. Add the job, close, and refresh the list once KStars has it.
        send_cmd(&send, "scheduler_add_jobs", serde_json::json!({}));
        open.set(false);
        let s = Arc::clone(&send);
        wasm_bindgen_futures::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(800).await;
            send_cmd(&s, "scheduler_get_jobs", serde_json::json!({}));
        });
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3">
            <div class="grid gap-3 md:grid-cols-2 md:items-start">
                <div class="min-w-0 flex flex-col gap-3">
                    // Target: name + catalog lookup, coordinates (J2000), PA.
                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().sched_target_label}</span>
                        <div class="flex gap-2">
                            <input type="text" class="input flex-1 min-w-0" enterkeyhint="search"
                                   placeholder=move || tr().sched_target_placeholder
                                   prop:value=move || f.name.get()
                                   on:input=move |ev| { f.name.set(event_target_value(&ev)); f.found.set(None); }
                                   on:keydown=move |ev: web_sys::KeyboardEvent| if ev.key() == "Enter" { search() } />
                            <button class="btn btn-ghost shrink-0 px-3" title=move || tr().sched_search_catalog
                                    on:click=move |_| search()>
                                <span class="inline-block w-4 h-4" inner_html=SEARCH_ICON></span>
                            </button>
                        </div>
                        {move || f.found.get().map(|hit| match hit {
                            Some(name) => view! { <span class="text-xs text-state-ok">{format!("\u{2713} {name}")}</span> }.into_any(),
                            None => view! { <span class="text-xs text-state-warn">{tr().sched_not_found}</span> }.into_any(),
                        })}
                        <div class=ROW>
                            <span class=format!("{LABEL} flex-1")>{move || tr().sched_ra_label}</span>
                            <CoordInput mode=CoordMode::Hms value=f.ra aria_label="RA" />
                        </div>
                        <div class=ROW>
                            <span class=format!("{LABEL} flex-1")>{move || tr().sched_dec_label}</span>
                            <CoordInput mode=CoordMode::DmsSigned value=f.dec aria_label="Dec" />
                        </div>
                        <div class=ROW>
                            <span class=format!("{LABEL} flex-1")>{move || tr().sched_pa_label}</span>
                            <input type="number" step="1" class=NUM
                                   prop:value=move || f.pa.get()
                                   on:input=move |ev| f.pa.set(event_target_value(&ev)) />
                            <span class="w-3 text-sm text-text-muted">"\u{00b0}"</span>
                        </div>
                    </div>

                    // Where the target is tonight, and this job's session on it.
                    {altitude_card(f, site, lang)}

                    // When to run, when it's done, and where it may run.
                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().sched_conditions_legend}</span>
                        {f.opts.conditions_view(lang, true)}
                        <span class=format!("{CARD_TITLE} pt-1")>{move || tr().sched_constraints_legend}</span>
                        {f.opts.constraints_view(lang)}
                    </div>
                </div>

                // Capture sequence and the startup steps before it.
                <div class="min-w-0 flex flex-col gap-3">
                    <div class=CARD>
                        <span class=CARD_TITLE>{move || tr().sched_seq_label}</span>
                        <SequenceEditor frames=f.frames fits_dir=f.fits_dir camera=camera filter_wheel=filter_wheel />
                        <span class=format!("{CARD_TITLE} pt-1")>{move || tr().sched_steps_legend}</span>
                        {f.opts.steps_view(lang)}
                    </div>
                </div>
            </div>
        </div>
        <div class=FOOTER>
            <div class="flex-1 min-w-0 text-sm leading-snug text-state-err">{move || f.error.get()}</div>
            <button class="btn btn-ghost h-11 shrink-0" on:click=move |_| f.reset()>{move || tr().sched_clear_btn}</button>
            <button class="btn btn-primary h-11 px-5 shrink-0 font-semibold" on:click=on_add>
                {move || tr().sched_add_job_btn}
            </button>
        </div>
    }
}
