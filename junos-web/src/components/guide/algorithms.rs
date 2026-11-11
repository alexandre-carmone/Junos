//! The native Guide tab's Algorithms sheet: PHD2's Guiding › Algorithms
//! tab. Per axis (the mount's RA and Dec, an AO's X and Y once one is
//! connected) the algorithm, limited to what PHD2 offers there (Predictive
//! PEC on RA only, an AO without it or Resist Switch), and that
//! algorithm's parameters with PHD2's ranges, units and defaults
//! (`protocol::ALGOS`), the Predictive PEC expert ones folded; then the
//! mount's rows (max durations, Dec guide mode, the calibration step,
//! backlash compensation and the flags of PHD2's Mount tab). Smart
//! min-move writes PHD2's suggested minimum move for the image scale, and
//! Reset puts an axis's parameters back to PHD2's defaults.
//!
//! The event server's setters act on PHD2's `pMount`, the AO when there is
//! one: its axes go through `set_algorithm` / `set_algo_param` (live),
//! `set_dec_guide_mode` for the Dec guide mode; every other row through
//! `set_profile_value`. Each edit stays shown until `guider_settings`
//! echoes it.

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{setting_row, CHECK, CHIP, LABEL, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::GuiderStatusData;

use super::protocol::{
    algo, algo_by_class, profile_rows, ALGOS, second_tap, AlgoParam, Reply, Rpc, AO_ALGOS, DEC_ALGOS, KEY_CAL_DISTANCE,
    KEY_CAL_DURATION, KEY_DEC_GUIDE_MODE, KEY_MAX_DEC_DURATION, KEY_MAX_RA_DURATION, RA_ALGOS,
};
use super::rows::{bool_row, num_row, num_row_with, section, LabelFn, Num, Rows};

/// PHD2's `DEC_GUIDE_MODE` order (`/scope/DecGuideMode`), as
/// `set_dec_guide_mode` spells them.
const DEC_MODES: [&str; 4] = ["Off", "Auto", "North", "South"];

/// A profile key built at run time, kept for the life of the page (the
/// rows key their values by `&'static str`; the set is small and fixed).
fn intern(s: String) -> &'static str {
    thread_local! {
        static KEYS: RefCell<HashMap<String, &'static str>> = RefCell::new(HashMap::new());
    }
    KEYS.with(|k| *k.borrow_mut().entry(s.clone()).or_insert_with(|| s.leak()))
}

/// One axis block: the profile group, X or Y, the event server's name for
/// it, its label and the algorithms it offers.
#[derive(Clone, Copy)]
struct AxisDef {
    prefix: &'static str,
    xy: &'static str,
    axis: &'static str,
    label: LabelFn,
    allowed: &'static [i64],
    default: i64,
}

const AXES: [AxisDef; 4] = [
    AxisDef { prefix: "/scope", xy: "X", axis: "ra", label: |t| t.ra_label, allowed: RA_ALGOS, default: 1 },
    AxisDef { prefix: "/scope", xy: "Y", axis: "dec", label: |t| t.dec_label, allowed: DEC_ALGOS, default: 4 },
    AxisDef { prefix: "/stepguider", xy: "X", axis: "ra", label: |t| t.guider_ao_x, allowed: AO_ALGOS, default: 1 },
    AxisDef { prefix: "/stepguider", xy: "Y", axis: "dec", label: |t| t.guider_ao_y, allowed: AO_ALGOS, default: 1 },
];

impl AxisDef {
    fn choice_key(&self) -> &'static str {
        intern(format!("{}/{}GuideAlgorithm", self.prefix, self.xy))
    }

    fn param_key(&self, class: &str, p: &AlgoParam) -> &'static str {
        intern(format!("{}/GuideAlgorithm/{}/{class}/{}", self.prefix, self.xy, p.key))
    }
}

