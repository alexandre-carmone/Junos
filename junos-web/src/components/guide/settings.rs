//! Guide tab: the settings sheet — every guide parameter, in collapsible
//! cards, each change sent to KStars straight away.
//!
//! - Module widgets go out as `guide_set_all_settings {<widgetName>: value}`
//!   (map at the payload root, message.cpp:673) and come back with the
//!   periodic `guide_get_all_settings` refresh. Combos travel as their text.
//! - GuiderType and the PHD2 / LinGuider host + port are global `Options::`
//!   entries: `option_set`, then re-read with `option_get` (KStars doesn't
//!   echo `option_set`).
//! - Save calibration is `guide_set_calibration_settings` (message.cpp:679),
//!   a bulk save of the 8 calibration fields from the current widget map.

use leptos::prelude::*;
use serde_json::Value;

use crate::components::form::{CARD_TITLE, CHECK, CHIP, FOOTER, LABEL, ROW};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::{dispatch_setting, send_cmd};

type LabelFn = fn(&Translations) -> &'static str;

const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";
const SELECT: &str = "input input--sm w-[180px] shrink-0 max-md:h-9";

// Combo option lists (sourced from kstars/ekos/guide/*.ui).
const BINNING_OPTIONS: &[&str] = &["1x1", "2x2", "3x3", "4x4"];
const SQUARE_OPTIONS: &[&str] = &["8", "16", "32", "64", "128"];
const PULSE_ALGO_OPTS: &[&str] = &["Standard", "Hysteresis", "Linear", "GPG"];
const GUIDE_ALGO_OPTS: &[&str] = &[
    "Smart",
    "SEP",
    "Fast",
    "Auto Threshold",
    "No Threshold",
    "SEP Multi Star (recommended)",
];

/// `Options::GuiderType` values.
pub const GUIDERS: [(i64, LabelFn); 3] = [
    (0, |t| t.guide_internal),
    (1, |t| t.guide_phd2_label),
    (2, |t| t.guide_linguider_label),
];

pub fn settings_i64(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
}

fn settings_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| match x {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

/// Set one `Options::` entry, then re-read the guide options.
fn set_option(send: &SendCmd, name: &str, value: Value) {
    send_cmd(send, "option_set", serde_json::json!({ "options": [{ "name": name, "value": value }] }));
    refresh_options(send);
}

pub fn refresh_options(send: &SendCmd) {
    send_cmd(send, "option_get", serde_json::json!({ "options": [
        {"name": "GuiderType"}, {"name": "PHD2Host"}, {"name": "PHD2Port"},
        {"name": "LinGuiderHost"}, {"name": "LinGuiderPort"},
    ]}));
}

/// `guide_set_calibration_settings` with the 8 fields its handler reads
/// (message.cpp:679-690), taken from the current `kcfg_*` widget map.
fn save_calibration(send: &SendCmd, s: &Value) {
    let int = |k: &str, d: i64| settings_i64(s, k).unwrap_or(d);
    let flag = |k: &str, d: bool| s.get(k).and_then(Value::as_bool).unwrap_or(d);
    send_cmd(send, "guide_set_calibration_settings", serde_json::json!({
        "pulse":               int("kcfg_CalibrationPulseDuration", 1000),
        "max_move":            int("kcfg_CalibrationMaxMove", 20),
        "two_axis":            flag("kcfg_TwoAxisEnabled", true),
        "square_size":         flag("kcfg_GuideAutoSquareSizeEnabled", false),
        "calibrationBacklash": flag("kcfg_GuideCalibrationBacklash", false),
        "resetCalibration":    flag("kcfg_ResetGuideCalibration", false),
        "reuseCalibration":    flag("kcfg_ReuseGuideCalibration", false),
        "reverseCalibration":  flag("kcfg_ReverseDecOnPierSideChange", false),
    }));
}

/// What every row needs: the command sink, both value maps and the language.
#[derive(Clone)]
struct Ctx {
    send: SendCmd,
    settings: Memo<Value>,
    options: Memo<Value>,
    lang: RwSignal<Lang>,
}

impl Ctx {
    fn label(&self, label: LabelFn) -> impl Fn() -> &'static str + Copy + Send + 'static + use<> {
        let lang = self.lang;
        move || label(t(lang.get()))
    }

    fn set(&self, key: &str, value: Value) {
        dispatch_setting(&self.send, "guide_set_all_settings", None, key, value);
    }
}

/// Checkbox row for a boolean widget.
fn bool_row(c: &Ctx, key: &'static str, label: LabelFn) -> impl IntoView + use<> {
    let (cc, settings) = (c.clone(), c.settings);
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || settings.with(|s| s.get(key).and_then(Value::as_bool).unwrap_or(false))
                   on:change=move |ev| cc.set(key, Value::Bool(event_target_checked(&ev))) />
            <span class=LABEL>{c.label(label)}</span>
        </label>
    }
}

