//! Darks and bad-pixel map sheet: PHD2's Dark Library and Bad-pixel Map
//! dialogs (`darks_dialog.cpp`, `Refine_DefMap.cpp`) over `guider_darks`.
//! What is loaded and the two Use toggles (`darks_use`, PHD2's menu items:
//! loading one unloads the other), a dark library build (`darks_build`, one
//! set of frames per exposure between min and max), and the defect map: its
//! master dark (`bpm_build`), then the hot / cold aggressiveness
//! (`bpm_set`, sent on release: its reply counts the pixels and gives the
//! master dark's stats) and `bpm_save` at those values. A build asks to cover the
//! scope first (a second tap), as PHD2's "Cover guide scope" box.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHECK, LABEL, NUM, ROW, SELECT};
use crate::i18n::{t, Lang};
use crate::ws::GuiderStatusData;

use super::protocol::{call_error, Reply, Rpc};

/// PHD2's exposure label (`ExposureDurationLabel`, myframe.cpp:779).
pub(super) fn exp_label(ms: i64) -> String {
    match ms {
        ms if ms >= 10_000 => format!("{} s", ms / 1000),
        ms if ms < 100 => format!("{:.2} s", ms as f64 / 1000.0),
        ms => format!("{:.1} s", ms as f64 / 1000.0),
    }
}

/// A checkbox row whose state comes from the daemon's echo.
fn use_row(label: impl Fn() -> &'static str + Send + Sync + 'static, on: Signal<bool>, set: impl Fn(bool) + 'static) -> impl IntoView {
    view! {
        <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
            <input type="checkbox" class=CHECK prop:checked=move || on.get()
                   on:change=move |ev| set(crate::dom::event_target_checked(&ev)) />
            <span class=LABEL>{move || label()}</span>
        </label>
    }
}

/// A whole-number input clamped to `lo..=hi`.
fn int_row(label: impl Fn() -> &'static str + Send + Sync + 'static, v: RwSignal<i64>, lo: i64, hi: i64) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            <input type="number" min=lo.to_string() max=hi.to_string() step="1" class=NUM prop:value=move || v.get().to_string()
                   on:change=move |ev| {
                       if let Ok(x) = crate::dom::event_target_value(&ev).parse::<i64>() {
                           v.set(x.clamp(lo, hi));
                       }
                   } />
        </div>
    }
}

/// An aggressiveness slider 0–100: moves `v`, sends on release.
fn aggr_row(label: impl Fn() -> &'static str + Send + Sync + 'static, v: RwSignal<i64>, on_release: impl Fn() + 'static) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} w-24 shrink-0")>{move || label()}</span>
            <input type="range" min="0" max="100" step="1" class="flex-1 min-w-0 accent-accent-cyan"
                   prop:value=move || v.get().to_string()
                   on:input=move |ev| v.set(crate::dom::event_target_value(&ev).parse().unwrap_or(75))
                   on:change=move |_| on_release() />
            <span class="w-9 text-right font-mono text-sm">{move || v.get()}</span>
        </div>
    }
}

