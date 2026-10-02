//! Files tab: small helpers — paths, extensions, formatting, icons.

use serde_json::Value;

pub(super) const DASH: &str = "\u{2014}";

// 24×24, `currentColor`, sized by the wrapping <span> (like `tab_icon`).
pub(super) const FOLDER_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7 L3 18 A1 1 0 0 0 4 19 L20 19 A1 1 0 0 0 21 18 L21 9 A1 1 0 0 0 20 8 L12 8 L10 5.5 L4 5.5 A1 1 0 0 0 3 6.5 Z"/></svg>"##;
pub(super) const REFRESH_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M22 4v6h-6"/><path d="M19.5 15a8.5 8.5 0 1 1-2-8.8L22 10"/></svg>"##;
pub(super) const TRASH_ICON: &str = r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/></svg>"##;

/// `dir/name`, or `name` at the captures root.
pub(super) fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

pub(super) fn parent_of(path: &str) -> String {
    path.rfind('/').map(|i| path[..i].to_string()).unwrap_or_default()
}

pub(super) fn name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Extension without the dot, "" if none.
pub(super) fn ext_of(path: &str) -> &str {
    name_of(path).rsplit_once('.').map_or("", |(_, e)| e)
}

pub(super) fn is_image_ext(ext: &str) -> bool {
    is_fits_ext(ext)
        || is_jpg_ext(ext)
        || matches!(ext.to_ascii_lowercase().as_str(), "tif" | "tiff" | "xisf" | "cr2" | "nef" | "arw")
}

pub(super) fn is_fits_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts")
}

pub(super) fn is_jpg_ext(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png")
}

pub(super) fn url_encode(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

/// The server's 256 px JPEG thumbnail (cached on disk).
pub(super) fn thumb_url(rel: &str) -> String {
    format!("/api/files/thumb?size=256&path={}", url_encode(rel))
}

/// The server's full-size preview render (FITS stretched to JPEG).
pub(super) fn preview_url(rel: &str) -> String {
    format!("/api/files/raw?as=preview&path={}", url_encode(rel))
}

pub(super) fn format_size(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    for unit in ["KB", "MB"] {
        if v < 1024.0 {
            return format!("{v:.1} {unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1} GB")
}

/// Local time, `YYYY-MM-DD HH:MM` — or `MM-DD HH:MM` when `short`.
pub(super) fn format_mtime(secs: u64, short: bool) -> String {
    if secs == 0 {
        return DASH.into();
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(secs as f64 * 1000.0));
    let (mo, day, h, mi) = (d.get_month() + 1, d.get_date(), d.get_hours(), d.get_minutes());
    if short {
        format!("{mo:02}-{day:02} {h:02}:{mi:02}")
    } else {
        format!("{}-{mo:02}-{day:02} {h:02}:{mi:02}", d.get_full_year())
    }
}

pub(super) fn value_or_dash(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => DASH.into(),
        Some(Value::String(s)) if s.is_empty() => DASH.into(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => (if *b { "Y" } else { "N" }).into(),
        Some(Value::Number(n)) => n.as_f64().map(fmt_float).unwrap_or_else(|| n.to_string()),
        Some(other) => other.to_string(),
    }
}

pub(super) fn fmt_float(f: f64) -> String {
    if !f.is_finite() || f == 0.0 {
        return DASH.into();
    }
    format!("{f:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(super) fn fov_str(v: Option<&Value>) -> String {
    let (Some(w), Some(h)) = (v.and_then(|o| o["w"].as_f64()), v.and_then(|o| o["h"].as_f64())) else {
        return DASH.into();
    };
    format!("{w:.1} \u{00d7} {h:.1}")
}
