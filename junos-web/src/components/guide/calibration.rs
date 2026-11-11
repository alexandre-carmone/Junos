//! The native Guide tab's Calibration sheet: PHD2's calibration review
//! (`calreview_dialog.cpp`). For the mount or the AO, a plot of the
//! calibration steps — RA (cyan) and Dec (amber) from `guider_calibration`,
//! the live `Calibrating` steps while it runs — with the fitted axes, and
//! the numbers: angles, rates in px/s and ″/s, parities, orthogonality
//! error, declination, pier side, binning, when, and the last issue. Then
//! Flip, Clear (a second tap; mount, AO or both) and Recalibrate (`guide`
//! with `recalibrate`), PHD2's calibration step calculator
//! (`calstep_dialog.cpp`, as `engine/calibration.rs::calibration_step_size`)
//! whose Apply writes the step and the distance, and the manual
//! calibration form (`manualcal_dialog.cpp`, `set_calibration_data`), and
//! the Calibration Assistant (`cal_assistant.rs`).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{CARD, CARD_TITLE, CHIP, LABEL, ROW};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};
use crate::ws::{GuiderCalStep, GuiderStatusData};

use super::cal_assistant::cal_assistant_card;
use super::protocol::{second_tap, Reply, Rpc, Settle, CALIBRATING, GUIDING, KEY_CAL_DISTANCE, KEY_CAL_DURATION, KEY_FOCAL_LENGTH, KEY_PIXEL_SIZE, LOST_LOCK};
use super::rows::section;

const NUM: &str = "input input--sm font-mono w-[104px] shrink-0 text-right max-md:h-9";

/// What the sheet reads and drives.
#[derive(Clone)]
pub struct CalSheet {
    pub state: RwSignal<Option<GuiderStatusData>>,
    pub calibration: RwSignal<Value>,
    pub cal_steps: RwSignal<Vec<GuiderCalStep>>,
    pub settings: RwSignal<Value>,
    /// `guider_tools`, for the assistant card.
    pub tools: RwSignal<Value>,
    pub rpc: Rpc,
    pub settle: Settle,
    pub lang: RwSignal<Lang>,
}

/// `calibration_step_size`: the image scale (″/px) and the step (ms) that
/// moves `distance` px in `steps` steps at `speed` × sidereal, at most
/// 6 steps' worth, longer by 1/cos(dec), rounded up to 50 ms.
fn calstep(focal: f64, px: f64, bin: f64, speed: f64, steps: f64, dec: f64, distance: f64) -> (f64, i64) {
    let scale = px / focal * 206.265 * bin;
    let total = distance * scale / (15.0 * speed);
    let pulse = total / steps * 1000.0;
    let max_pulse = total / 6.0 * 1000.0;
    let pulse = max_pulse.min(pulse / dec.abs().to_radians().cos());
    (scale, (pulse / 50.0).ceil() as i64 * 50)
}

fn steps_of(v: &Value) -> Vec<(f64, f64)> {
    v.as_array().into_iter().flatten().filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?))).collect()
}

