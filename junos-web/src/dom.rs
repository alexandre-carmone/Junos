//! Small helpers for reading values out of DOM events.
//!
//! `leptos::ev::Event` is a re-export of `web_sys::Event`, so these work for
//! both `on:input` and `on:change` handlers.

use wasm_bindgen::JsCast;

/// Current value of the element that fired the event.
///
/// Handles `<input>`, `<select>` and `<textarea>`; returns an empty string
/// for anything else, so a handler never panics on an unexpected target.
pub fn event_target_value(ev: &web_sys::Event) -> String {
    let Some(target) = ev.target() else { return String::new() };
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

/// Checked state of the `<input type="checkbox">` that fired the event.
/// False when the target is something else.
pub fn event_target_checked(ev: &web_sys::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|el| el.checked())
        .unwrap_or(false)
}
