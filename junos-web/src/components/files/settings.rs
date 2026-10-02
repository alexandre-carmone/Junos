//! Files tab: the Live Stack settings cards. Each field shows KStars' value
//! (`livestacker_get_all_settings`) and sends its own key on change:
//! `livestacker_set_all_settings` merges the keys it gets and echoes the
//! whole map back (message.cpp:3404). The defaults are KStars' own, for keys
//! it hasn't stored yet (message.cpp:3490).

use leptos::prelude::*;
use serde_json::{json, Value};

use crate::components::form::{setting_row, CARD, CARD_TITLE, CHECK, LABEL, NUM, SELECT};
use crate::dom::{event_target_checked, event_target_value};
use crate::i18n::{t, Lang, Translations};
use crate::ws::SendCmd;
use crate::ws_helpers::send_cmd;

use super::Shared;

type Label = fn(&'static Translations) -> &'static str;

#[derive(Clone)]
struct Form {
    settings: RwSignal<Value>,
    send: SendCmd,
    lang: RwSignal<Lang>,
}

impl Form {
    fn put(&self, key: &'static str, v: Value) {
        send_cmd(&self.send, "livestacker_set_all_settings", json!({ key: v }));
    }

    fn label(&self, label: Label) -> impl Fn() -> &'static str + Copy + Send + 'static + use<> {
        let lang = self.lang;
        move || label(t(lang.get()))
    }

    fn text(&self, key: &'static str) -> Signal<String> {
        let settings = self.settings;
        Signal::derive(move || settings.with(|v| v[key].as_str().unwrap_or_default().to_string()))
    }

    /// A host path, sent on Enter / blur.
    fn path(&self, key: &'static str, label: Label) -> impl IntoView + use<> {
        let (f, value) = (self.clone(), self.text(key));
        view! {
            <label class="flex flex-col gap-1">
                <span class=LABEL>{self.label(label)}</span>
                <input type="text" class="input w-full h-11 md:h-9 font-mono text-sm" spellcheck="false"
                       prop:value=move || value.get()
                       on:change=move |ev| f.put(key, event_target_value(&ev).trim().into()) />
            </label>
        }
    }

    fn select(&self, key: &'static str, default: i64, label: Label, options: Vec<(i64, Label)>) -> impl IntoView + use<> {
        let (f, settings, lang) = (self.clone(), self.settings, self.lang);
        let cur = move || settings.with(|v| v[key].as_i64().unwrap_or(default));
        setting_row(self.label(label), view! {
            <select class=SELECT on:change=move |ev| {
                if let Ok(n) = event_target_value(&ev).parse::<i64>() { f.put(key, n.into()) }
            }>
                {options.into_iter().map(|(n, l)| view! {
                    <option value=n.to_string() prop:selected=move || cur() == n>{move || l(t(lang.get()))}</option>
                }).collect_view()}
            </select>
        })
    }

    /// Sent as a double; KStars reads integer keys with `toInt()`.
    fn number(&self, key: &'static str, default: f64, step: &'static str, label: Label) -> impl IntoView + use<> {
        let (f, settings) = (self.clone(), self.settings);
        setting_row(self.label(label), view! {
            <input type="number" step=step min="0" inputmode="decimal" class=NUM
                   prop:value=move || settings.with(|v| v[key].as_f64().unwrap_or(default)).to_string()
                   on:change=move |ev| {
                       if let Ok(x) = event_target_value(&ev).trim().parse::<f64>() { f.put(key, x.into()) }
                   } />
        })
    }

    fn check(&self, key: &'static str, default: bool, label: Label) -> impl IntoView + use<> {
        let (f, settings) = (self.clone(), self.settings);
        view! {
            <label class="flex items-center gap-3 min-h-[44px] md:min-h-9 cursor-pointer">
                <input type="checkbox" class=CHECK
                       prop:checked=move || settings.with(|v| v[key].as_bool().unwrap_or(default))
                       on:change=move |ev| f.put(key, event_target_checked(&ev).into()) />
                <span class=LABEL>{self.label(label)}</span>
            </label>
        }
    }
}

/// A stacking / output folder, with Use current folder and Open buttons.
fn folder(f: &Form, s: Shared, open: RwSignal<bool>, key: &'static str, label: Label) -> impl IntoView + use<> {
    let (put, value, lang) = (f.clone(), f.text(key), f.lang);
    let tr = move || t(lang.get());
    view! {
        <div class="flex flex-col gap-2">
            {f.path(key, label)}
            <div class="grid grid-cols-2 gap-2">
                <button class="btn btn-ghost h-11 md:h-9 px-2" disabled=move || s.root.with(String::is_empty)
                        on:click=move |_| put.put(key, s.abs(&s.path.get_untracked()).into())>
                    {move || tr().files_use_current}
                </button>
                <button class="btn btn-ghost h-11 md:h-9 px-2" disabled=move || value.with(String::is_empty)
                        on:click=move |_| {
                            s.reveal(value.get_untracked());
                            open.set(false);
                        }>
                    {move || tr().files_open}
                </button>
            </div>
        </div>
    }
}

pub(super) fn cards(s: Shared, open: RwSignal<bool>, settings: RwSignal<Value>, send: SendCmd, lang: RwSignal<Lang>) -> impl IntoView {
    let f = Form { settings, send, lang };
    let title = |label: Label| view! { <span class=CARD_TITLE>{f.label(label)}</span> };
    view! {
        <div class=CARD>
            {title(|t| t.livestack_section_directories)}
            {folder(&f, s, open, "stackingDirectory", |t| t.livestack_dir_in)}
            {folder(&f, s, open, "outputDirectory", |t| t.livestack_dir_out)}
        </div>
        <div class=CARD>
            {title(|t| t.livestack_section_stacking)}
            {f.select("alignMethod", 0, |t| t.livestack_align_method, vec![
                (0, |t| t.livestack_align_plate_solve),
                (1, |t| t.livestack_align_none),
            ])}
            {f.select("stackingMethod", 0, |t| t.livestack_stack_method, vec![
                (0, |t| t.livestack_stack_mean),
                (1, |t| t.livestack_stack_sigma),
                (2, |t| t.livestack_stack_windsor),
                (3, |t| t.livestack_stack_imagemm),
            ])}
            {f.select("weighting", 0, |t| t.livestack_weighting, vec![
                (0, |t| t.livestack_weighting_equal),
                (1, |t| t.livestack_weighting_hfr),
                (2, |t| t.livestack_weighting_stars),
            ])}
            {f.select("downscale", 0, |t| t.livestack_downscale, vec![
                (0, |t| t.livestack_downscale_none),
                (1, |t| t.livestack_downscale_x2),
                (2, |t| t.livestack_downscale_x3),
                (3, |t| t.livestack_downscale_x4),
            ])}
            {f.number("numInMem", 10.0, "1", |t| t.livestack_num_in_mem)}
            {f.check("looping", false, |t| t.livestack_looping)}
        </div>
        <div class=CARD>
            {title(|t| t.livestack_section_rejection)}
            {f.check("calcSNR", true, |t| t.livestack_calc_snr)}
            {f.number("lowSigma", 2.0, "0.1", |t| t.livestack_low_sigma)}
            {f.number("highSigma", 3.0, "0.1", |t| t.livestack_high_sigma)}
        </div>
        <div class=CARD>
            {title(|t| t.livestack_section_postprocess)}
            {f.check("postProcess", false, |t| t.livestack_post_process)}
            {f.number("sharpenAmt", 0.0, "0.1", |t| t.livestack_sharpen)}
            {f.number("denoiseAmt", 0.0, "0.1", |t| t.livestack_denoise)}
            {f.number("deconvAmt", 0.0, "0.1", |t| t.livestack_deconv)}
        </div>
        <div class=CARD>
            {title(|t| t.livestack_section_calibration)}
            {f.path("masterDarkPath", |t| t.livestack_master_dark)}
            {f.path("masterFlatPath", |t| t.livestack_master_flat)}
        </div>
    }
}
