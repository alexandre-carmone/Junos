//! What a sequence leaves the flat panel and dust cap doing.
//!
//! KStars closes the cap and lights the panel for flats, and undoes both
//! before the next light frame (`SequenceJobState`, sequencejobstate.cpp) —
//! but a sequence that ends on calibration frames leaves them as they are
//! (the end-of-capture "light off" in `CameraProcess::stopCapturing` checks a
//! flag nothing sets). So the last row can carry a `<PostJobScript>` that
//! does it through KStars' own D-Bus API, whatever host the INDI server is
//! on. KStars runs the script with no shell and no arguments, so it is
//! written through `/api/taskqueue/script` (mode 0755) as soon as the option
//! is picked, and referenced by the absolute path that returns.

use leptos::prelude::*;

use crate::components::scheduler::save_script;
use crate::i18n::{t, Lang, Translations};

use super::card::{LABEL, PILL};
use super::model::SeqFrame;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum EndAction {
    #[default]
    Leave,
    PanelOff,
    PanelOffOpenCap,
}

/// Every flat-panel light off, then (`open_cap`) every dust cap open.
fn script_body(open_cap: bool) -> String {
    let mut s = String::from(r#"#!/usr/bin/env sh
# Written by junos-web. KStars runs it after a sequence's last frame
# (<PostJobScript>). It goes through KStars' own D-Bus API, so it reaches the
# devices wherever their INDI server runs. Needs dbus-send.

# KStars pipes the job as JSON on stdin; not needed here.
cat > /dev/null

# D-Bus object paths of the devices with one INDI interface bit.
paths() {
    out=$(dbus-send --session --print-reply --dest=org.kde.kstars /KStars/INDI \
        org.kde.kstars.INDI.getDevicesPaths "uint32:$1") || exit 1
    printf '%s\n' "$out" | sed -n 's/^ *string "\(.*\)"$/\1/p'
}
call() { dbus-send --session --print-reply --dest=org.kde.kstars "$@" > /dev/null; }

status=0
# Panel lights off (LIGHTBOX_INTERFACE = 1 << 10).
lights=$(paths 1024) || exit 1
for p in $lights; do
    call "$p" org.kde.kstars.INDI.LightBox.setLightEnabled boolean:false || status=1
done
"#);
    if open_cap {
        s.push_str(r#"# Dust caps open (DUSTCAP_INTERFACE = 1 << 9).
caps=$(paths 512) || exit 1
for p in $caps; do
    call "$p" org.kde.kstars.INDI.DustCap.unpark || status=1
done
"#);
    }
    s.push_str("exit $status\n");
    s
}

impl EndAction {
    /// The script's name in the task-queue `scripts/` folder, and its body.
    fn script(self) -> Option<(&'static str, String)> {
        match self {
            Self::Leave => None,
            Self::PanelOff => Some(("junos_panel_off", script_body(false))),
            Self::PanelOffOpenCap => Some(("junos_panel_off_open_cap", script_body(true))),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SeqEnd {
    pub action: EndAction,
    /// The last write: for which action, and its path or why it failed.
    /// `None` while a write is in flight.
    pub script: Option<(EndAction, Result<String, String>)>,
}

/// Only calibration rows move the cap or light the panel.
fn has_calibration(frames: &[SeqFrame]) -> bool {
    frames.iter().any(|f| f.frame_type != "Light")
}

impl SeqEnd {
    /// The `<PostJobScript>` for the last of `frames`: `None` when there's
    /// nothing to undo, `Err` while the chosen script isn't written.
    pub fn post_job_script(&self, frames: &[SeqFrame]) -> Result<Option<String>, ()> {
        if self.action == EndAction::Leave || !has_calibration(frames) {
            return Ok(None);
        }
        match &self.script {
            Some((action, Ok(path))) if *action == self.action => Ok(Some(path.clone())),
            _ => Err(()),
        }
    }
}

/// Write the chosen action's script, tagging the result with the action so a
/// late reply for an earlier pick can't stand in for the current one.
fn write_script(end: RwSignal<SeqEnd>, action: EndAction) {
    let Some((name, body)) = action.script() else { return };
    end.update(|e| e.script = None);
    wasm_bindgen_futures::spawn_local(async move {
        let result = save_script(name, &body).await;
        end.update(|e| e.script = Some((action, result)));
    });
}

fn action_label(action: EndAction, s: &Translations) -> &'static str {
    match action {
        EndAction::Leave => s.seq_end_leave,
        EndAction::PanelOff => s.seq_end_panel_off,
        EndAction::PanelOffOpenCap => s.seq_end_open_cap,
    }
}

/// The "After the last frame" block, shown while the sequence has a
/// calibration row.
#[component]
pub fn EndRow(end: RwSignal<SeqEnd>, frames: RwSignal<Vec<SeqFrame>>) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let shown = Memo::new(move |_| frames.with(|f| has_calibration(f)));
    let action = Memo::new(move |_| end.with(|e| e.action));
    let pick = move |a: EndAction| {
        end.update(|e| e.action = a);
        write_script(end, a);
    };
    // A draft that already holds an action (the sheet was closed and opened
    // again, or the page restored it) without its script gets it written.
    Effect::new(move |_| {
        let a = action.get_untracked();
        if end.with_untracked(|e| e.script.is_none() && a != EndAction::Leave) {
            write_script(end, a);
        }
    });

    let status = move || {
        let s = tr();
        end.with(|e| match (e.action, &e.script) {
            (EndAction::Leave, _) => None,
            (a, Some((done, Ok(path)))) if *done == a => Some(view! {
                <span class="text-xs leading-snug text-text-muted">
                    {s.seq_end_hint}" "<span class="font-mono break-all">{path.clone()}</span>
                </span>
            }.into_any()),
            (a, Some((done, Err(err)))) if *done == a => Some(view! {
                <span class="text-xs leading-snug text-state-err">{format!("{} {err}", s.seq_end_failed)}</span>
            }.into_any()),
            _ => Some(view! { <span class="text-xs text-text-muted">{s.seq_end_saving}</span> }.into_any()),
        })
    };

    view! {
        <Show when=move || shown.get()>
            <div class="flex flex-col gap-sp-2 rounded-md border border-border-base p-sp-2">
                <span class=LABEL>{move || tr().seq_end}</span>
                <div class="flex flex-wrap gap-[4px]">
                    {[EndAction::Leave, EndAction::PanelOff, EndAction::PanelOffOpenCap].into_iter().map(|a| view! {
                        <button type="button" class=PILL
                                class:btn--active=move || action.get() == a
                                on:click=move |_| pick(a)>
                            {move || action_label(a, tr())}
                        </button>
                    }).collect::<Vec<_>>()}
                </div>
                {status}
            </div>
        </Show>
    }
}
