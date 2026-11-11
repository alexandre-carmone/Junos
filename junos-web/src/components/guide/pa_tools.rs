//! Polar tools sheet, three sub-tabs. Drift: PHD2's Polar Drift Alignment
//! (`polardrift_toolwin.cpp`) over `pa_drift_open/start/stop/close` and
//! `guider_pa.drift` — the hemisphere and mirror are its profile keys,
//! read when the tool opens. Static (`staticpa_toolwin.cpp`) over
//! `pa_static_*` and `guider_pa.static`: auto (Rotate / Stop) or manual
//! (hour angle, reference star, flip; the three positions), hemisphere,
//! the rotation and the alt / az adjustment. Drift align
//! (`drift_tool.cpp`) over `drift_align_*` and `guider_pa.drift_align`:
//! the phase's instructions, Slew to the meridian offset / Dec fields,
//! Drift, Adjust, the other phase, the trend's PA error and the circle.

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHECK, CHIP, LABEL, NUM, ROW, SELECT};
use crate::i18n::{t, Lang};

use super::protocol::{call_error, Reply, Rpc};

const KEY_HEMISPHERE: &str = "/PolarDriftTool/Hemisphere";
const KEY_MIRROR: &str = "/PolarDriftTool/Mirror";

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Drift,
    Static,
    DriftAlign,
}

/// A number tile: label over value.
fn tile(label: impl Fn() -> &'static str + Send + Sync + 'static, value: impl Fn() -> String + Send + Sync + 'static) -> impl IntoView {
    view! {
        <div class="panel p-2 flex flex-col min-w-0">
            <span class="text-[11px] text-text-muted truncate">{move || label()}</span>
            <span class="font-mono text-base text-text-blue-bright">{move || value()}</span>
        </div>
    }
}