/// The step plot: RA cyan, Dec amber (dashed back), backlash grey, the
/// origin cross and the fitted axes.
fn plot(ra: &[(f64, f64)], dec: &[(f64, f64)], other: &[(f64, f64)], angles: Option<(f64, f64)>) -> impl IntoView + use<> {
    let r = ra.iter().chain(dec).chain(other).fold(5.0_f64, |m, (x, y)| m.max(x.abs()).max(y.abs())) * 1.1;
    let k = 90.0 / r;
    let path = |pts: &[(f64, f64)]| {
        let mut d = String::from("M100 100");
        for (x, y) in pts {
            d.push_str(&format!(" L{:.1} {:.1}", 100.0 + x * k, 100.0 + y * k));
        }
        d
    };
    let dots = |pts: &[(f64, f64)], color: &'static str| {
        pts.iter()
            .map(|(x, y)| view! { <circle cx=format!("{:.1}", 100.0 + x * k) cy=format!("{:.1}", 100.0 + y * k) r="2" fill=color /> })
            .collect_view()
    };
    let axis = |a: f64, color: &'static str| {
        let (x, y) = (100.0 + 92.0 * a.to_radians().cos(), 100.0 + 92.0 * a.to_radians().sin());
        view! { <line x1="100" y1="100" x2=format!("{x:.1}") y2=format!("{y:.1}") stroke=color stroke-width="0.8" stroke-dasharray="3 3" /> }
    };
    view! {
        <svg viewBox="0 0 200 200" class="w-full max-w-[320px] mx-auto aspect-square bg-bg-input-deep rounded-lg border border-border-base">
            <line x1="92" y1="100" x2="108" y2="100" stroke="var(--text-faint)" stroke-width="0.8" />
            <line x1="100" y1="92" x2="100" y2="108" stroke="var(--text-faint)" stroke-width="0.8" />
            {angles.map(|(xa, ya)| view! { {axis(xa, "var(--accent-cyan)")} {axis(ya, "var(--state-warn)")} })}
            <path d=path(other) fill="none" stroke="var(--text-faint)" stroke-width="1" />
            <path d=path(ra) fill="none" stroke="var(--accent-cyan)" stroke-width="1.2" />
            <path d=path(dec) fill="none" stroke="var(--state-warn)" stroke-width="1.2" />
            {dots(ra, "var(--accent-cyan)")}
            {dots(dec, "var(--state-warn)")}
        </svg>
    }
}

/// A local number field, kept as typed.
fn field(label: impl Fn() -> &'static str + Send + 'static, v: RwSignal<String>) -> impl IntoView {
    view! {
        <div class=ROW>
            <span class=format!("{LABEL} flex-1")>{move || label()}</span>
            <input type="number" inputmode="decimal" step="any" class=NUM
                   prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) />
        </div>
    }
}

fn parse(v: RwSignal<String>) -> Option<f64> {
    v.with(|s| s.trim().parse::<f64>().ok()).filter(|x| x.is_finite())
}

