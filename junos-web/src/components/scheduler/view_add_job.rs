//! Scheduler tab: the add-job sheet — target, capture sequence, conditions —
//! submitted as `scheduler_save_sequence_file` + `scheduler_set_all_settings`
//! + `scheduler_add_jobs` (KStars processes them in order).

use std::sync::Arc;

use leptos::prelude::*;
use serde_json::Value;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot, SiteSnapshot};
use crate::components::coord_input::{
    degrees_to_dms_string, dms_string_to_degrees, hms_string_to_hours, hours_to_hms_string,
    parse_canonical, CoordInput, CoordMode,
};
use crate::components::form::{JobOptions, CARD, CARD_TITLE, FOOTER, LABEL, NUM, ROW, SELECT};
use crate::components::sequence_editor::{build_esq_xml, fmt_duration, SeqEnd, SeqFrame, SeqLimits, SequenceEditor};
use crate::components::sky::clock;
use crate::dom::event_target_value;
use crate::dso_catalog::DsoCatalogData;
use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::altitude::{self, altitude, altitude_chart, hhmm, job_ms, samples, Night};
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
    /// Refocus / guide limits written with the sequence.
    pub limits: RwSignal<SeqLimits>,
    /// Panel / dust cap after the last frame.
    pub end: RwSignal<SeqEnd>,
    /// Where the altitude card's session starts — `""` (the job that ends
    /// last, else the start condition), `"auto"`, `"time"` or a job index.
    /// Only an indication: it's never sent to KStars.
    pub alt_ref: RwSignal<String>,
    /// "HH:MM" tonight, for `alt_ref == "time"`.
    pub alt_ref_time: RwSignal<String>,
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
            limits: RwSignal::new(SeqLimits::default()),
            end: RwSignal::new(SeqEnd::default()),
            alt_ref: RwSignal::new(String::new()),
            alt_ref_time: RwSignal::new(String::new()),
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
        // Not edited: the editor takes Ekos' values again.
        self.limits.set(SeqLimits::default());
        self.end.set(SeqEnd::default());
        self.alt_ref.set(String::new());
        self.alt_ref_time.set(String::new());
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