pub(super) fn pa_tools_sheet(pa: RwSignal<Value>, settings: RwSignal<Value>, rpc: Rpc, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());
    let rpc = StoredValue::new(rpc);
    let last = RwSignal::new(None::<Reply>);
    let call = move |method: &'static str, params: Value| last.set(Some(rpc.with_value(|r| r.call(method, params))));
    let tab = RwSignal::new(Tab::Drift);

    let drift = Memo::new(move |_| pa.with(|p| p["drift"].clone()));
    let open = move || drift.with(|d| !d.is_null());
    let drifting = move || drift.with(|d| d["drifting"].as_bool().unwrap_or(false));
    let num = move |k: &str| drift.with(|d| d["result"][k].as_f64());
    let fmt = move |k: &'static str, digits: usize, unit: &'static str| {
        move || num(k).map_or("\u{2014}".to_string(), |v| format!("{v:.digits$}{unit}"))
    };
    let key = move |k: &str| settings.with(|s| s[k].as_i64().unwrap_or(1));
    let north = move || key(KEY_HEMISPHERE) >= 0;
    let set_key = move |k: &'static str, v: i64| call("set_profile_value", json!({"key": k, "value": v}));

    let tab_chip = move |t: Tab, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if tab.get() == t { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| tab.set(t)>{move || label(tr())}</button>
    };
    let hemi_chip = move |n: bool, label: fn(&'static crate::i18n::Translations) -> &'static str| view! {
        <button class=move || if north() == n { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                disabled=open on:click=move |_| set_key(KEY_HEMISPHERE, if n { 1 } else { -1 })>
            {move || label(tr())}
        </button>
    };
    let button = move |label: fn(&'static crate::i18n::Translations) -> &'static str, class: &'static str, method: &'static str| view! {
        <button class=class on:click=move |_| call(method, json!({}))>{move || label(tr())}</button>
    };
    const GO: &str = "btn btn-primary h-11 px-5 font-semibold";
    const STOP: &str = "btn btn-danger h-11 px-5 font-semibold";
    const MORE: &str = "btn btn-ghost h-11 px-4";

    let drift_tab = move || view! {
        <div class=CARD>
            <div class="text-sm text-text whitespace-pre-line">{move || tr().guider_pa_drift_instructions}</div>
            <div class=ROW>
                <span class=format!("{LABEL} flex-1")>{move || tr().guider_pa_hemisphere}</span>
                {hemi_chip(true, |t| t.guider_pa_north)}
                {hemi_chip(false, |t| t.guider_pa_south)}
            </div>
            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                <input type="checkbox" class=CHECK disabled=open prop:checked=move || key(KEY_MIRROR) < 0
                       on:change=move |ev| set_key(KEY_MIRROR, if crate::dom::event_target_checked(&ev) { -1 } else { 1 }) />
                <span class=LABEL>{move || tr().guider_pa_mirror}</span>
            </label>
            <Show when=open>
                <span class="text-xs text-text-muted">{move || tr().guider_pa_applies_on_open}</span>
            </Show>
            <div class="grid grid-cols-2 md:grid-cols-4 gap-2">
                {tile(move || tr().guider_pa_elapsed, fmt("time", 0, " s"))}
                {tile(move || tr().guider_pa_alpha, fmt("angle_deg", 1, "\u{00b0}"))}
                {tile(move || tr().guider_pa_offset, fmt("offset_px", 1, " px"))}
                {tile(move || tr().guider_pa_error, fmt("pa_error_arcmin", 1, "\u{2032}"))}
            </div>
            <div class="text-xs text-text-muted font-mono">
                {move || drift.with(|d| d["status"].as_array().map(|a| {
                    a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{00b7} ")
                }).unwrap_or_default())}
            </div>
            <div class="flex gap-2 flex-wrap">
                <Show when=open fallback=move || button(|t| t.guider_pa_open, GO, "pa_drift_open")>
                    <Show when=drifting fallback=move || button(|t| t.guider_ga_start, GO, "pa_drift_start")>
                        {button(|t| t.guider_btn_stop, STOP, "pa_drift_stop")}
                    </Show>
                    {button(|t| t.guider_pa_close, MORE, "pa_drift_close")}
                </Show>
            </div>
        </div>
    };
    let pick = move |on: bool, label: fn(&'static crate::i18n::Translations) -> &'static str, act: Box<dyn Fn() + Send + Sync>| view! {
        <button class=if on { format!("{CHIP} btn--active") } else { CHIP.to_string() } on:click=move |_| act()>
            {move || label(tr())}
        </button>
    };
    let number = move |label: fn(&'static crate::i18n::Translations) -> &'static str, v: RwSignal<String>, send: Box<dyn Fn() + Send + Sync>| view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label(tr())}</span>
            <input type="number" inputmode="decimal" step="any" class=NUM prop:value=move || v.get()
                   on:input=move |ev| v.set(crate::dom::event_target_value(&ev)) on:change=move |_| send() />
        </div>
    };
    let parsed = |v: RwSignal<String>| v.with_untracked(|s| s.trim().parse::<f64>().ok());

    // Static: PHD2's window over `guider_pa.static`.
    let st = Memo::new(move |_| pa.with(|p| p["static"].clone()));
    let st_open = move || st.with(|s| !s.is_null());
    let st_flag = move |k: &'static str| st.with(|s| s[k].as_bool().unwrap_or(false));
    let st_num = move |k: &'static str, unit: &'static str| {
        move || st.with(|s| s["adjustment"][k].as_f64()).map_or("\u{2014}".to_string(), |v| format!("{v:.1}{unit}"))
    };
    let ha = RwSignal::new(String::new());
    let st_ha = Memo::new(move |_| st.with(|s| s["hour_angle"].as_f64()));
    Effect::new(move |_| ha.set(st_ha.get().map(|h| format!("{h:.2}")).unwrap_or_default()));
    let st_set = move |p: Value| call("pa_static_set", p);
    // The settings block redraws only when these change, not per frame.
    let st_auto = Memo::new(move |_| st_flag("auto"));
    let st_north = Memo::new(move |_| st.with(|s| s["hemisphere"].as_i64()) != Some(-1));
    let st_aligning = Memo::new(move |_| st_flag("aligning"));
    let st_can_slew = Memo::new(move |_| st_flag("can_slew"));
    let st_stars = Memo::new(move |_| st.with(|s| (s["stars"].clone(), s["ref_star"].as_u64())));
    let static_tab = move || view! {
        <div class=CARD>
            <Show when=st_open fallback=move || view! {
                <div class="text-sm text-text">{move || tr().guider_pa_static_hint}</div>
                <div>{button(|t| t.guider_pa_open, GO, "pa_static_start")}</div>
            }>
                {move || {
                    let (auto, north, aligning) = (st_auto.get(), st_north.get(), st_aligning.get());
                    view! {
                        <div class="flex gap-1.5 flex-wrap" class:pointer-events-none=aligning class:opacity-50=aligning>
                            {st_can_slew.get().then(|| view! {
                                {pick(auto, |t| t.guider_pa_auto, Box::new(move || st_set(json!({"manual": false}))))}
                                {pick(!auto, |t| t.guider_pa_manual, Box::new(move || st_set(json!({"manual": true}))))}
                            })}
                            {pick(north, |t| t.guider_pa_north, Box::new(move || st_set(json!({"north": true}))))}
                            {pick(!north, |t| t.guider_pa_south, Box::new(move || st_set(json!({"north": false}))))}
                        </div>
                        {(!auto).then(|| view! {
                            {number(|t| t.guider_hour_angle, ha, Box::new(move || {
                                if let Some(h) = parsed(ha) { st_set(json!({"hour_angle": h.rem_euclid(24.0)})) }
                            }))}
                            <div class=ROW>
                                <span class=format!("{LABEL} flex-1")>{move || tr().guider_pa_ref_star}</span>
                                <select class=SELECT on:change=move |ev| {
                                    if let Ok(i) = crate::dom::event_target_value(&ev).parse::<u64>() { st_set(json!({"ref_star": i})) }
                                }>
                                    {st_stars.with(|(stars, cur)| {
                                        let cur = *cur;
                                        stars.as_array().into_iter().flatten().enumerate().map(|(i, star)| view! {
                                            <option value=i.to_string() selected=cur == Some(i as u64)>
                                                {format!("{} ({:.1})", star["name"].as_str().unwrap_or("?"), star["mag"].as_f64().unwrap_or(0.0))}
                                            </option>
                                        }).collect_view()
                                    })}
                                </select>
                            </div>
                            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                                <input type="checkbox" class=CHECK prop:checked=move || st_flag("flip")
                                       on:change=move |ev| st_set(json!({"flip": crate::dom::event_target_checked(&ev)})) />
                                <span class=LABEL>{move || tr().guider_pa_flip}</span>
                            </label>
                        })}
                    }
                }}
                <div class="grid grid-cols-2 md:grid-cols-4 gap-2">
                    {tile(move || tr().guider_pa_rotation, move || st.with(|s| {
                        let r = &s["rotation"];
                        format!("{:.0}/{:.0}\u{00b0}", r["total_deg"].as_f64().unwrap_or(0.0), r["req_deg"].as_f64().unwrap_or(0.0))
                    }))}
                    {tile(move || tr().guider_pa_alt, st_num("alt_arcmin", "\u{2032}"))}
                    {tile(move || tr().guider_pa_az, st_num("az_arcmin", "\u{2032}"))}
                    {tile(move || tr().guider_pa_error, st_num("total_arcmin", "\u{2032}"))}
                </div>
                <div class="text-sm text-text-blue break-words">{move || st.with(|s| s["status"].as_str().unwrap_or("").to_string())}</div>
                <div class="flex gap-2 flex-wrap">
                    <Show when=move || st_flag("auto") fallback=move || view! {
                        {button(|t| t.guider_pa_pos1, GO, "pa_static_rotate")}
                        <button class=MORE on:click=move |_| call("pa_static_mark", json!({"position": 2}))>{move || tr().guider_pa_pos2}</button>
                        <button class=MORE on:click=move |_| call("pa_static_mark", json!({"position": 3}))>{move || tr().guider_pa_pos3}</button>
                    }>
                        <button class=move || if st_flag("aligning") { STOP } else { GO } on:click=move |_| call("pa_static_rotate", json!({}))>
                            {move || if st_flag("aligning") { tr().guider_btn_stop } else { tr().guider_pa_rotate }}
                        </button>
                    </Show>
                    {button(|t| t.guider_clear, MORE, "pa_static_clear")}
                    {button(|t| t.guider_pa_close, MORE, "pa_static_stop")}
                </div>
            </Show>
        </div>
    };

    // Drift align: PHD2's Drift Align tool over `guider_pa.drift_align`.
    let da = Memo::new(move |_| pa.with(|p| p["drift_align"].clone()));
    let da_open = move || da.with(|d| !d.is_null());
    let da_text = move |k: &'static str| move || da.with(|d| d[k].as_str().unwrap_or("").to_string());
    let mode = move || da.with(|d| d["mode"].as_str().unwrap_or("").to_string());
    let alt_phase = move || da.with(|d| d["phase"] == "Alt");
    let (s_ra, s_dec) = (RwSignal::new(String::new()), RwSignal::new(String::new()));
    let slew = Memo::new(move |_| da.with(|d| d["slew"].clone()));
    Effect::new(move |_| {
        let s = slew.get();
        s_ra.set(s["ra"].as_f64().map(|v| format!("{v:.0}")).unwrap_or_default());
        s_dec.set(s["dec"].as_f64().map(|v| format!("{v:.0}")).unwrap_or_default());
    });
    let mode_button = move |m: &'static str, label: fn(&'static crate::i18n::Translations) -> &'static str, method: &'static str| view! {
        <button class=move || if mode() == m { format!("{MORE} btn--active") } else { MORE.to_string() }
                on:click=move |_| call(method, json!({}))>{move || label(tr())}</button>
    };
    let align_tab = move || view! {
        <div class=CARD>
            <Show when=da_open fallback=move || view! {
                <div class="text-sm text-text whitespace-pre-line">{move || tr().guider_pa_drift_align_hint}</div>
                <div>{button(|t| t.guider_pa_open, GO, "drift_align_start")}</div>
            }>
                <span class=CARD_TITLE>{da_text("title")}</span>
                <div class="text-sm text-text whitespace-pre-line">{da_text("instructions")}</div>
                <div class="text-xs text-text-muted font-mono">
                    {move || da.with(|d| {
                        let c = &d["current"];
                        format!("{}\u{00b0} \u{00b7} {} {}\u{00b0}", c["ra"].as_str().unwrap_or("?"), tr().dec_label, c["dec"].as_str().unwrap_or("?"))
                    })}
                </div>
                {number(|t| t.guider_ca_offset, s_ra, Box::new(|| ()))}
                {number(|t| t.guider_cal_declination, s_dec, Box::new(|| ()))}
                <div class="grid grid-cols-2 gap-2">
                    {tile(move || tr().guider_pa_error, move || da.with(|d| d["trend"]["error_arcmin"].as_f64())
                        .map_or("\u{2014}".to_string(), |v| format!("{v:.1}\u{2032}")))}
                    {tile(move || tr().guider_pa_circle, move || da.with(|d| d["circle"]["error_px"].as_f64())
                        .map_or("\u{2014}".to_string(), |v| format!("{v:.1} px")))}
                </div>
                <div class="text-sm text-text-blue break-words">{da_text("status")}</div>
                <div class="flex gap-2 flex-wrap">
                    <button class=MORE disabled=move || !da.with(|d| d["can_slew"].as_bool().unwrap_or(false))
                            on:click=move |_| call("drift_align_slew", json!({"ra": parsed(s_ra), "dec": parsed(s_dec)}))>
                        {move || tr().guider_slew}
                    </button>
                    {mode_button("Drift", |t| t.guider_pa_drift, "drift_align_drift")}
                    {mode_button("Adjust", |t| t.guider_pa_adjust, "drift_align_adjust")}
                    <button class=MORE on:click=move |_| call("drift_align_phase", json!({}))>
                        {move || if alt_phase() { tr().guider_pa_az } else { tr().guider_pa_alt }}
                    </button>
                    {button(|t| t.guider_pa_close, MORE, "drift_align_stop")}
                </div>
            </Show>
        </div>
    };

    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] \
                    flex flex-col gap-3">
            <div class="flex gap-1.5 flex-wrap">
                <span class=format!("{CARD_TITLE} self-center mr-auto")>{move || tr().guider_pa_tool}</span>
                {tab_chip(Tab::Drift, |t| t.guider_pa_tab_drift)}
                {tab_chip(Tab::Static, |t| t.guider_pa_tab_static)}
                {tab_chip(Tab::DriftAlign, |t| t.guider_pa_tab_drift_align)}
            </div>
            {move || match tab.get() {
                Tab::Drift => drift_tab().into_any(),
                Tab::Static => static_tab().into_any(),
                Tab::DriftAlign => align_tab().into_any(),
            }}
            {move || call_error(last).map(|e| view! { <div class="text-sm text-state-err break-words">{e}</div> })}
        </div>
    }
}
