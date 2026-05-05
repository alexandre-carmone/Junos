use std::sync::Arc;

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::i18n::{t, Lang};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::utils::{
    event_checked, event_select_value, event_value, parse_f64, parse_i64, FIELD_CLS, INPUT_CLS,
    PANEL_CLS, SMALL_BTN, SUMMARY_CLS,
};

#[component]
pub(super) fn LiveStackSettings(
    settings: RwSignal<Value>,
    current_path: RwSignal<String>,
    send: SendCmd,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let dir_in = RwSignal::new(String::new());
    let dir_out = RwSignal::new(String::new());
    let align_method = RwSignal::new("0".to_string());
    let stacking_method = RwSignal::new("0".to_string());
    let downscale = RwSignal::new("0".to_string());
    let num_in_mem = RwSignal::new("10".to_string());
    let weighting = RwSignal::new("0".to_string());
    let low_sigma = RwSignal::new("2.0".to_string());
    let high_sigma = RwSignal::new("3.0".to_string());
    let looping = RwSignal::new(false);
    let calc_snr = RwSignal::new(true);
    let post_process = RwSignal::new(false);
    let sharpen = RwSignal::new("0.0".to_string());
    let denoise = RwSignal::new("0.0".to_string());
    let deconv = RwSignal::new("0.0".to_string());
    let master_dark = RwSignal::new(String::new());
    let master_flat = RwSignal::new(String::new());

    Effect::new(move |_| {
        hydrate_livestack_settings(
            settings.get(),
            dir_in,
            dir_out,
            align_method,
            stacking_method,
            downscale,
            num_in_mem,
            weighting,
            low_sigma,
            high_sigma,
            looping,
            calc_snr,
            post_process,
            sharpen,
            denoise,
            deconv,
            master_dark,
            master_flat,
        )
    });

    let send_apply = Arc::clone(&send);
    let on_apply = move |_| {
        let payload = json!({
            "stackingDirectory": dir_in.get(),
            "outputDirectory":   dir_out.get(),
            "alignMethod":       parse_i64(&align_method.get(), 0),
            "stackingMethod":    parse_i64(&stacking_method.get(), 0),
            "downscale":         parse_i64(&downscale.get(), 0),
            "numInMem":          parse_i64(&num_in_mem.get(), 10),
            "weighting":         parse_i64(&weighting.get(), 0),
            "looping":           looping.get(),
            "calcSNR":           calc_snr.get(),
            "lowSigma":          parse_f64(&low_sigma.get(), 2.0),
            "highSigma":         parse_f64(&high_sigma.get(), 3.0),
            "postProcess":       post_process.get(),
            "sharpenAmt":        parse_f64(&sharpen.get(), 0.0),
            "denoiseAmt":        parse_f64(&denoise.get(), 0.0),
            "deconvAmt":         parse_f64(&deconv.get(), 0.0),
            "masterDarkPath":    master_dark.get(),
            "masterFlatPath":    master_flat.get(),
        });
        send_cmd(&send_apply, "livestacker_set_all_settings", payload);
    };

    let send_reset = Arc::clone(&send);
    let pick_current_in = move |_| dir_in.set(current_path.get());
    let pick_current_out = move |_| dir_out.set(current_path.get());

    view! {
        <details class=PANEL_CLS>
            <summary class=SUMMARY_CLS><span>{move || tr().livestack_settings}</span></summary>
            <div class="grid gap-sp-3 p-sp-4">
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_directories}</legend>
                    {text_field(move || tr().livestack_dir_in, dir_in)}
                    <button class=SMALL_BTN on:click=pick_current_in>{move || tr().files_reveal_captures}</button>
                    {text_field(move || tr().livestack_dir_out, dir_out)}
                    <button class=SMALL_BTN on:click=pick_current_out>{move || tr().files_reveal_captures}</button>
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_alignment}</legend>
                    {select_field(move || tr().livestack_align_method, align_method, vec![("0", tr().livestack_align_plate_solve), ("1", tr().livestack_align_none)])}
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_stacking}</legend>
                    {select_field(move || tr().livestack_stack_method, stacking_method, vec![("0", tr().livestack_stack_mean), ("1", tr().livestack_stack_sigma), ("2", tr().livestack_stack_windsor), ("3", tr().livestack_stack_imagemm)])}
                    {select_field(move || tr().livestack_downscale, downscale, vec![("0", tr().livestack_downscale_none), ("1", tr().livestack_downscale_x2), ("2", tr().livestack_downscale_x3), ("3", tr().livestack_downscale_x4)])}
                    {number_field(move || tr().livestack_num_in_mem, num_in_mem, "1")}
                    {select_field(move || tr().livestack_weighting, weighting, vec![("0", tr().livestack_weighting_equal), ("1", tr().livestack_weighting_hfr), ("2", tr().livestack_weighting_stars)])}
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_rejection}</legend>
                    {check_field(move || tr().livestack_looping, looping)}
                    {check_field(move || tr().livestack_calc_snr, calc_snr)}
                    {number_field(move || tr().livestack_low_sigma, low_sigma, "0.1")}
                    {number_field(move || tr().livestack_high_sigma, high_sigma, "0.1")}
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_postprocess}</legend>
                    {check_field(move || tr().livestack_post_process, post_process)}
                    {number_field(move || tr().livestack_sharpen, sharpen, "0.1")}
                    {number_field(move || tr().livestack_denoise, denoise, "0.1")}
                    {number_field(move || tr().livestack_deconv, deconv, "0.1")}
                </fieldset>
                <fieldset class="fieldset">
                    <legend class="fieldset__legend">{move || tr().livestack_section_calibration}</legend>
                    {text_field(move || tr().livestack_master_dark, master_dark)}
                    {text_field(move || tr().livestack_master_flat, master_flat)}
                </fieldset>
                <div class="flex gap-sp-2">
                    <button class="btn btn--sm btn-primary" on:click=on_apply>{move || tr().livestack_apply}</button>
                    <button class=SMALL_BTN on:click=move |_| send_cmd(&send_reset, "livestacker_get_all_settings", json!({}))>{move || tr().livestack_reset}</button>
                </div>
            </div>
        </details>
    }
}

