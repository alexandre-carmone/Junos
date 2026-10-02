//! Files tab: what the viewer's buttons do — rename, delete, copy, slew.
//! Each reports through `flash`, the tab's toast.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::i18n::Translations;
use crate::ws::SendCmd;

use super::api::{delete_file, rename_file};
use super::utils::join;

type Flash = RwSignal<Option<String>>;

fn report(flash: Flash, tr: &'static Translations, e: String) {
    flash.set(Some(format!("{}: {e}", tr.files_error)));
}

/// Ask for a new name, rename, then `done(new_rel)`.
pub(super) fn rename(rel: String, flash: Flash, tr: &'static Translations, done: impl FnOnce(String) + 'static) {
    let Some(win) = web_sys::window() else { return };
    let (dir, old) = rel.rsplit_once('/').unwrap_or(("", &rel));
    let Some(name) = win.prompt_with_message_and_default(tr.files_rename_prompt, old).ok().flatten() else { return };
    let name = name.trim().to_string();
    if name.is_empty() || name == old {
        return;
    }
    let new_rel = join(dir, &name);
    wasm_bindgen_futures::spawn_local(async move {
        match rename_file(&rel, &name).await {
            Ok(()) => done(new_rel),
            Err(e) => report(flash, tr, e),
        }
    });
}

/// Confirm, delete, then `done()`.
pub(super) fn delete(rel: String, flash: Flash, tr: &'static Translations, done: impl FnOnce() + 'static) {
    let Some(win) = web_sys::window() else { return };
    if !win.confirm_with_message(tr.files_confirm_delete).unwrap_or(false) {
        return;
    }
    wasm_bindgen_futures::spawn_local(async move {
        match delete_file(&rel).await {
            Ok(()) => done(),
            Err(e) => report(flash, tr, e),
        }
    });
}

/// `navigator.clipboard` exists only in a secure context (the HTTPS port);
/// elsewhere, a prompt the user can copy from.
pub(super) fn copy_text(text: &str, flash: Flash, done_msg: &'static str) {
    let Some(win) = web_sys::window() else { return };
    let clip = js_sys::Reflect::get(&win.navigator(), &"clipboard".into()).unwrap_or_default();
    let write = js_sys::Reflect::get(&clip, &"writeText".into()).ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    match write {
        Some(f) if f.call1(&clip, &text.into()).is_ok() => flash.set(Some(done_msg.to_string())),
        _ => drop(win.prompt_with_message_and_default(done_msg, text)),
    }
}

/// Plate-solve a captured file and slew the mount to its framing, reproducing a
/// prior night's target. Dispatches `align_load_and_slew` with a `{filename}`
/// payload so KStars reads the file straight from disk (message.cpp:1081) —
/// captures live on the host KStars runs on. The `{data, ext}` base64 form the
/// Mount tab's "Load FITS" uses can't carry a full-size capture: a ~300 MB FITS
/// becomes a ~400 MB text frame, far past the relay's 16 MiB WebSocket frame
/// cap, so the browser socket is dropped and nothing reaches KStars.
/// `Align::loadAndSlew` forces GOTO_SLEW, so it solves then slews on its own.
pub(super) fn resolve_and_slew(abs: String, send: &SendCmd, flash: Flash, tr: &'static Translations) {
    let Some(win) = web_sys::window() else { return };
    if abs.is_empty() {
        flash.set(Some(tr.files_resolve_slew_fail.to_string()));
        return;
    }
    if !win.confirm_with_message(tr.files_resolve_slew_confirm).unwrap_or(false) {
        return;
    }
    send(serde_json::json!({ "type": "align_load_and_slew", "payload": { "filename": abs } }).to_string());
    flash.set(Some(tr.files_resolve_slew_sent.to_string()));
}
