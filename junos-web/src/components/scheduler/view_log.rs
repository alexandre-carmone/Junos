use leptos::html;
use leptos::prelude::*;

use crate::compat::SchedulerSnapshot;
use crate::i18n::{t, Lang};

/// Live scheduler log — the full accumulated `getLogText()` KStars pushes via
/// `new_scheduler_state {log}`. Rendered as a scrollable monospace panel that
/// sticks to the bottom as new lines arrive.
#[component]
pub fn SchedulerLogSection(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

    let log_ref = NodeRef::<html::Pre>::new();

    // Auto-scroll to the bottom whenever the log grows.
    Effect::new(move |_| {
        let _ = scheduler.get().log; // track
        if let Some(pre) = log_ref.get() {
            pre.set_scroll_top(pre.scroll_height());
        }
    });

    view! {
        <div class="sched-section-bar">
            <span class="sched-section-label">{move || tr().sched_log_title}</span>
        </div>
        <div class="sched-log-wrap">
            <Show
                when=move || !scheduler.get().log.trim().is_empty()
                fallback=move || view! {
                    <div class="sched-log-empty">{move || tr().sched_log_empty}</div>
                }
            >
                <pre class="sched-log-panel" node_ref=log_ref>
                    {move || scheduler.get().log}
                </pre>
            </Show>
        </div>
    }
}