#[allow(clippy::too_many_arguments)]
fn hydrate_livestack_settings(
    v: Value,
    dir_in: RwSignal<String>,
    dir_out: RwSignal<String>,
    align_method: RwSignal<String>,
    stacking_method: RwSignal<String>,
    downscale: RwSignal<String>,
    num_in_mem: RwSignal<String>,
    weighting: RwSignal<String>,
    low_sigma: RwSignal<String>,
    high_sigma: RwSignal<String>,
    looping: RwSignal<bool>,
    calc_snr: RwSignal<bool>,
    post_process: RwSignal<bool>,
    sharpen: RwSignal<String>,
    denoise: RwSignal<String>,
    deconv: RwSignal<String>,
    master_dark: RwSignal<String>,
    master_flat: RwSignal<String>,
) {
    set_str(&v, "stackingDirectory", dir_in);
    set_str(&v, "outputDirectory", dir_out);
    set_num(&v, "alignMethod", align_method, 0);
    set_num(&v, "stackingMethod", stacking_method, 0);
    set_num(&v, "downscale", downscale, 0);
    set_num(&v, "numInMem", num_in_mem, 10);
    set_num(&v, "weighting", weighting, 0);
    set_float(&v, "lowSigma", low_sigma, 2.0);
    set_float(&v, "highSigma", high_sigma, 3.0);
    set_bool(&v, "looping", looping, false);
    set_bool(&v, "calcSNR", calc_snr, true);
    set_bool(&v, "postProcess", post_process, false);
    set_float(&v, "sharpenAmt", sharpen, 0.0);
    set_float(&v, "denoiseAmt", denoise, 0.0);
    set_float(&v, "deconvAmt", deconv, 0.0);
    set_str(&v, "masterDarkPath", master_dark);
    set_str(&v, "masterFlatPath", master_flat);
}

fn set_str(v: &Value, key: &str, sig: RwSignal<String>) {
    if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
        sig.set(s.to_string());
    }
}

fn set_num(v: &Value, key: &str, sig: RwSignal<String>, default: i64) {
    sig.set(
        v.get(key)
            .and_then(|x| x.as_i64())
            .unwrap_or(default)
            .to_string(),
    );
}

fn set_float(v: &Value, key: &str, sig: RwSignal<String>, default: f64) {
    sig.set(
        v.get(key)
            .and_then(|x| x.as_f64())
            .unwrap_or(default)
            .to_string(),
    );
}

fn set_bool(v: &Value, key: &str, sig: RwSignal<bool>, default: bool) {
    sig.set(v.get(key).and_then(|x| x.as_bool()).unwrap_or(default));
}

fn text_field(
    label: impl Fn() -> &'static str + Copy + 'static,
    sig: RwSignal<String>,
) -> impl IntoView {
    view! {
        <label class="flex flex-col gap-sp-1 text-sm text-text-muted">
            <span>{label()}</span>
            <input type="text" class=INPUT_CLS prop:value=move || sig.get() on:input=move |ev| sig.set(event_value(&ev)) />
        </label>
    }
}

fn number_field(
    label: impl Fn() -> &'static str + Copy + 'static,
    sig: RwSignal<String>,
    step: &'static str,
) -> impl IntoView {
    view! {
        <label class=FIELD_CLS>
            <span>{label()}</span>
            <input type="number" step=step class="input input--sm w-[120px] num" prop:value=move || sig.get() on:input=move |ev| sig.set(event_value(&ev)) />
        </label>
    }
}

fn check_field(
    label: impl Fn() -> &'static str + Copy + 'static,
    sig: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <label class="flex items-center justify-between gap-sp-3 text-sm text-text-muted">
            <span>{label()}</span>
            <input type="checkbox" prop:checked=move || sig.get() on:change=move |ev| sig.set(event_checked(&ev)) />
        </label>
    }
}

fn select_field(
    label: impl Fn() -> &'static str + Copy + 'static,
    sig: RwSignal<String>,
    options: Vec<(&'static str, &'static str)>,
) -> impl IntoView {
    view! {
        <label class=FIELD_CLS>
            <span>{label()}</span>
            <select class="input input--sm min-w-[150px]" prop:value=move || sig.get() on:change=move |ev| sig.set(event_select_value(&ev))>
                {options.into_iter().map(|(value, label)| view! { <option value=value>{label}</option> }).collect_view()}
            </select>
        </label>
    }
}