/// Number row. A whole `step` sends an integer (QSpinBox), else a float
/// (QDoubleSpinBox); either is clamped to `min..=max`.
fn num_row(c: &Ctx, key: &'static str, label: LabelFn, min: f64, max: f64, step: f64) -> impl IntoView + use<> {
    let (cc, settings) = (c.clone(), c.settings);
    let int = step.fract() == 0.0;
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{c.label(label)}</span>
            <input type="number" inputmode="decimal" class=NUM
                   min=min.to_string() max=max.to_string() step=step.to_string()
                   prop:value=move || settings.with(|s| s.get(key).and_then(Value::as_f64).map(|v| v.to_string()).unwrap_or_default())
                   on:change=move |ev| {
                       let Ok(v) = event_target_value(&ev).parse::<f64>() else { return };
                       let v = v.clamp(min, max);
                       cc.set(key, if int { Value::from(v.round() as i64) } else { Value::from(v) });
                   } />
        </div>
    }
}

/// Combo row; a current value missing from `options` is kept selectable.
fn select_row(c: &Ctx, key: &'static str, label: LabelFn, options: &'static [&'static str]) -> impl IntoView + use<> {
    let (cc, settings) = (c.clone(), c.settings);
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{c.label(label)}</span>
            <select class=SELECT on:change=move |ev| cc.set(key, Value::String(event_target_value(&ev)))>
                {move || {
                    let cur = settings.with(|s| settings_str(s, key).unwrap_or_default());
                    let extra = (!cur.is_empty() && !options.contains(&cur.as_str())).then(|| cur.clone());
                    extra.into_iter().chain(options.iter().map(|o| o.to_string())).map(|o| {
                        let (selected, text) = (o == cur, o.clone());
                        view! { <option value=o prop:selected=selected>{text}</option> }
                    }).collect::<Vec<_>>()
                }}
            </select>
        </div>
    }
}

/// Host (text) or port (1–65535) row for an `Options::` entry.
fn option_row(c: &Ctx, name: &'static str, label: LabelFn, port: bool) -> impl IntoView + use<> {
    let (send, options) = (c.send.clone(), c.options);
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{c.label(label)}</span>
            <input type=if port { "number" } else { "text" }
                   class=if port { NUM } else { "input input--sm font-mono w-[180px] shrink-0 max-md:h-9" }
                   prop:value=move || options.with(|o| settings_str(o, name).unwrap_or_default())
                   on:change=move |ev| {
                       let v = event_target_value(&ev);
                       let value = if port {
                           match v.trim().parse::<i64>() { Ok(n) => Value::from(n.clamp(1, 65535)), Err(_) => return }
                       } else {
                           Value::String(v.trim().to_string())
                       };
                       set_option(&send, name, value);
                   } />
        </div>
    }
}

/// A collapsible card.
fn section(title: impl Fn() -> &'static str + Send + 'static, open: bool, body: impl IntoView) -> impl IntoView {
    view! {
        <details class="panel group" open=open>
            <summary class="flex items-center gap-2 px-3 min-h-[44px] md:min-h-10 cursor-pointer select-none \
                            list-none [&::-webkit-details-marker]:hidden">
                <span class=format!("{CARD_TITLE} flex-1")>{move || title()}</span>
                <span class="text-text-faint transition-transform group-open:rotate-90">"\u{203a}"</span>
            </summary>
            <div class="px-3 pb-3 flex flex-col">{body}</div>
        </details>
    }
}