/// A parameter's label, its unit included.
fn param_label(key: &str) -> LabelFn {
    match key {
        "hysteresis" => |t| t.guider_algo_hysteresis,
        "aggression" => |t| t.guider_algo_aggression,
        "minMove" | "gp_min_move" => |t| t.guider_algo_min_move,
        "SlopeWeight" => |t| t.guider_algo_slope_weight,
        "Aggressiveness" => |t| t.guider_algo_aggressiveness,
        "fastSwitch" => |t| t.guider_algo_fast_switch,
        "gp_prediction_gain" => |t| t.guider_algo_prediction_gain,
        "gp_control_gain" => |t| t.guider_algo_control_gain,
        "gp_period_per_kern" => |t| t.guider_algo_period,
        "gp_compute_period" => |t| t.guider_algo_compute_period,
        "gp_points_for_approximation" => |t| t.guider_algo_points,
        "gp_period_lengths_inference" => |t| t.guider_algo_periods_inference,
        "gp_period_lengths_period_estimation" => |t| t.guider_algo_periods_estimation,
        "gp_length_scale_se0_kern" => |t| t.guider_algo_se0_length,
        "gp_sigvar_se0_kern" => |t| t.guider_algo_se0_sigvar,
        "gp_length_scale_per_kern" => |t| t.guider_algo_per_length,
        "gp_sigvar_per_kern" => |t| t.guider_algo_per_sigvar,
        "gp_length_scale_se1_kern" => |t| t.guider_algo_se1_length,
        "gp_sigvar_se1_kern" => |t| t.guider_algo_se1_sigvar,
        "noreset_max_pct_period" => |t| t.guider_algo_noreset,
        "expFactor" => |t| t.guider_algo_exp_factor,
        _ => |t| t.guider_algo_param,
    }
}

/// The Predictive PEC expert dialog's parameters (no event-server name).
fn advanced(p: &AlgoParam) -> bool {
    p.rpc.is_none() && !p.flag
}

/// A 0/1 parameter: a bool in the profile, a number on the event server.
fn flag_row(r: &Rows, key: &'static str, label: LabelFn) -> impl IntoView + use<> {
    let (set, values) = (r.set.clone(), r.values);
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK
                   prop:checked=move || values.with(|s| {
                       s.get(key).and_then(|v| v.as_bool().or(v.as_f64().map(|x| x != 0.0))).unwrap_or(false)
                   })
                   on:change=move |ev| set(key, Value::Bool(event_target_checked(&ev))) />
            <span class=LABEL>{r.label(label)}</span>
        </label>
    }
}

fn param_row(r: &Rows, key: &'static str, p: &'static AlgoParam) -> AnyView {
    let label = param_label(p.key);
    if p.flag {
        return flag_row(r, key, label).into_any();
    }
    let int = p.key == "gp_points_for_approximation";
    num_row_with(r, key, label, Num { min: p.min, max: p.max, step: p.step, scale: p.scale, int }).into_any()
}

