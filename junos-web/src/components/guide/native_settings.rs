//! The native Guide tab's Guider settings sheet: PHD2's Advanced Settings
//! as collapsible sections — Exposure (the duration chips and Auto with its
//! bounds and target SNR, time lapse, variable delay), Noise reduction,
//! Star (search region, SNR and HFD bounds, mass change, jumps, multi-star,
//! auto-select downsampling, fast recenter, sticky lock), Dither (mode, RA
//! only, scale, and the settle the tab's Guide and Dither send, kept in the
//! browser), AO, Server, the image logger — and the tools: the manual pulse
//! pad and lock shift. Footer: Export settings (PHD2's `PHD Config 1` text).
//!
//! The rows with a live setter use it (`set_exposure`, `set_auto_exposure`,
//! `set_variable_delay_settings`, `set_dither_mode`, `set_image_logger`);
//! the others `set_profile_value`. Each edit stays shown until
//! `guider_settings` echoes it; an error shows at the top.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{setting_row, CHIP, LABEL, ROW, SELECT};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::GuiderStatusData;

use super::download_text;
use super::lock_shift::lock_shift_card;
use super::protocol::{profile_rows, Reply, Rpc, Settle, KEY_DITHER_MODE, KEY_DITHER_RA_ONLY, KEY_DITHER_SCALE};
use super::pulses::pulses_card;
use super::rows::{as_flag, bool_row, num_row, num_row_with, section, LabelFn, Num, Rows};

const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";

const VAR_DELAY: [&str; 3] = ["/frame/var_delay/enabled", "/frame/var_delay/short_delay", "/frame/var_delay/long_delay"];
const IMAGE_LOGGER: [&str; 7] = [
    "/ImageLogger/LoggingEnabled",
    "/ImageLogger/LogFramesOverThreshRel",
    "/ImageLogger/ErrorThreshRel",
    "/ImageLogger/LogFramesOverThreshPx",
    "/ImageLogger/ErrorThreshPx",
    "/ImageLogger/LogFramesDropped",
    "/ImageLogger/LogAutoSelectFrames",
];

/// `set_image_logger`'s params for `IMAGE_LOGGER`, in its order.
const LOGGER_PARAMS: [&str; 7] = ["enabled", "over_rel", "thresh_rel", "over_px", "thresh_px", "dropped", "auto_select"];

/// `set_image_logger {enabled, over_rel, …}` from the keys' values.
fn logger_params(cur: &impl Fn(&str) -> Value) -> serde_json::Map<String, Value> {
    IMAGE_LOGGER.iter().zip(LOGGER_PARAMS).map(|(k, n)| (n.to_string(), cur(k))).collect()
}

/// PHD2's defaults for the keys this sheet shows.
fn defaults() -> Vec<(&'static str, Value)> {
    vec![
        ("/frame/timeLapse", json!(0)),
        (VAR_DELAY[0], json!(false)),
        (VAR_DELAY[1], json!(1000)),
        (VAR_DELAY[2], json!(10000)),
        ("/NoiseReductionMethod", json!(0)),
        ("/guider/onestar/SearchRegion", json!(15)),
        ("/guider/StarMinSNR", json!(6.0)),
        ("/guider/StarMinHFD", json!(1.5)),
        ("/guider/StarMaxHFD", json!(20.0)),
        ("/guider/onestar/MassChangeThresholdEnabled", json!(false)),
        ("/guider/onestar/MassChangeThreshold", json!(0.5)),
        ("/guider/onestar/TolerateJumpsEnabled", json!(false)),
        ("/guider/onestar/TolerateJumpsThreshold", json!(4.0)),
        ("/guider/multistar/enabled", json!(false)),
        ("/guider/AutoSelDownsample", json!(0)),
        ("/guider/FastRecenter", json!(true)),
        ("/StickyLockPosition", json!(false)),
        (KEY_DITHER_MODE, json!(0)),
        (KEY_DITHER_RA_ONLY, json!(false)),
        (KEY_DITHER_SCALE, json!(1.0)),
        ("/stepguider/BumpOnDither", json!(true)),
        ("/stepguider/BumpSettlingBoost", json!(3.0)),
        ("/server/guide_allow_recalibrate", json!(true)),
        (IMAGE_LOGGER[0], json!(false)),
        (IMAGE_LOGGER[1], json!(false)),
        (IMAGE_LOGGER[2], json!(4.0)),
        (IMAGE_LOGGER[3], json!(false)),
        (IMAGE_LOGGER[4], json!(4.0)),
        (IMAGE_LOGGER[5], json!(false)),
        (IMAGE_LOGGER[6], json!(false)),
    ]
}