#[component]
pub fn GuideSettings(
    settings: Memo<Value>,
    options: Memo<Value>,
    status: Memo<String>,
    #[prop(into)] send: SendCmd,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let c = Ctx { send: send.clone(), settings, options, lang };
    let guider = Memo::new(move |_| options.with(|o| settings_i64(o, "GuiderType").unwrap_or(0)));

    let s_save = send.clone();
    let on_save_cal = move |_| save_calibration(&s_save, &settings.get_untracked());
    let s_clear = send.clone();
    let on_clear_cal = move |_| send_cmd(&s_clear, "guide_clear", serde_json::json!({}));
    let s_refresh = send.clone();
    let on_refresh = move |_| {
        send_cmd(&s_refresh, "guide_get_all_settings", serde_json::json!({}));
        refresh_options(&s_refresh);
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-3 flex flex-col gap-3">
            {section(c.label(|t| t.guide_backend), true, view! {
                <div class="flex flex-wrap gap-1.5 py-1">
                    {GUIDERS.map(|(v, label)| {
                        let send = send.clone();
                        view! {
                            <button type="button"
                                    class=move || if guider.get() == v { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                                    on:click=move |_| set_option(&send, "GuiderType", Value::from(v))>
                                {c.label(label)}
                            </button>
                        }
                    })}
                </div>
                <div class="flex flex-col" class:hidden=move || guider.get() != 1>
                    {option_row(&c, "PHD2Host", |t| t.guide_host, false)}
                    {option_row(&c, "PHD2Port", |t| t.guide_port, true)}
                </div>
                <div class="flex flex-col" class:hidden=move || guider.get() != 2>
                    {option_row(&c, "LinGuiderHost", |t| t.guide_host, false)}
                    {option_row(&c, "LinGuiderPort", |t| t.guide_port, true)}
                </div>
            })}

            {section(c.label(|t| t.guide_essentials), true, view! {
                {num_row   (&c, "guideExposure",    |t| t.guide_f_exposure,     0.1, 60.0, 0.1)}
                {num_row   (&c, "guideDelay",       |t| t.guide_f_delay,        0.0, 60.0, 0.1)}
                {num_row   (&c, "guideGain",        |t| t.guide_f_gain,         0.0, 1000.0, 1.0)}
                {select_row(&c, "guideBinning",     |t| t.guide_f_binning,      BINNING_OPTIONS)}
                {select_row(&c, "guideSquareSize",  |t| t.guide_f_tracking_box, SQUARE_OPTIONS)}
                {bool_row  (&c, "guideDarkFrame",   |t| t.guide_f_dark_frame)}
                {bool_row  (&c, "guideSubframe",    |t| t.guide_f_subframe)}
                {bool_row  (&c, "guideAutoStar",    |t| t.guide_f_auto_star)}
                {bool_row  (&c, "guideStreamingEnabled", |t| t.guide_f_stream)}
            })}

            {section(c.label(|t| t.guide_ra_dec_corrections), false, view! {
                {bool_row(&c, "rAGuideEnabled",       |t| t.guide_f_ra_guiding)}
                {bool_row(&c, "eastRAGuideEnabled",   |t| t.guide_f_east_pulses)}
                {bool_row(&c, "westRAGuideEnabled",   |t| t.guide_f_west_pulses)}
                {bool_row(&c, "dECGuideEnabled",      |t| t.guide_f_dec_guiding)}
                {bool_row(&c, "northDECGuideEnabled", |t| t.guide_f_north_pulses)}
                {bool_row(&c, "southDECGuideEnabled", |t| t.guide_f_south_pulses)}
            })}

            {section(c.label(|t| t.guide_calibration), false, view! {
                {num_row (&c, "kcfg_AutoModeIterations",         |t| t.guide_f_iterations,       1.0, 100.0, 1.0)}
                {num_row (&c, "kcfg_CalibrationPulseDuration",   |t| t.guide_f_pulse_duration,   100.0, 10000.0, 100.0)}
                {num_row (&c, "kcfg_CalibrationMaxMove",         |t| t.guide_f_max_move,         1.0, 200.0, 1.0)}
                {bool_row(&c, "kcfg_TwoAxisEnabled",             |t| t.guide_f_two_axis)}
                {bool_row(&c, "kcfg_GuideAutoSquareSizeEnabled", |t| t.guide_f_auto_box_size)}
                {bool_row(&c, "kcfg_GuideCalibrationBacklash",   |t| t.guide_f_dec_backlash)}
                {bool_row(&c, "kcfg_ResetGuideCalibration",      |t| t.guide_f_reset_each_start)}
                {bool_row(&c, "kcfg_ReuseGuideCalibration",      |t| t.guide_f_reuse_cal)}
                {bool_row(&c, "kcfg_ReverseDecOnPierSideChange", |t| t.guide_f_reverse_dec_flip)}
                <div class="flex gap-2 pt-2">
                    // Clearing mid-calibration / guiding / dithering would pull
                    // the rug from under KStars.
                    <button class="btn btn-ghost flex-1 max-md:h-11"
                            disabled=move || matches!(status.get().as_str(), "Calibrating" | "Guiding" | "Dithering")
                            on:click=on_clear_cal>
                        {move || tr().guide_clear_cal}
                    </button>
                    <button class="btn btn-primary flex-1 max-md:h-11" on:click=on_save_cal>
                        {move || tr().guide_save_calibration}
                    </button>
                </div>
            })}

            {section(c.label(|t| t.guide_dither), false, view! {
                <div class="text-xs text-text-faint leading-snug pb-1">{move || tr().guide_dither_note}</div>
                {bool_row(&c, "kcfg_DitherEnabled",             |t| t.guide_f_dither_enabled)}
                {num_row (&c, "kcfg_DitherPixels",              |t| t.guide_f_dither_amount,         0.1, 30.0, 0.1)}
                {num_row (&c, "kcfg_DitherFrames",              |t| t.guide_f_dither_frames,         1.0, 100.0, 1.0)}
                {num_row (&c, "kcfg_DitherThreshold",           |t| t.guide_f_dither_settle_thr,     0.1, 10.0, 0.1)}
                {num_row (&c, "kcfg_DitherSettle",              |t| t.guide_f_dither_settle_t,       0.0, 300.0, 1.0)}
                {num_row (&c, "kcfg_DitherTimeout",             |t| t.guide_f_dither_timeout,        1.0, 600.0, 1.0)}
                {num_row (&c, "kcfg_DitherMaxIterations",       |t| t.guide_f_dither_max_iter,       1.0, 100.0, 1.0)}
                {bool_row(&c, "kcfg_DitherWithOnePulse",        |t| t.guide_f_dither_one_pulse)}
                {bool_row(&c, "kcfg_DitherFailAbortsAutoGuide", |t| t.guide_f_dither_fail_abort)}
                {bool_row(&c, "kcfg_DitherNoGuiding",           |t| t.guide_f_dither_no_guiding)}
                {num_row (&c, "kcfg_DitherNoGuidingPulse",      |t| t.guide_f_dither_no_guide_pulse, 100.0, 10000.0, 100.0)}
            })}

            {section(c.label(|t| t.guide_algorithms), false, view! {
                {select_row(&c, "kcfg_GuideAlgorithm",            |t| t.guide_f_detection,      GUIDE_ALGO_OPTS)}
                {select_row(&c, "kcfg_RAGuidePulseAlgorithm",     |t| t.guide_f_ra_pulse_algo,  PULSE_ALGO_OPTS)}
                {select_row(&c, "kcfg_DECGuidePulseAlgorithm",    |t| t.guide_f_dec_pulse_algo, PULSE_ALGO_OPTS)}
                {num_row   (&c, "kcfg_RAProportionalGain",        |t| t.guide_f_ra_kp,          0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_RAIntegralGain",            |t| t.guide_f_ra_ki,          0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_RAMinimumPulseArcSec",      |t| t.guide_f_ra_min_pulse,   0.0, 10.0, 0.01)}
                {num_row   (&c, "kcfg_RAMaximumPulseArcSec",      |t| t.guide_f_ra_max_pulse,   0.0, 30.0, 0.1)}
                {num_row   (&c, "kcfg_RAHysteresis",              |t| t.guide_f_ra_hysteresis,  0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_DECProportionalGain",       |t| t.guide_f_dec_kp,         0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_DECIntegralGain",           |t| t.guide_f_dec_ki,         0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_DECMinimumPulseArcSec",     |t| t.guide_f_dec_min_pulse,  0.0, 10.0, 0.01)}
                {num_row   (&c, "kcfg_DECMaximumPulseArcSec",     |t| t.guide_f_dec_max_pulse,  0.0, 30.0, 0.1)}
                {num_row   (&c, "kcfg_DECHysteresis",             |t| t.guide_f_dec_hysteresis, 0.0, 1.0, 0.01)}
                {num_row   (&c, "kcfg_GuideMaxDeltaRMS",          |t| t.guide_f_max_drms,       0.0, 30.0, 0.1)}
                {num_row   (&c, "kcfg_GuideMaxHFR",               |t| t.guide_f_max_hfr,        0.0, 30.0, 0.1)}
                {num_row   (&c, "kcfg_GuideLostStarTimeout",      |t| t.guide_f_lost_star_to,   1.0, 600.0, 1.0)}
                {num_row   (&c, "kcfg_GuideCalibrationTimeout",   |t| t.guide_f_cal_timeout,    1.0, 600.0, 1.0)}
                {num_row   (&c, "kcfg_MinDetectionsSEPMultistar", |t| t.guide_f_sep_min,        1.0, 200.0, 1.0)}
                {num_row   (&c, "kcfg_MaxMultistarReferenceStars",|t| t.guide_f_sep_max_ref,    1.0, 200.0, 1.0)}
            })}

            {section(c.label(|t| t.guide_advanced), false, view! {
                {bool_row(&c, "kcfg_SaveGuideLog",          |t| t.guide_f_save_log)}
                {bool_row(&c, "kcfg_UseGuideHead",          |t| t.guide_f_use_guide_head)}
                {bool_row(&c, "kcfg_AlwaysInventGuideStar", |t| t.guide_f_invent_star)}
                {bool_row(&c, "latestCheck",                |t| t.guide_f_latest_checks)}
                {num_row (&c, "guiderAccuracyThreshold",    |t| t.guide_f_accuracy_thr, 0.0, 10.0, 0.1)}
                <span class=format!("{CARD_TITLE} pt-3 pb-1")>{move || tr().guide_gpg}</span>
                {num_row (&c, "kcfg_GPGPeriod",                      |t| t.guide_f_gpg_period,             1.0, 3600.0, 1.0)}
                {bool_row(&c, "kcfg_GPGEstimatePeriod",              |t| t.guide_f_gpg_estimate_period)}
                {bool_row(&c, "kcfg_GPGDarkGuiding",                 |t| t.guide_f_gpg_dark)}
                {num_row (&c, "kcfg_GPGDarkGuidingInterval",         |t| t.guide_f_gpg_dark_interval,      1.0, 600.0, 1.0)}
                {num_row (&c, "kcfg_GPGpWeight",                     |t| t.guide_f_gpg_p_weight,           0.0, 1.0, 0.01)}
                {num_row (&c, "kcfg_GPGSE0KLengthScale",             |t| t.guide_f_gpg_se0_length,         0.0, 1000.0, 1.0)}
                {num_row (&c, "kcfg_GPGSE0KSignalVariance",          |t| t.guide_f_gpg_se0_signal,         0.0, 10.0, 0.01)}
                {num_row (&c, "kcfg_GPGPKLengthScale",               |t| t.guide_f_gpg_pk_length,          0.0, 1000.0, 1.0)}
                {num_row (&c, "kcfg_GPGPKSignalVariance",            |t| t.guide_f_gpg_pk_signal,          0.0, 10.0, 0.01)}
                {num_row (&c, "kcfg_GPGSE1KLengthScale",             |t| t.guide_f_gpg_se1_length,         0.0, 1000.0, 1.0)}
                {num_row (&c, "kcfg_GPGSE1KSignalVariance",          |t| t.guide_f_gpg_se1_signal,         0.0, 10.0, 0.01)}
                {num_row (&c, "kcfg_GPGPointsForApproximation",      |t| t.guide_f_gpg_points_approx,      1.0, 10000.0, 10.0)}
                {num_row (&c, "kcfg_GPGMinPeriodsForInference",      |t| t.guide_f_gpg_min_periods_inf,    1.0, 100.0, 1.0)}
                {num_row (&c, "kcfg_GPGMinPeriodsForPeriodEstimate", |t| t.guide_f_gpg_min_periods_period, 1.0, 100.0, 1.0)}
            })}
        </div>
        <div class=FOOTER>
            <button class="btn btn-ghost h-11 px-4 ml-auto" on:click=on_refresh>
                {move || format!("\u{21bb} {}", tr().guide_refresh_settings)}
            </button>
        </div>
    }
}