/// The sheet's body.
pub fn algorithms_sheet(
    state: RwSignal<Option<GuiderStatusData>>,
    settings: RwSignal<Value>,
    rpc: Rpc,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let err = RwSignal::new(None::<String>);
    let ao = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.equipment.ao.as_ref()).is_some_and(|a| a.connected)));
    let scale = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.pixel_scale)));

    // PHD2's defaults for what the profile never stored.
    let mut defaults: Vec<(&'static str, Value)> = vec![
        (KEY_MAX_RA_DURATION, json!(2500)),
        (KEY_MAX_DEC_DURATION, json!(2500)),
        (KEY_DEC_GUIDE_MODE, json!(1)),
        (KEY_CAL_DURATION, json!(750)),
        (KEY_CAL_DISTANCE, json!(25)),
        ("/scope/UseDecComp", json!(true)),
        ("/scope/AssumeOrthogonal", json!(false)),
        ("/scope/CalFlipRequiresDecFlip", json!(false)),
        ("/scope/StopGuidingWhenSlewing", json!(true)),
        ("/scope/HiResEncoders", json!(false)),
        ("/scope/BacklashCompEnabled", json!(false)),
    ];
    for a in &AXES {
        defaults.push((a.choice_key(), json!(a.default)));
        for al in ALGOS {
            for p in al.params {
                defaults.push((a.param_key(al.class, p), json!(p.default)));
            }
        }
    }

    // pMount is the AO when one is connected: the live setters act on it.
    let write = {
        let rpc = rpc.clone();
        move |key: &'static str, v: Value| -> Reply {
            let primary = if ao.get_untracked() { "/stepguider" } else { "/scope" };
            let axis = |xy: &str| if xy == "X" { "ra" } else { "dec" };
            if let Some(rest) = key.strip_prefix(primary) {
                if let Some(xy) = rest.strip_prefix('/').and_then(|r| r.strip_suffix("GuideAlgorithm")) {
                    return rpc.set_algorithm(axis(xy), v.as_i64().unwrap_or(0));
                }
                let mut it = rest.strip_prefix("/GuideAlgorithm/").unwrap_or_default().splitn(3, '/');
                if let (Some(xy), Some(class), Some(name)) = (it.next(), it.next(), it.next()) {
                    let p = algo_by_class(class).and_then(|a| a.params.iter().find(|p| p.key == name));
                    if let Some(rpc_name) = p.and_then(|p| p.rpc) {
                        let x = v.as_f64().or(v.as_bool().map(|b| f64::from(u8::from(b)))).unwrap_or(0.0);
                        return rpc.set_algo_param(axis(xy), rpc_name, x);
                    }
                }
            }
            if key == KEY_DEC_GUIDE_MODE {
                let i = v.as_u64().unwrap_or(1).min(3) as usize;
                return rpc.set_dec_guide_mode(DEC_MODES[i]);
            }
            rpc.set_profile_value(key, v)
        }
    };
    let rows = profile_rows(settings, defaults, lang, err, write);
    let values = rows.values;

    // Reset: `reset_algo_params` on pMount's axes, the table's defaults on
    // the other mount's.
    let resets = RwSignal::new(Vec::<Reply>::new());
    Effect::new(move |_| {
        for r in resets.get() {
            if let Some(Err(e)) = r.get() {
                err.set(Some(e.message));
            }
        }
    });

    let block_rows = rows.clone();
    let axis_block = move |a: AxisDef| {
        let rows = block_rows.clone();
        let choice = a.choice_key();
        let kind = Memo::new(move |_| values.with(|v| v.get(choice).and_then(Value::as_i64).unwrap_or(a.default)));
        let set = rows.set.clone();
        let select = view! {
            <select class=SELECT on:change=move |ev| set(choice, json!(event_target_value(&ev).parse::<i64>().unwrap_or(a.default)))>
                {a.allowed.iter().filter_map(|k| algo(*k)).map(|al| {
                    let k = al.kind;
                    view! { <option value=k.to_string() prop:selected=move || kind.get() == k>{al.name}</option> }
                }).collect_view()}
            </select>
        };
        let params = {
            let rows = rows.clone();
            move || {
                let Some(al) = algo(kind.get()) else { return ().into_any() };
                let (plain, expert): (Vec<_>, Vec<_>) = al.params.iter().partition(|p| !advanced(p));
                let row = |p: &&'static AlgoParam| param_row(&rows, a.param_key(al.class, p), *p);
                let plain = plain.iter().map(row).collect_view();
                let expert = (!expert.is_empty()).then(|| {
                    let rows = expert.iter().map(row).collect_view();
                    view! {
                        <details class="pt-1">
                            <summary class="cursor-pointer text-sm text-text-muted select-none min-h-9 flex items-center">
                                {move || tr().guider_algo_advanced}
                            </summary>
                            {rows}
                        </details>
                    }
                });
                view! { {plain} {expert} }.into_any()
            }
        };
        let armed = RwSignal::new(false);
        let (rpc, set) = (rpc.clone(), rows.set.clone());
        let reset = move |_| {
            if !second_tap(armed) {
                return;
            }
            let primary = if ao.get_untracked() { "/stepguider" } else { "/scope" };
            if a.prefix == primary {
                let r = rpc.reset_algo_params(a.axis);
                resets.update(|v| v.push(r));
            } else if let Some(al) = algo(kind.get_untracked()) {
                for p in al.params {
                    set(a.param_key(al.class, p), json!(p.default));
                }
            }
        };
        section(move || (a.label)(t(lang.get())), true, view! {
            {setting_row(move || tr().guider_algo, select)}
            {params}
            <div class="flex justify-end pt-1">
                <button class=move || if armed.get() { "btn btn-danger h-9 px-3" } else { "btn btn-ghost h-9 px-3" }
                        on:click=reset>
                    {move || if armed.get() { tr().guider_tap_to_confirm } else { tr().guider_algo_reset }}
                </button>
            </div>
        })
    };

    // Smart min-move: PHD2's suggestion for the image scale, on every axis
    // shown, into its algorithm's min-move.
    let smart = {
        let set = rows.set.clone();
        move |_| {
            let Some(k) = scale.get_untracked() else { return };
            let mm = ((0.1515 + 0.1548 / k).max(0.15) * 100.0).round() / 100.0;
            for a in AXES.iter().filter(|a| a.prefix == "/scope" || ao.get_untracked()) {
                let kind = values.with_untracked(|v| v.get(a.choice_key()).and_then(Value::as_i64).unwrap_or(a.default));
                if let Some(al) = algo(kind) {
                    for p in al.params.iter().filter(|p| p.key == "minMove" || p.key == "gp_min_move") {
                        set(a.param_key(al.class, p), json!(mm));
                    }
                }
            }
        }
    };

    let dec_mode = {
        let set = rows.set.clone();
        let cur = Memo::new(move |_| values.with(|v| v.get(KEY_DEC_GUIDE_MODE).and_then(Value::as_i64).unwrap_or(1)));
        let chips = (0..DEC_MODES.len())
            .map(|i| {
                let set = set.clone();
                let label = move || {
                    let tr = tr();
                    [tr.guider_dec_off, tr.guider_dec_auto, tr.guider_dec_north, tr.guider_dec_south][i]
                };
                view! {
                    <button type="button" class=move || if cur.get() == i as i64 { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                            on:click=move |_| set(KEY_DEC_GUIDE_MODE, json!(i))>{label}</button>
                }
            })
            .collect_view();
        view! {
            <div class="flex flex-col gap-1 py-1">
                <span class=LABEL>{move || tr().guider_dec_guide_mode}</span>
                <div class="flex flex-wrap gap-1.5">{chips}</div>
            </div>
        }
    };

    let r = &rows;
    let mount_body = view! {
        {num_row(r, KEY_MAX_RA_DURATION, |t| t.guider_max_ra, 50.0, 8000.0, 1.0)}
        {num_row(r, KEY_MAX_DEC_DURATION, |t| t.guider_max_dec, 50.0, 8000.0, 1.0)}
        {dec_mode}
        {bool_row(r, "/scope/UseDecComp", |t| t.guider_dec_comp)}
        {bool_row(r, "/scope/AssumeOrthogonal", |t| t.guider_assume_ortho)}
        {bool_row(r, "/scope/CalFlipRequiresDecFlip", |t| t.guider_flip_dec)}
        {bool_row(r, "/scope/StopGuidingWhenSlewing", |t| t.guider_stop_slewing)}
        {bool_row(r, "/scope/HiResEncoders", |t| t.guider_hires_encoders)}
    };
    let backlash_body = view! {
        {bool_row(r, "/scope/BacklashCompEnabled", |t| t.guider_blc_enabled)}
        {num_row(r, "/scope/DecBacklashPulse", |t| t.guider_blc_pulse, 20.0, 8000.0, 1.0)}
        {num_row(r, "/scope/DecBacklashFloor", |t| t.guider_blc_floor, 20.0, 8000.0, 1.0)}
        {num_row(r, "/scope/DecBacklashCeiling", |t| t.guider_blc_ceiling, 20.0, 8000.0, 1.0)}
    };
    let cal_body = view! {
        {num_row(r, KEY_CAL_DURATION, |t| t.guider_cal_step, 0.0, 10000.0, 1.0)}
        {num_row(r, KEY_CAL_DISTANCE, |t| t.guider_cal_distance, 10.0, 200.0, 1.0)}
    };
    let title = |f: fn(&Translations) -> &'static str| move || f(t(lang.get()));

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 flex flex-col gap-3">
            {move || err.get().map(|m| view! { <div class="text-sm text-state-err">{m}</div> })}
            <div class="grid gap-3 md:grid-cols-2 items-start">
                <div class="flex flex-col gap-3">
                    {axis_block(AXES[0])}
                    {axis_block(AXES[1])}
                    <Show when=move || ao.get()>
                        {axis_block(AXES[2])}
                        {axis_block(AXES[3])}
                    </Show>
                </div>
                <div class="flex flex-col gap-3">
                    {section(title(|t| t.guider_mount_title), true, mount_body)}
                    {section(title(|t| t.guider_backlash_title), false, backlash_body)}
                    {section(title(|t| t.guider_calibration_title), false, cal_body)}
                </div>
            </div>
        </div>
        <div class="shrink-0 flex items-center gap-3 px-4 pt-2 pb-[max(0.5rem,env(safe-area-inset-bottom))] border-t border-border-base">
            <span class="flex-1 min-w-0 truncate text-xs text-text-muted">
                {move || scale.get().map_or_else(
                    || tr().guider_scale_unknown.to_string(),
                    |k| format!("{} {:.2} px", tr().guider_smart_min_move_hint, (0.1515 + 0.1548 / k).max(0.15)),
                )}
            </span>
            <button class="btn btn-ghost h-11 px-4" disabled=move || scale.with(Option::is_none) on:click=smart>
                {move || tr().guider_smart_min_move}
            </button>
        </div>
    }
}