pub(super) fn darks_sheet(
    state: RwSignal<Option<GuiderStatusData>>,
    darks: RwSignal<Value>,
    settings: RwSignal<Value>,
    rpc: Rpc,
    lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call_r = move |method: &'static str, params: Value| {
        let r = rpc.with_value(|r| r.call(method, params));
        last.set(Some(r));
        r
    };
    let call = move |method: &'static str, params: Value| {
        call_r(method, params);
    };

    let exposures = Memo::new(move |_| {
        state.with(|s| s.as_ref().map(|s| s.exposure_options.iter().copied().filter(|&ms| ms > 0).collect::<Vec<_>>()))
            .unwrap_or_default()
    });
    // PHD2's defaults: 1–6 s, 0.1–2.5 s with an AO (`MinExposureDefault`).
    let ao = state.with_untracked(|s| s.as_ref().is_some_and(|s| s.equipment.ao.as_ref().is_some_and(|a| a.connected)));
    let min_ms = RwSignal::new(if ao { 100 } else { 1000 });
    let max_ms = RwSignal::new(if ao { 2500 } else { 6000 });
    let count = RwSignal::new(5_i64);
    let new_library = RwSignal::new(false);
    let dm_exp_s = RwSignal::new(15_i64);
    let dm_count = RwSignal::new(25_i64);
    // The refine dialog starts from the last saved map's (DefDMSigmaX 75).
    let factor = |k: &str| settings.with_untracked(|s| s[k].as_i64()).unwrap_or(75);
    let hot = RwSignal::new(factor("/camera/dmap_hot_factor"));
    let cold = RwSignal::new(factor("/camera/dmap_cold_factor"));
    // `bpm_set`'s reply: the pixel counts and the master dark's stats.
    let refine = RwSignal::new(None::<Reply>);
    // The build waiting for its second tap ("cover the scope"): defect map or not.
    let armed = RwSignal::new(None::<bool>);

    let dark_loaded = Signal::derive(move || darks.with(|d| d["dark"]["loaded"].as_bool().unwrap_or(false)));
    let defect_loaded = Signal::derive(move || darks.with(|d| d["defect_map"]["loaded"].as_bool().unwrap_or(false)));
    let library = move || {
        let lib: Vec<i64> = darks.with(|d| {
            d["dark"]["exposures"].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default()
        });
        match (lib.iter().min(), lib.iter().max()) {
            (Some(&lo), Some(&hi)) => {
                tr().guider_darks_library_covers.replace("{n}", &lib.len().to_string())
                    .replace("{min}", &exp_label(lo)).replace("{max}", &exp_label(hi))
            }
            _ => tr().guider_darks_no_library.to_string(),
        }
    };
    let defect_counts = move || {
        darks.with(|d| match (d["defect_map"]["hot"].as_i64(), d["defect_map"]["cold"].as_i64()) {
            (Some(h), Some(c)) => format!("{} {h} \u{00b7} {} {c}", tr().guider_bpm_hot, tr().guider_bpm_cold),
            _ => String::new(),
        })
    };
    let build = Memo::new(move |_| darks.with(|d| d["build"].clone()));
    let building = move || build.with(|b| !b.is_null());

    let exp_select = move |v: RwSignal<i64>| {
        view! {
            <select class=SELECT on:change=move |ev| v.set(crate::dom::event_target_value(&ev).parse().unwrap_or(1000))>
                {move || exposures.get().into_iter().map(|ms| view! {
                    <option value=ms.to_string() prop:selected=move || v.get() == ms>{exp_label(ms)}</option>
                }).collect_view()}
            </select>
        }
    };
    let start = move |defect: bool| {
        if armed.get_untracked() != Some(defect) {
            armed.set(Some(defect));
            return;
        }
        armed.set(None);
        if defect {
            call("bpm_build", json!({"exposure_ms": dm_exp_s.get_untracked() * 1000, "count": dm_count.get_untracked()}));
        } else {
            let (lo, hi) = (min_ms.get_untracked(), max_ms.get_untracked());
            call("darks_build", json!({"min_ms": lo.min(hi), "max_ms": lo.max(hi), "count": count.get_untracked(),
                                      "new_library": new_library.get_untracked()}));
        }
    };
    let build_button = move |defect: bool| {
        let label = move || {
            let tr = tr();
            match (armed.get() == Some(defect), defect) {
                (true, _) => tr.guider_darks_cover_scope,
                (false, true) => tr.guider_bpm_build,
                (false, false) => tr.guider_darks_build,
            }
        };
        view! {
            <button class=move || if armed.get() == Some(defect) { "btn btn-danger h-11 px-4" } else { "btn btn-primary h-11 px-4" }
                    disabled=building on:click=move |_| start(defect)>
                {label}
            </button>
        }
    };
    let aggr = move || json!({"hot": hot.get_untracked(), "cold": cold.get_untracked()});
    let set_aggr = move || refine.set(Some(call_r("bpm_set", aggr())));
    let refined = move || {
        let v = refine.get().and_then(|r| r.get()).and_then(Result::ok)?;
        let n = |k: &str| v[k].as_f64().unwrap_or(0.0);
        let tr = tr();
        Some(view! {
            <span class="text-sm text-text-blue">
                {format!("{} {} \u{00b7} {} {}", tr.guider_bpm_hot, n("hot"), tr.guider_bpm_cold, n("cold"))}
            </span>
            <span class="text-xs text-text-muted font-mono break-words">
                {tr.guider_bpm_stats.replace("{exp}", &exp_label(n("exposure_ms") as i64)).replace("{n}", &n("frames").to_string())
                    .replace("{mean}", &format!("{:.0}", n("mean"))).replace("{sd}", &format!("{:.1}", n("stdev")))
                    .replace("{med}", &format!("{:.0}", n("median"))).replace("{mad}", &format!("{:.0}", n("mad")))}
            </span>
        })
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    grid grid-cols-1 md:grid-cols-2 gap-3 items-start">
            <div class=format!("{CARD} md:col-span-2")>
                <span class=CARD_TITLE>{move || tr().guider_darks_status}</span>
                <div class="text-sm text-text-muted">{library}</div>
                {use_row(move || tr().guider_darks_use, dark_loaded, move |on| call("darks_use", json!({"dark": on})))}
                <div class="text-sm text-text-muted">
                    {move || if defect_loaded.get() { tr().guider_bpm_loaded } else { tr().guider_bpm_none }}
                    " " {defect_counts}
                </div>
                {use_row(move || tr().guider_bpm_use, defect_loaded, move |on| call("darks_use", json!({"defect": on})))}
                {move || (!build.with(Value::is_null)).then(|| {
                    let (done, total) = build.with(|b| (b["progress"][0].as_f64().unwrap_or(0.0), b["progress"][1].as_f64().unwrap_or(0.0)));
                    let pct = if total > 0.0 { (100.0 * done / total).clamp(0.0, 100.0) } else { 0.0 };
                    view! {
                        <div class=ROW>
                            <span class="flex-1 min-w-0 text-sm text-text-blue break-words">
                                {build.with(|b| b["status"].as_str().unwrap_or("").to_string())}
                            </span>
                            <button class="btn btn-danger h-9 px-4" on:click=move |_| call("darks_stop", json!({}))>
                                {move || tr().guider_btn_stop}
                            </button>
                        </div>
                        <div class="h-1.5 rounded bg-bg-elev-2 overflow-hidden">
                            <div class="h-full bg-accent-cyan" style=format!("width:{pct:.0}%")></div>
                        </div>
                    }
                })}
            </div>

            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().guider_darks_library}</span>
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || tr().guider_darks_min_exp}</span>
                    {exp_select(min_ms)}
                </div>
                <div class=ROW>
                    <span class=format!("{LABEL} flex-1")>{move || tr().guider_darks_max_exp}</span>
                    {exp_select(max_ms)}
                </div>
                {int_row(move || tr().guider_darks_count, count, 1, 20)}
                <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                    <input type="checkbox" class=CHECK prop:checked=move || new_library.get()
                           on:change=move |ev| new_library.set(crate::dom::event_target_checked(&ev)) />
                    <span class=LABEL>{move || tr().guider_darks_new_library}</span>
                </label>
                {build_button(false)}
            </div>

            <div class=CARD>
                <span class=CARD_TITLE>{move || tr().guider_bpm_title}</span>
                {int_row(move || tr().guider_bpm_exposure, dm_exp_s, 5, 15)}
                {int_row(move || tr().guider_bpm_count, dm_count, 5, 25)}
                {build_button(true)}
                <span class="text-xs text-text-muted">{move || tr().guider_bpm_aggressiveness}</span>
                {aggr_row(move || tr().guider_bpm_hot, hot, set_aggr)}
                {aggr_row(move || tr().guider_bpm_cold, cold, set_aggr)}
                {refined}
                <button class="btn btn-ghost h-11 px-4" on:click=move |_| call("bpm_save", aggr())>
                    {move || tr().guider_bpm_save}
                </button>
            </div>

            {move || call_error(last).map(|e| view! {
                <div class="md:col-span-2 text-sm text-state-err break-words">{e}</div>
            })}
        </div>
    }
}