/// The sheet's body.
pub fn calibration_sheet(c: CalSheet) -> impl IntoView {
    let lang = c.lang;
    let tr = move || t(lang.get());
    let (state, calibration, cal_steps, settings, rpc, settle) = (c.state, c.calibration, c.cal_steps, c.settings, c.rpc.clone(), c.settle);
    let err = RwSignal::new(None::<String>);
    let calls = RwSignal::new(Vec::<Reply>::new());
    Effect::new(move |_| {
        for r in calls.get() {
            if let Some(Err(e)) = r.get() {
                err.set(Some(e.message));
            }
        }
    });
    let call = move |r: Reply| {
        err.set(None);
        calls.update(|c| c.push(r));
    };

    let app = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.app_state.clone()).unwrap_or_default()));
    let busy = Memo::new(move |_| matches!(app.get().as_str(), CALIBRATING | GUIDING | LOST_LOCK));
    let has_ao = Memo::new(move |_| state.with(|s| s.as_ref().is_some_and(|s| s.equipment.ao.is_some())));
    let which = RwSignal::new("mount");
    let cal = Memo::new(move |_| calibration.with(|c| c[which.get()].clone()));
    let scale = Memo::new(move |_| state.with(|s| s.as_ref().and_then(|s| s.pixel_scale)));

    let review = move || {
        let tr = tr();
        let live = app.get() == CALIBRATING;
        let (ra, dec, other, angles) = if live {
            cal_steps.with(|s| {
                let pick = |d: &[&str]| s.iter().filter(|c| d.contains(&c.dir.as_str())).map(|c| (c.dx, c.dy)).collect::<Vec<_>>();
                (pick(&["West", "East", "Left", "Right"]), pick(&["North", "South", "Up", "Down"]), pick(&["Backlash"]), None)
            })
        } else {
            cal.with(|c| {
                let angles = c["x_angle_deg"].as_f64().zip(c["y_angle_deg"].as_f64());
                (steps_of(&c["ra_steps"]), steps_of(&c["dec_steps"]), Vec::new(), angles)
            })
        };
        let rows = cal.with(|c| {
            if c.is_null() {
                return Vec::new();
            }
            let f = |k: &str, d: usize| c[k].as_f64().map_or("\u{2014}".to_string(), |v| format!("{v:.d$}"));
            let k = c["image_scale"].as_f64().filter(|k| *k > 0.0).or(scale.get());
            let rate = |key: &str| {
                c[key].as_f64().map_or("\u{2014}".to_string(), |r| match k {
                    Some(k) => format!("{r:.2} px/s \u{00b7} {:.2}\u{2033}/s", r * k),
                    None => format!("{r:.2} px/s"),
                })
            };
            let s = |k: &str| c[k].as_str().unwrap_or("\u{2014}").to_string();
            let ortho = c["ortho_error"].as_f64();
            vec![
                (tr.guider_cal_ra_angle, format!("{}\u{00b0}", f("x_angle_deg", 1)), false),
                (tr.guider_cal_ra_rate, rate("x_rate"), false),
                (tr.guider_cal_ra_parity, s("x_parity"), false),
                (tr.guider_cal_dec_angle, format!("{}\u{00b0}", f("y_angle_deg", 1)), false),
                (tr.guider_cal_dec_rate, rate("y_rate"), false),
                (tr.guider_cal_dec_parity, s("y_parity"), false),
                (tr.guider_cal_ortho, ortho.map_or("\u{2014}".into(), |o| format!("{o:.1}\u{00b0}")), ortho.is_some_and(|o| o.abs() > 12.5)),
                (tr.guider_cal_declination, format!("{}\u{00b0}", f("declination", 1)), false),
                (tr.guider_pier_side, s("pier_side"), false),
                (tr.guider_cam_binning, c["binning"].as_i64().map_or("\u{2014}".into(), |b| b.to_string()), false),
                (tr.guider_focal_length, f("focal_length", 0), false),
                (tr.guider_cal_image_scale, format!("{}\u{2033}/px", f("image_scale", 2)), false),
                (tr.guider_cal_when, s("timestamp"), false),
                (tr.guider_cal_last_issue, s("last_issue"), c["last_issue"].as_str().is_some_and(|i| i != "None")),
            ]
        });
        let empty = rows.is_empty() && !live;
        view! {
            {(!empty).then(|| plot(&ra, &dec, &other, angles))}
            {empty.then(|| view! { <div class="text-sm text-text-faint text-center py-4">{tr.guider_cal_none}</div> })}
            <div class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
                {rows.into_iter().map(|(label, value, bad)| view! {
                    <span class="text-text-muted">{label}</span>
                    <span class=if bad { "font-mono text-state-err text-right" } else { "font-mono text-text-dim text-right" }>{value}</span>
                }).collect_view()}
            </div>
        }
    };

    // Clear: which calibration, on a second tap.
    let clear_which = RwSignal::new("mount");
    let armed = RwSignal::new(false);
    let (r_flip, r_clear, r_recal) = (rpc.clone(), rpc.clone(), rpc.clone());
    let chip = move |on: Signal<bool>, label: Signal<&'static str>, set: Box<dyn Fn() + Send + Sync>| view! {
        <button type="button" class=move || if on.get() { format!("{CHIP} btn--active") } else { CHIP.to_string() }
                on:click=move |_| set()>{move || label.get()}</button>
    };
    let lbl = move |f: fn(&Translations) -> &'static str| Signal::derive(move || f(t(lang.get())));
    let actions = view! {
        <div class="flex flex-wrap gap-1.5 items-center">
            <span class=format!("{LABEL} mr-1")>{move || tr().guider_cal_clear_which}</span>
            {chip(Signal::derive(move || clear_which.get() == "mount"), lbl(|t| t.guider_role_mount), Box::new(move || clear_which.set("mount")))}
            <Show when=move || has_ao.get()>
                {chip(Signal::derive(move || clear_which.get() == "ao"), lbl(|t| t.guider_role_ao), Box::new(move || clear_which.set("ao")))}
                {chip(Signal::derive(move || clear_which.get() == "both"), lbl(|t| t.guider_cal_both), Box::new(move || clear_which.set("both")))}
            </Show>
        </div>
        <div class="grid grid-cols-3 gap-2">
            <button class="btn btn-ghost h-11" disabled=move || busy.get()
                    on:click=move |_| call(r_flip.call("flip_calibration", json!({})))>
                {move || tr().guider_cal_flip}
            </button>
            <button class=move || if armed.get() { "btn btn-danger h-11" } else { "btn btn-ghost h-11" } disabled=move || busy.get()
                    on:click=move |_| if second_tap(armed) { call(r_clear.call("clear_calibration", json!({ "which": clear_which.get_untracked() }))) }>
                {move || if armed.get() { tr().guider_tap_to_confirm } else { tr().guider_clear }}
            </button>
            <button class="btn btn-primary h-11" disabled=move || busy.get()
                    on:click=move |_| call(r_recal.guide(&settle, true))>
                {move || tr().guider_cal_recalibrate}
            </button>
        </div>
    };

    // The step calculator, opened with the profile's optics.
    let get = move |k: &str, d: f64| settings.with_untracked(|s| s.get(k).and_then(Value::as_f64)).unwrap_or(d);
    let fl = RwSignal::new(get(KEY_FOCAL_LENGTH, 0.0).to_string());
    let px = RwSignal::new(get(KEY_PIXEL_SIZE, 0.0).to_string());
    let bin = RwSignal::new(get("/camera/binning", 1.0).to_string());
    let speed = RwSignal::new("0.5".to_string());
    let dec = RwSignal::new("0".to_string());
    let steps = RwSignal::new("12".to_string());
    let dist = RwSignal::new(get(KEY_CAL_DISTANCE, 25.0).to_string());
    let result = Memo::new(move |_| {
        let (f, p) = (parse(fl).unwrap_or(0.0), parse(px).unwrap_or(0.0));
        if f < 50.0 {
            return Err(tr().guider_calstep_need_fl);
        }
        if p <= 0.0 {
            return Err(tr().guider_calstep_need_px);
        }
        let clamp = |v: Option<f64>, lo: f64, hi: f64, d: f64| v.unwrap_or(d).clamp(lo, hi);
        Ok(calstep(
            f,
            p.clamp(0.1, 25.0),
            clamp(parse(bin), 1.0, 8.0, 1.0),
            clamp(parse(speed), 0.1, 2.0, 0.5),
            clamp(parse(steps), 6.0, 60.0, 12.0),
            clamp(parse(dec), -60.0, 60.0, 0.0),
            clamp(parse(dist), 10.0, 200.0, 25.0),
        ))
    });
    let r_apply = rpc.clone();
    let calc = view! {
        {field(move || tr().guider_focal_length, fl)}
        {field(move || tr().guider_cam_pixel_size, px)}
        {field(move || tr().guider_cam_binning, bin)}
        {field(move || tr().guider_calstep_speed, speed)}
        {field(move || tr().guider_cal_declination, dec)}
        {field(move || tr().guider_calstep_steps, steps)}
        {field(move || tr().guider_cal_distance, dist)}
        <div class="flex items-center gap-2 pt-1">
            <span class="flex-1 min-w-0 text-sm font-mono">
                {move || match result.get() {
                    Ok((k, ms)) => view! {
                        <span class="text-text-dim">{format!("{k:.2}\u{2033}/px \u{00b7} {ms} ms")}</span>
                    }.into_any(),
                    Err(e) => view! { <span class="text-state-warn">{e}</span> }.into_any(),
                }}
            </span>
            <button class="btn btn-ghost h-9 px-3" disabled=move || result.with(Result::is_err)
                    on:click=move |_| {
                        let Ok((_, ms)) = result.get_untracked() else { return };
                        let d = parse(dist).unwrap_or(25.0).clamp(10.0, 200.0).round() as i64;
                        call(r_apply.set_profile_value(KEY_CAL_DURATION, json!(ms)));
                        call(r_apply.set_profile_value(KEY_CAL_DISTANCE, json!(d)));
                    }>
                {move || tr().guider_apply}
            </button>
        </div>
    };

    // Manual calibration, opened with the current one.
    let cur = move |k: &str| cal.with_untracked(|c| c[k].as_f64()).map(|v| format!("{v:.3}")).unwrap_or_default();
    let m: [RwSignal<String>; 5] =
        ["x_rate", "y_rate", "x_angle_deg", "y_angle_deg", "declination"].map(|k| RwSignal::new(cur(k)));
    let m_bin = RwSignal::new(cal.with_untracked(|c| c["binning"].as_i64()).unwrap_or(1).to_string());
    let r_manual = rpc.clone();
    let manual = view! {
        <span class="text-xs text-text-muted">{move || tr().guider_manual_cal_hint}</span>
        {field(move || tr().guider_manual_cal_ra_rate, m[0])}
        {field(move || tr().guider_manual_cal_dec_rate, m[1])}
        {field(move || tr().guider_cal_ra_angle, m[2])}
        {field(move || tr().guider_cal_dec_angle, m[3])}
        {field(move || tr().guider_cal_declination, m[4])}
        {field(move || tr().guider_cam_binning, m_bin)}
        <div class="flex justify-end pt-1">
            <button class="btn btn-ghost h-9 px-3" disabled=move || busy.get()
                    on:click=move |_| {
                        let v: Option<Vec<f64>> = m.iter().map(|s| parse(*s)).collect();
                        let Some(v) = v else {
                            err.set(Some(tr().guider_manual_cal_incomplete.to_string()));
                            return;
                        };
                        call(r_manual.call("set_calibration_data", json!({
                            "which": which.get_untracked(), "xRate": v[0], "yRate": v[1], "xAngle": v[2], "yAngle": v[3],
                            "declination": v[4], "binning": parse(m_bin).unwrap_or(1.0).round() as i64,
                        })));
                    }>
                {move || tr().guider_apply}
            </button>
        </div>
    };

    let title = |f: fn(&Translations) -> &'static str| move || f(t(lang.get()));
    view! {
        <div class="flex-1 min-h-0 overflow-y-auto [overscroll-behavior:contain] p-4 pb-[max(1rem,env(safe-area-inset-bottom))] flex flex-col gap-3">
            {move || err.get().map(|m| view! { <div class="text-sm text-state-err">{m}</div> })}
            <div class="grid gap-3 md:grid-cols-2 items-start">
                <div class=CARD>
                    <div class="flex items-center gap-1.5">
                        <span class=format!("{CARD_TITLE} flex-1")>{move || tr().guider_cal_review}</span>
                        <Show when=move || has_ao.get()>
                            {chip(Signal::derive(move || which.get() == "mount"), lbl(|t| t.guider_role_mount), Box::new(move || which.set("mount")))}
                            {chip(Signal::derive(move || which.get() == "ao"), lbl(|t| t.guider_role_ao), Box::new(move || which.set("ao")))}
                        </Show>
                    </div>
                    {review}
                </div>
                <div class="flex flex-col gap-3">
                    <div class=CARD>{actions}</div>
                    {cal_assistant_card(c.tools, c.rpc.clone(), lang)}
                    {section(title(|t| t.guider_calstep_title), false, calc)}
                    {section(title(|t| t.guider_manual_cal_title), false, manual)}
                </div>
            </div>
        </div>
    }
}