/// The altitude card's reference: the effective `alt_ref` — `""` resolves to
/// the job that ends last, else `"auto"` — and its instant, if any.
fn reference(sel: &str, time: &str, jobs: &[Value], site: &SiteSnapshot) -> (String, Option<f64>) {
    let job_end = |i: usize| jobs.get(i).and_then(|j| job_ms(j, "stopTime"));
    match sel {
        "auto" => ("auto".into(), None),
        // "HH:MM" within tonight's noon → noon night: 01:30 is after midnight.
        "time" => {
            let mut hm = time.split(':').map(|s| s.parse::<u32>().ok());
            let at = hm.next().flatten().zip(hm.next().flatten());
            ("time".into(), at.map(|(h, m)| clock::night_hour(Night::tonight(site).start, h, m)))
        }
        i => match i.parse().ok().and_then(|i| Some((i, job_end(i)?))) {
            Some((i, t)) => (i.to_string(), Some(t)),
            None => (0..jobs.len())
                .filter_map(|i| Some((i, job_end(i)?)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map_or(("auto".into(), None), |(i, t)| (i.to_string(), Some(t))),
        },
    }
}

/// What the altitude card draws, recomputed only when an input changes.
#[derive(Clone, PartialEq)]
struct Plan {
    ra: f64,
    dec: f64,
    night: Night,
    min_alt: Option<f64>,
    /// The session's start: the reference, else the start condition's estimate.
    start: Option<f64>,
    session: Option<(f64, f64)>,
    /// ASAP without a reference, and nothing tonight clears the minimum altitude.
    no_slot: bool,
}

/// Tonight's altitude of the target with this job's session on it. The
/// session is an estimate: ASAP starts once the target clears the minimum
/// altitude (from dusk when the twilight constraint is on), and it lasts the
/// sequence's exposures × repeats — delays, slews, focus and align aside.
/// A reference — the end of a job already scheduled, or an hour — moves its
/// start there instead, as an indication only: the job keeps its start
/// condition.
fn altitude_card(
    f: AddJobForm,
    site: Signal<SiteSnapshot>,
    jobs: Signal<Vec<Value>>,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let refr = Memo::new(move |_| {
        let (sel, time) = (f.alt_ref.get(), f.alt_ref_time.get());
        jobs.with(|js| site.with(|s| reference(&sel, &time, js, s)))
    });
    let plan = Memo::new(move |_| {
        let (ra, dec) = target_deg(&f.ra.get(), &f.dec.get())?;
        let site = site.get();
        let reference = refr.with(|r| r.1);
        let night = reference.map_or_else(|| Night::tonight(&site), |t| Night::containing(t, &site));
        let alt = |t: f64| altitude(ra, dec, t, &site);
        let o = f.opts;
        let min_alt = if o.use_alt.get() { o.min_alt.get().trim().parse::<f64>().ok() } else { None };

        let asap = o.start_cond.get() != "at";
        let start = match reference {
            Some(t) => Some(t),
            None if asap => {
                let from = js_sys::Date::now().max(if o.twilight.get() { night.dusk.unwrap_or(night.start) } else { night.start });
                samples(from, night.end, PLAN_STEP_MS).find(|t| min_alt.is_none_or(|m| alt(*t) >= m))
            }
            None => local_ms(&o.start_at.get()),
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
        Some(Plan { ra, dec, night, min_alt, start, session, no_slot: reference.is_none() && asap && start.is_none() })
    });
    let has_target = Memo::new(move |_| plan.with(Option::is_some));

    let chart = move || plan.get().map(|p| {
        let track = altitude::Track { ra_deg: p.ra, dec_deg: p.dec, window: p.session, label: None };
        altitude_chart(p.night, site.get(), vec![track], p.min_alt, t(lang.get()))
    });

    let lines = move || plan.get().map(|p| {
        let tr = t(lang.get());
        let site = site.get();
        let alt = |t: f64| altitude(p.ra, p.dec, t, &site);
        let warn = |s: String| view! { <span class="text-state-warn">{format!("\u{26a0} {s}")}</span> };
        let no_slot = p.no_slot.then(|| p.min_alt.map(|m| warn(format!("{} {m:.0}\u{00b0}", tr.sched_alt_no_slot))));
        let below = p.session.and_then(|(a, b)| {
            let m = p.min_alt?;
            let t = samples(a, b, PLAN_STEP_MS).find(|t| alt(*t) < m)?;
            Some(warn(format!("{} {m:.0}\u{00b0} \u{00b7} {}", tr.sched_alt_below, hhmm(t))))
        });
        let past_dawn = p.session.and_then(|(_, b)| {
            let dawn = p.night.dawn.filter(|d| b > *d)?;
            Some(warn(format!("{} \u{00b7} {}", tr.sched_alt_dawn, hhmm(dawn))))
        });
        // The session, or only its start while the sequence has no frames.
        let session = match (p.session, p.start) {
            (Some((a, b)), _) => Some(format!("{} ({:.0}\u{00b0}) \u{2192} {} ({:.0}\u{00b0}) \u{00b7} \u{2248}{}",
                                              hhmm(a), alt(a), hhmm(b), alt(b), fmt_duration((b - a) / 1000.0))),
            (None, Some(a)) => Some(format!("{} ({:.0}\u{00b0})", hhmm(a), alt(a))),
            _ => None,
        };
        let session_line = session.map(|s| view! {
            <span class="text-text"><span class="text-accent-cyan">"\u{25ac} "</span>{s}</span>
        });
        let peak = samples(p.night.start, p.night.end, PLAN_STEP_MS)
            .map(|t| (t, alt(t)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(t, a)| format!("{} {a:.0}\u{00b0} \u{00b7} {}", tr.sched_alt_peak, hhmm(t)));
        view! {
            {session_line}
            <span>{peak}</span>
            {no_slot}
            {below}
            {past_dawn}
        }
    });

    // The end of each job, the start condition, or an hour.
    let options = move || {
        let tr = t(lang.get());
        let cur = refr.with(|r| r.0.clone());
        let mut items: Vec<(String, String, bool)> = jobs.with(|js| js.iter().enumerate().map(|(i, j)| {
            let end = job_ms(j, "stopTime");
            // The time first: a phone's select cuts the label's end.
            let label = format!("{} \u{00b7} {} #{} {}", end.map_or_else(|| "\u{2014}".to_string(), hhmm),
                                tr.sched_alt_ref_end, i + 1, j["name"].as_str().unwrap_or("?"));
            (i.to_string(), label, end.is_some())
        }).collect());
        items.push(("auto".into(), tr.sched_alt_ref_auto.into(), true));
        items.push(("time".into(), tr.sched_alt_ref_time.into(), true));
        items.into_iter().map(|(value, label, enabled)| {
            let selected = value == cur;
            view! { <option value=value prop:selected=selected disabled=!enabled>{label}</option> }
        }).collect::<Vec<_>>()
    };
    // An hour picked for the first time starts where the session did.
    let on_ref = move |ev: web_sys::Event| {
        let v = event_target_value(&ev);
        if v == "time" && f.alt_ref_time.with_untracked(String::is_empty) {
            if let Some(t) = plan.with_untracked(|p| p.as_ref().and_then(|p| p.start.or(p.night.dusk))) {
                f.alt_ref_time.set(hhmm(t));
            }
        }
        f.alt_ref.set(v);
    };

    // Only the target appearing or going rebuilds the card: the picker stays
    // put while the chart redraws, so the time input keeps the focus.
    move || {
        if !has_target.get() {
            let tr = t(lang.get());
            return view! {
                <div class=CARD>
                    <span class=CARD_TITLE>{tr.sched_alt_title}</span>
                    <span class="text-sm text-text-faint">{tr.sched_alt_hint}</span>
                </div>
            }.into_any();
        }
        view! {
            <div class=CARD>
                {chart}
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || t(lang.get()).sched_alt_ref}</span>
                    <select class=SELECT on:change=on_ref>{options}</select>
                </div>
                <Show when=move || refr.with(|r| r.0 == "time")>
                    <div class=format!("{ROW} justify-end")>
                        <input type="time" class="input input--sm font-mono w-[150px] shrink-0 max-md:h-9"
                               prop:value=move || f.alt_ref_time.get()
                               on:input=move |ev| f.alt_ref_time.set(event_target_value(&ev)) />
                    </div>
                </Show>
                <Show when=move || refr.with(|r| r.0 != "auto")>
                    <span class="text-xs text-text-faint">{move || t(lang.get()).sched_alt_ref_note}</span>
                </Show>
                <div class="flex flex-col gap-1 font-mono text-xs text-text-muted">{lines}</div>
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
    /// Ekos' Capture settings, whose limits the sequence starts from.
    #[prop(into)] capture_settings: Signal<Value>,
    /// The jobs already scheduled, whose ends the altitude card can start from.
    #[prop(into)] jobs: Signal<Vec<Value>>,
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
        f.limits.track();
        f.end.track();
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
        if !f.limits.with_untracked(SeqLimits::is_valid) {
            return fail(tr.seq_err_limits);
        }
        let Ok(post) = f.end.with_untracked(|e| e.post_job_script(&frames)) else {
            return fail(tr.seq_err_end);
        };
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
        let xml = camera.with_untracked(|cam| f.limits.with_untracked(|l| {
            build_esq_xml("", &seq_fits_path, &frames, l, post.as_deref(), false, cam)
        }));
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
                    {altitude_card(f, site, jobs, lang)}

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
                        <SequenceEditor frames=f.frames fits_dir=f.fits_dir limits=f.limits end=f.end
                                        capture_settings=capture_settings camera=camera filter_wheel=filter_wheel />
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
