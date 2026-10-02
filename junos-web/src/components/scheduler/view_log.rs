//! Scheduler tab: the live log.

use leptos::prelude::*;

use crate::components::form::{CARD, CARD_TITLE};
use crate::i18n::{t, Lang};

/// The full accumulated `getLogText()` KStars pushes via
/// `new_scheduler_state {log}`, newest line first so fresh entries appear on
/// top and never shift the reader's scroll position.
#[component]
pub fn SchedulerLog(#[prop(into)] log: Signal<String>, lang: RwSignal<Lang>) -> impl IntoView {
    let tr = move || t(lang.get());

    view! {
        <div class=format!("{CARD} md:min-h-0")>
            <span class=CARD_TITLE>{move || tr().sched_log_title}</span>
            <Show
                when=move || log.with(|l| !l.trim().is_empty())
                fallback=move || view! { <div class="py-4 text-sm text-text-faint">{move || tr().sched_log_empty}</div> }
            >
                <pre class="m-0 max-h-[40dvh] md:max-h-none md:flex-1 md:min-h-0 overflow-auto [overscroll-behavior:contain] \
                            font-mono text-xs leading-relaxed text-text-dim whitespace-pre-wrap break-words">
                    {move || log.with(|l| l.lines().rev().collect::<Vec<_>>().join("\n"))}
                </pre>
            </Show>
        </div>
    }
}
