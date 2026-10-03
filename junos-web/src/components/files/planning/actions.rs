//! Files › Planning: sending a schedule or a sequence back to KStars — from the
//! Planning sheet and from the captures viewer (a mosaic import writes both
//! next to its frames). The outcome lands in the tab's toast.

use leptos::prelude::*;
use serde_json::json;

use crate::i18n::Translations;
use crate::ws::{FileReply, SendCmd};
use crate::ws_helpers::{file_command, send_cmd};

use super::parse::Kind;

/// KStars answers a load at once; past this it never will (Ekos not started).
const REPLY_MS: u32 = 5000;

pub(crate) fn load_label(kind: Kind, tr: &'static Translations) -> Option<&'static str> {
    match kind {
        Kind::Schedules => Some(tr.plan_load_schedule),
        Kind::Sequences => Some(tr.plan_load_sequence),
        Kind::Queues | Kind::Scripts => None,
    }
}

/// Confirm, then `scheduler_load_file` (replaces the Scheduler's job list) or
/// `capture_load_sequence_file` (replaces the Imaging queue) with the host
/// path `abs`.
pub(crate) fn load(
    kind: Kind,
    abs: String,
    send: SendCmd,
    reply: RwSignal<Option<FileReply>>,
    flash: RwSignal<Option<String>>,
    tr: &'static Translations,
) {
    let (cmd, confirm, done) = match kind {
        Kind::Schedules => ("scheduler_load_file", tr.plan_confirm_load_schedule, tr.plan_loaded_schedule),
        Kind::Sequences => ("capture_load_sequence_file", tr.plan_confirm_load_sequence, tr.plan_loaded_sequence),
        Kind::Queues | Kind::Scripts => return,
    };
    if abs.is_empty() {
        flash.set(Some(tr.plan_no_path.to_string()));
        return;
    }
    let Some(win) = web_sys::window() else { return };
    if !win.confirm_with_message(confirm).unwrap_or(false) {
        return;
    }
    flash.set(Some(tr.plan_sent.to_string()));
    wasm_bindgen_futures::spawn_local(async move {
        let msg = match file_command(&send, reply, cmd, json!({ "filepath": abs }), REPLY_MS).await {
            Some(r) if r.ok => {
                if kind == Kind::Schedules {
                    // The loaded file brings its own jobs and procedures.
                    send_cmd(&send, "scheduler_get_jobs", json!({}));
                    send_cmd(&send, "scheduler_get_all_settings", json!({}));
                }
                done
            }
            Some(_) => tr.plan_load_failed,
            None => tr.plan_no_reply,
        };
        flash.try_set(Some(msg.to_string()));
    });
}