/// A combo over an integer key.
fn int_select(r: &Rows, key: &'static str, label: LabelFn, options: &'static [(i64, LabelFn)]) -> impl IntoView + use<> {
    let (set, values, lang) = (r.set.clone(), r.values, r.lang);
    let cur = move || values.with(|v| v.get(key).and_then(Value::as_i64).unwrap_or(0));
    setting_row(r.label(label), view! {
        <select class=SELECT on:change=move |ev| set(key, json!(event_target_value(&ev).parse::<i64>().unwrap_or(0)))>
            {options.iter().map(|(v, l)| {
                let v = *v;
                let l = *l;
                view! { <option value=v.to_string() prop:selected=move || cur() == v>{move || l(t(lang.get()))}</option> }
            }).collect_view()}
        </select>
    })
}

/// A browser-kept number (the settle the footer sends).
fn local_num(label: impl Fn() -> &'static str + Send + 'static, v: RwSignal<f64>, min: f64, max: f64) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            <input type="number" inputmode="decimal" step="any" class=NUM min=min.to_string() max=max.to_string()
                   prop:value=move || v.get().to_string()
                   on:change=move |ev| {
                       if let Ok(x) = event_target_value(&ev).parse::<f64>() {
                           v.set(x.clamp(min, max))
                       }
                   } />
        </div>
    }
}

