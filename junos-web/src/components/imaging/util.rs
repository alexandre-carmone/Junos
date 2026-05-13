//! Small DOM and data utilities used across the Imaging tab.

use wasm_bindgen::JsCast;

pub(super) fn default_capture_setting_value(key: &str) -> Option<serde_json::Value> {
    match key {
        // One-shot + sequence common defaults
        "captureExposureN" => serde_json::Number::from_f64(1.0).map(serde_json::Value::Number),
        "captureTypeS" => Some(serde_json::Value::String("Light".to_string())),
        "captureCountN" => Some(serde_json::Value::Number(1.into())),
        "captureDelayN" => Some(serde_json::Value::Number(0.into())),
        "captureBinHN" => Some(serde_json::Value::Number(1.into())),
        "captureBinVN" => Some(serde_json::Value::Number(1.into())),
        "captureGainN" => Some(serde_json::Value::Number(100.into())),
        "captureOffsetN" => Some(serde_json::Value::Number(0.into())),
        "cameraTemperatureEnforceB" => Some(serde_json::Value::Bool(false)),
        "cameraTemperatureN" => serde_json::Number::from_f64(-10.0).map(serde_json::Value::Number),
        _ => None,
    }
}

pub(super) fn initial_preview_visible() -> bool {
    web_sys::window()
        .and_then(|w| w.inner_width().ok())
        .and_then(|v| v.as_f64())
        .map(|w| w >= 900.0)
        .unwrap_or(true)
}

pub(super) fn capture_reveal_path(settings: &serde_json::Value) -> Option<String> {
    let dir = settings
        .get("fileDirectoryT")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if dir.is_empty() {
        None
    } else {
        Some(dir.to_string())
    }
}

pub(super) fn event_target_value(ev: &web_sys::Event) -> String {
    let Some(target) = ev.target() else { return String::new(); };
    if let Ok(el) = target.clone().dyn_into::<web_sys::HtmlInputElement>() {
        return el.value();
    }
    if let Ok(el) = target.clone().dyn_into::<web_sys::HtmlSelectElement>() {
        return el.value();
    }
    if let Ok(el) = target.dyn_into::<web_sys::HtmlTextAreaElement>() {
        return el.value();
    }
    String::new()
}
