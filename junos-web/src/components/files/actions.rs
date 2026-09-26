//! Files tab: the actions a file menu can trigger (download, rename, delete, slew).

use leptos::prelude::*;
use serde_json::Value;
use wasm_bindgen::JsCast;

use crate::i18n::Translations;
use crate::ws::SendCmd;

use super::api::{abs_path_of, delete_file, rename_file, resolve_abs};
use super::utils::url_encode;

pub(super) fn download_file(rel: &str) {
    let Some(win) = web_sys::window() else { return; };
    let Some(doc) = win.document() else { return; };
    if let Ok(a) = doc.create_element("a") {
        let _ = a.set_attribute("href", &format!("/api/files/download?path={}", url_encode(rel)));
        let _ = a.set_attribute("download", "");
        if let Ok(a) = a.dyn_into::<web_sys::HtmlElement>() { a.click(); }
    }
}

pub(super) fn rename_file_action(rel: &str, refresh_tick: RwSignal<u32>, selected: RwSignal<Option<String>>, tr: &'static Translations) {
    let Some(win) = web_sys::window() else { return; };
    let old_name = rel.rsplit('/').next().unwrap_or(rel);
    let Some(new_name) = win.prompt_with_message_and_default(tr.files_rename_prompt, old_name).ok().flatten() else { return; };
    if new_name.trim().is_empty() || new_name == old_name { return; }
    let path = rel.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        if rename_file(&path, &new_name).await.is_ok() {
            let new_rel = if let Some(i) = path.rfind('/') { format!("{}/{}", &path[..i], new_name) } else { new_name };
            selected.set(Some(new_rel));
            refresh_tick.update(|n| *n = n.wrapping_add(1));
        }
    });
}

pub(super) fn delete_file_action(rel: &str, refresh_tick: RwSignal<u32>, selected: RwSignal<Option<String>>, flash: RwSignal<Option<String>>, tr: &'static Translations) {
    let Some(win) = web_sys::window() else { return; };
    if !win.confirm_with_message(tr.files_confirm_delete).unwrap_or(false) { return; }
    let path = rel.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        if delete_file(&path).await.is_ok() {
            selected.set(None);
            flash.set(Some(tr.files_delete.to_string()));
            refresh_tick.update(|n| *n = n.wrapping_add(1));
        }
    });
}

/// Plate-solve a captured file and slew the mount to its framing, reproducing a
/// prior night's target. Dispatches `align_load_and_slew` with a `{filename}`
/// payload so KStars reads the file straight from disk (message.cpp:1081) —
/// captures live on the host KStars runs on. The `{data, ext}` base64 form the
/// Mount tab's "Load FITS" uses can't carry a full-size capture: a ~300 MB FITS
/// becomes a ~400 MB text frame, far past the relay's 16 MiB WebSocket frame
/// cap, so the browser socket is dropped and nothing reaches KStars.
/// `Align::loadAndSlew` forces GOTO_SLEW, so it solves then slews on its own.
pub(super) fn resolve_and_slew(
    rel: &str,
    send: SendCmd,
    flash: RwSignal<Option<String>>,
    tr: &'static Translations,
) {
    let Some(win) = web_sys::window() else { return; };
    if !win.confirm_with_message(tr.files_resolve_slew_confirm).unwrap_or(false) { return; }
    let path = rel.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let Ok(abs) = abs_path_of(&path).await else {
            flash.set(Some(tr.files_resolve_slew_fail.to_string()));
            return;
        };
        send(serde_json::json!({
            "type": "align_load_and_slew",
            "payload": { "filename": abs }
        }).to_string());
        flash.set(Some(tr.files_resolve_slew_sent.to_string()));
    });
}

pub(super) fn copy_to_clipboard(text: &str, flash: RwSignal<Option<String>>, msg: &'static str) {
    // web-sys clipboard APIs are not always enabled in this crate; show a
    // selectable prompt as a reliable fallback.
    if let Some(win) = web_sys::window() {
        let _ = win.prompt_with_message_and_default(msg, text);
    }
    flash.set(Some(msg.to_string()));
}

pub(super) fn open_abs_setting_dir(settings: Value, key: &'static str, current_path: RwSignal<String>, selected: RwSignal<Option<String>>, refresh_tick: RwSignal<u32>) {
    let Some(abs) = settings.get(key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string) else { return; };
    wasm_bindgen_futures::spawn_local(async move {
        if let Ok(r) = resolve_abs(&abs).await {
            if r.in_sandbox {
                current_path.set(if r.relative.is_empty() { r.parent } else { r.relative });
                selected.set(None);
                refresh_tick.update(|n| *n = n.wrapping_add(1));
            }
        }
    });
}