pub fn native_settings_sheet(
    state: RwSignal<Option<GuiderStatusData>>,
    settings: RwSignal<Value>,
    tools: RwSignal<Value>,
    rpc: Rpc,
    settle: Settle,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let err = RwSignal::new(None::<String>);

    // The live setters take a group of keys at once: the others' current
    // values (or PHD2's defaults) complete the call.
    let stored = move |k: &str| {
        settings.with_untracked(|s| s.get(k).cloned()).or_else(|| defaults().into_iter().find(|d| d.0 == k).map(|d| d.1)).unwrap_or(Value::Null)
    };
    let write = {
        let rpc = rpc.clone();
        move |key: &'static str, v: Value| -> Reply {
            let cur = |k: &str| if k == key { v.clone() } else { stored(k) };
            let f = |k: &str| cur(k).as_f64().unwrap_or(0.0);
            let b = |k: &str| as_flag(&cur(k)).unwrap_or(false);
            if VAR_DELAY.contains(&key) {
                return rpc.call("set_variable_delay_settings", json!({
                    "Enabled": b(VAR_DELAY[0]),
                    "ShortDelaySeconds": f(VAR_DELAY[1]) / 1000.0,
                    "LongDelaySeconds": f(VAR_DELAY[2]) / 1000.0,
                }));
            }
            if [KEY_DITHER_MODE, KEY_DITHER_RA_ONLY, KEY_DITHER_SCALE].contains(&key) {
                let mode = if cur(KEY_DITHER_MODE).as_i64() == Some(1) { "spiral" } else { "random" };
                return rpc.call("set_dither_mode", json!({ "mode": mode, "ra_only": b(KEY_DITHER_RA_ONLY), "scale": f(KEY_DITHER_SCALE) }));
            }
            if IMAGE_LOGGER.contains(&key) {
                return rpc.call("set_image_logger", Value::Object(logger_params(&cur)));
            }
            rpc.set_profile_value(key, v)
        }
    };
    let rows = profile_rows(settings, defaults(), lang, err, write);
    let r = &rows;

    // Exposure: PHD2's duration list and Auto (−1).
    let exposure = {
        let rpc = rpc.clone();
        let reply = RwSignal::new(None::<Reply>);
        Effect::new(move |_| {
            if let Some(Err(e)) = reply.get().and_then(|r| r.get()) {
                err.set(Some(e.message));
            }
        });
        let cur = Memo::new(move |_| state.with(|s| s.as_ref().map_or(0, |s| s.exposure_ms)));
        let options = Memo::new(move |_| state.with(|s| s.as_ref().map(|s| s.exposure_options.clone()).unwrap_or_default()));
        move || {
            let rpc = rpc.clone();
            std::iter::once(-1)
                .chain(options.get())
                .map(|ms| {
                    let rpc = rpc.clone();
                    let label = if ms < 0 {
                        tr().guider_exposure_auto.to_string()
                    } else if ms < 1000 {
                        format!("{ms} ms")
                    } else {
                        format!("{} s", f64::from(ms as i32) / 1000.0)
                    };
                    view! {
                        <button type="button" class=move || if cur.get() == ms { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                                on:click=move |_| reply.set(Some(rpc.set_exposure(ms)))>{label}</button>
                    }
                })
                .collect_view()
        }
    };
    let auto_fields = {
        let rpc = rpc.clone();
        let ae = Memo::new(move |_| state.with(|s| s.as_ref().map(|s| s.auto_exposure.clone()).unwrap_or_default()));
        let (min, max, snr) = (RwSignal::new(0.0), RwSignal::new(0.0), RwSignal::new(0.0));
        // The fields as shown, refreshed from the daemon's (another client's edit).
        Effect::new(move |_| {
            ae.with(|a| {
                min.set(a.min as f64);
                max.set(a.max as f64);
                snr.set(a.snr);
            })
        });
        let reply = RwSignal::new(None::<Reply>);
        Effect::new(move |_| {
            if let Some(Err(e)) = reply.get().and_then(|r| r.get()) {
                err.set(Some(e.message));
            }
        });
        Effect::new(move |prev: Option<()>| {
            let (a, b, c) = (min.get(), max.get(), snr.get());
            // An edit is sent, the echo is not.
            let echo = ae.with_untracked(|e| (e.min as f64, e.max as f64, e.snr) == (a.round(), b.round(), c));
            if prev.is_some() && !echo {
                reply.set(Some(rpc.call("set_auto_exposure", json!({ "min": a.round() as i64, "max": b.round() as i64, "target_snr": c }))));
            }
        });
        view! {
            {local_num(move || tr().guider_auto_min, min, 10.0, 60000.0)}
            {local_num(move || tr().guider_auto_max, max, 10.0, 60000.0)}
            {local_num(move || tr().guider_auto_snr, snr, 3.5, 99.9)}
        }
    };
    let exposure_body = view! {
        <div class="flex flex-wrap gap-1.5 py-1">{exposure}</div>
        {auto_fields}
        {num_row(r, "/frame/timeLapse", |t| t.guider_time_lapse, 0.0, 10000.0, 1.0)}
        {bool_row(r, VAR_DELAY[0], |t| t.guider_var_delay)}
        {num_row_with(r, VAR_DELAY[1], |t| t.guider_var_delay_short, Num { min: 0.0, max: 10.0, step: 1.0, scale: 0.001, int: true })}
        {num_row_with(r, VAR_DELAY[2], |t| t.guider_var_delay_long, Num { min: 0.0, max: 120.0, step: 1.0, scale: 0.001, int: true })}
    };
    let noise_body = int_select(r, "/NoiseReductionMethod", |t| t.guider_noise, &[
        (0, |t| t.guider_noise_none),
        (1, |t| t.guider_noise_mean),
        (2, |t| t.guider_noise_median),
    ]);
    let star_body = view! {
        {num_row(r, "/guider/onestar/SearchRegion", |t| t.guider_search_region, 7.0, 50.0, 1.0)}
        {num_row(r, "/guider/StarMinSNR", |t| t.guider_min_snr, 6.0, 200.0, 0.5)}
        {num_row(r, "/guider/StarMinHFD", |t| t.guider_min_hfd, 0.1, 10.0, 0.1)}
        {num_row(r, "/guider/StarMaxHFD", |t| t.guider_max_hfd, 1.0, 50.0, 0.5)}
        {bool_row(r, "/guider/onestar/MassChangeThresholdEnabled", |t| t.guider_mass_change)}
        {num_row_with(r, "/guider/onestar/MassChangeThreshold", |t| t.guider_mass_change_pct, Num { min: 10.0, max: 100.0, step: 1.0, scale: 100.0, int: false })}
        {bool_row(r, "/guider/onestar/TolerateJumpsEnabled", |t| t.guider_tolerate_jumps)}
        {num_row(r, "/guider/onestar/TolerateJumpsThreshold", |t| t.guider_tolerate_jumps_px, 1.0, 1000.0, 0.5)}
        {bool_row(r, "/guider/multistar/enabled", |t| t.guider_multistar)}
        {int_select(r, "/guider/AutoSelDownsample", |t| t.guider_downsample, &[
            (0, |t| t.guider_downsample_auto),
            (1, |_| "1\u{00d7}1"),
            (2, |_| "2\u{00d7}2"),
            (3, |_| "3\u{00d7}3"),
        ])}
        {bool_row(r, "/guider/FastRecenter", |t| t.guider_fast_recenter)}
        {bool_row(r, "/StickyLockPosition", |t| t.guider_sticky_lock)}
    };
    let dither_body = {
        let set = rows.set.clone();
        let values = rows.values;
        let mode = Memo::new(move |_| values.with(|v| v.get(KEY_DITHER_MODE).and_then(Value::as_i64).unwrap_or(0)));
        let chips = [0_i64, 1].map(|m| {
            let set = set.clone();
            view! {
                <button type="button" class=move || if mode.get() == m { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                        on:click=move |_| set(KEY_DITHER_MODE, json!(m))>
                    {move || if m == 0 { tr().guider_dither_random } else { tr().guider_dither_spiral }}
                </button>
            }
        });
        view! {
            <div class="flex flex-wrap gap-1.5 py-1">{chips}</div>
            {bool_row(r, KEY_DITHER_RA_ONLY, |t| t.guider_dither_ra_only)}
            {num_row(r, KEY_DITHER_SCALE, |t| t.guider_dither_scale, 0.1, 100.0, 0.1)}
            <span class="text-xs text-text-muted pt-2">{move || tr().guider_settle_note}</span>
            {local_num(move || tr().guider_dither_amount, settle.dither_amount, 0.1, 100.0)}
            {local_num(move || tr().guider_settle_px, settle.pixels, 0.1, 100.0)}
            {local_num(move || tr().guider_settle_time, settle.time, 0.0, 600.0)}
            {local_num(move || tr().guider_settle_timeout, settle.timeout, 1.0, 3600.0)}
        }
    };
    let has_ao = Memo::new(move |_| state.with(|s| s.as_ref().is_some_and(|s| s.equipment.ao.is_some())));
    let ao_section = {
        let r = rows.clone();
        move || {
            has_ao.get().then(|| {
                let body = view! {
                    {num_row(&r, "/stepguider/SamplesToAverage", |t| t.guider_ao_samples, 0.0, 9.0, 1.0)}
                    {num_row(&r, "/stepguider/BumpPercentage", |t| t.guider_ao_bump_pct, 0.0, 99.0, 1.0)}
                    {num_row(&r, "/stepguider/BumpMaxStepsPerCycle", |t| t.guider_ao_bump_steps, 0.01, 99.99, 0.25)}
                    {bool_row(&r, "/stepguider/BumpOnDither", |t| t.guider_ao_bump_dither)}
                    {num_row(&r, "/stepguider/BumpSettlingBoost", |t| t.guider_ao_settle_boost, 1.0, 10.0, 0.5)}
                };
                section(move || t(lang.get()).guider_ao_title, false, body)
            })
        }
    };
    let server_body = bool_row(r, "/server/guide_allow_recalibrate", |t| t.guider_allow_recal);
    // "Save the next N frames": a one-off series, not stored.
    let next_n = RwSignal::new("1".to_string());
    let next_reply = RwSignal::new(None::<Reply>);
    Effect::new(move |_| {
        if let Some(Err(e)) = next_reply.get().and_then(|r| r.get()) {
            err.set(Some(e.message));
        }
    });
    let save_next = {
        let rpc = rpc.clone();
        move || {
            let mut p = logger_params(&stored);
            let n = next_n.with_untracked(|s| s.trim().parse::<i64>().unwrap_or(1)).clamp(1, 100);
            p.insert("next_n".into(), json!(true));
            p.insert("next_n_count".into(), json!(n));
            next_reply.set(Some(rpc.call("set_image_logger", Value::Object(p))));
        }
    };
    let logger_body = view! {
        {bool_row(r, IMAGE_LOGGER[0], |t| t.guider_imglog_enabled)}
        {bool_row(r, IMAGE_LOGGER[1], |t| t.guider_imglog_rel)}
        {num_row(r, IMAGE_LOGGER[2], |t| t.guider_imglog_rel_thresh, 1.0, 10.0, 0.1)}
        {bool_row(r, IMAGE_LOGGER[3], |t| t.guider_imglog_px)}
        {num_row(r, IMAGE_LOGGER[4], |t| t.guider_imglog_px_thresh, 1.0, 10.0, 0.1)}
        {bool_row(r, IMAGE_LOGGER[5], |t| t.guider_imglog_dropped)}
        {bool_row(r, IMAGE_LOGGER[6], |t| t.guider_imglog_autoselect)}
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || tr().guider_imglog_next_n}</span>
            <input type="number" min="1" max="100" step="1" class=NUM prop:value=move || next_n.get()
                   on:input=move |ev| next_n.set(event_target_value(&ev)) />
            <button class="btn btn-ghost h-9 px-3"
                    disabled=move || !settings.with(|s| s.get(IMAGE_LOGGER[0]).and_then(as_flag).unwrap_or(false))
                    on:click=move |_| save_next()>
                {move || tr().save_btn}
            </button>
        </div>
    };
    // Export settings: PHD2's `PHD Config 1` text, downloaded.
    let export = RwSignal::new(None::<Reply>);
    Effect::new(move |_| match export.get().and_then(|r| r.get()) {
        Some(Ok(v)) => {
            download_text("PHD2_settings.txt", v["text"].as_str().unwrap_or_default());
            export.set(None);
        }
        Some(Err(e)) => {
            err.set(Some(e.message));
            export.set(None);
        }
        None => {}
    });

    let title = |f: fn(&Translations) -> &'static str| move || f(t(lang.get()));
    let (rpc_p, rpc_l, rpc_x) = (rpc.clone(), rpc.clone(), rpc.clone());
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 flex flex-col gap-3">
            {move || err.get().map(|m| view! { <div class="text-sm text-state-err">{m}</div> })}
            <div class="grid gap-3 md:grid-cols-2 items-start">
                <div class="flex flex-col gap-3">
                    {section(title(|t| t.guider_exposure_title), true, exposure_body)}
                    {section(title(|t| t.guider_star_title), false, star_body)}
                    {section(title(|t| t.guider_dither_title), false, dither_body)}
                    {section(title(|t| t.guider_noise_title), false, noise_body)}
                </div>
                <div class="flex flex-col gap-3">
                    {ao_section}
                    {section(title(|t| t.guider_server_title), false, server_body)}
                    {section(title(|t| t.guider_imglog_title), false, logger_body)}
                    {pulses_card(state, rpc_p, lang)}
                    {lock_shift_card(state, tools, rpc_l, lang)}
                </div>
            </div>
        </div>
        <div class="shrink-0 flex items-center justify-end gap-3 px-4 pt-2 pb-[max(0.5rem,env(safe-area-inset-bottom))] border-t border-border-base">
            <button class="btn btn-ghost h-11 px-4" on:click=move |_| export.set(Some(rpc_x.call("export_settings", json!({}))))>
                {move || tr().guider_export_settings}
            </button>
        </div>
    }
}
