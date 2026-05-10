use leptos::prelude::*;
use serde_json::Value;
use wasm_bindgen::JsCast;

pub(super) const PANEL_CLS: &str = "panel overflow-hidden";
pub(super) const SUMMARY_CLS: &str = "flex cursor-pointer list-none items-center justify-between gap-sp-3 border-b border-border px-sp-4 py-sp-3 text-sm font-semibold uppercase tracking-[0.06em] text-text-blue [&::-webkit-details-marker]:hidden";
pub(super) const PANEL_BODY: &str = "p-sp-4";
pub(super) const FIELD_CLS: &str = "field justify-between";
pub(super) const INPUT_CLS: &str = "input input--sm w-full";
pub(super) const SELECT_CLS: &str = "input input--sm";
pub(super) const SMALL_BTN: &str = "btn btn--sm btn-ghost";
pub(super) const KV_ROW: &str = "grid grid-cols-[minmax(92px,auto)_1fr] gap-sp-2 text-sm";
pub(super) const FILE_CARD: &str = "group relative flex min-h-[164px] flex-col overflow-hidden rounded-md border border-border-strong bg-bg-elev-2 text-left text-text transition hover:border-border-mid hover:bg-bg-elev-3";
pub(super) const FILE_CARD_ACTIVE: &str = "group relative flex min-h-[164px] flex-col overflow-hidden rounded-md border border-accent-cyan bg-[color-mix(in_srgb,var(--accent-cyan)_14%,var(--bg-elev-2))] text-left text-text shadow-[0_0_0_2px_rgba(40,220,240,0.16)]";
pub(super) const FILE_ROW: &str = "group flex items-center gap-sp-3 rounded-md border border-border-strong bg-bg-elev-2 px-sp-3 py-sp-2 text-left text-sm text-text transition hover:border-border-mid hover:bg-bg-elev-3";
pub(super) const FILE_ROW_ACTIVE: &str = "group flex items-center gap-sp-3 rounded-md border border-accent-cyan bg-[color-mix(in_srgb,var(--accent-cyan)_14%,var(--bg-elev-2))] px-sp-3 py-sp-2 text-left text-sm text-text";

pub(super) fn kv(label: &'static str, value: String) -> impl IntoView {
    view! { <div class=KV_ROW><span class="text-text-muted">{label}</span><span class="break-words text-right text-text-dim num">{value}</span></div> }
}

pub(super) fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

pub(super) fn is_image_ext(ext: &str) -> bool {
    is_fits_ext(ext)
        || is_jpg_ext(ext)
        || matches!(
            ext.to_ascii_lowercase().as_str(),
            "tif" | "tiff" | "xisf" | "cr2" | "nef" | "arw"
        )
}

pub(super) fn is_fits_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts")
}

pub(super) fn is_jpg_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
}

pub(super) fn url_encode(s: &str) -> String {
    js_sys::encode_uri_component(s)
        .as_string()
        .unwrap_or_default()
}

pub(super) fn event_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

pub(super) fn event_select_value(ev: &web_sys::Event) -> String {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

pub(super) fn event_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or(false)
}

pub(super) fn parse_i64(v: &str, default: i64) -> i64 {
    v.trim().parse().unwrap_or(default)
}
pub(super) fn parse_f64(v: &str, default: f64) -> f64 {
    v.trim().parse().unwrap_or(default)
}

pub(super) fn format_size(n: u64) -> String {
    if n < 1024 {
        return format!("{} B", n);
    }
    let kb = n as f64 / 1024.0;
    if kb < 1024.0 {
        return format!("{:.1} KB", kb);
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{:.1} MB", mb);
    }
    format!("{:.1} GB", mb / 1024.0)
}

pub(super) fn format_mtime(secs: u64) -> String {
    if secs == 0 {
        return "—".into();
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
    d.to_iso_string().as_string().unwrap_or_default()
}

pub(super) fn value_or_dash(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "—".into(),
        Some(Value::String(s)) if s.is_empty() => "—".into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => (if *b { "Y" } else { "N" }).into(),
        Some(Value::Number(n)) => n.as_f64().map(fmt_float).unwrap_or_else(|| n.to_string()),
        Some(other) => other.to_string(),
    }
}

pub(super) fn fmt_float(f: f64) -> String {
    if !f.is_finite() || f == 0.0 {
        return "—".into();
    }
    format!("{:.4}", f)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

pub(super) fn fov_str(v: Option<&Value>) -> String {
    let Some(o) = v else { return "—".into() };
    if o.is_null() {
        return "—".into();
    }
    let w = o.get("w").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let h = o.get("h").and_then(|x| x.as_f64()).unwrap_or(0.0);
    format!("{:.1} x {:.1}", w, h)
}
