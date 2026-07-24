use leptos::prelude::*;

use crate::compat::SchedulerSnapshot;
use crate::i18n::{t, Lang};

/// Live scheduler log — the full accumulated `getLogText()` KStars pushes via
/// `new_scheduler_state {log}`. Rendered newest-line-first so fresh entries
/// appear at the top; the scroll position is never disturbed by updates.
#[component]
pub fn SchedulerLogSection(
    #[prop(into)] scheduler: Signal<SchedulerSnapshot>,
    #[prop(into)] lang: RwSignal<Lang>,
) -> impl IntoView {
    let tr = move || t(lang.get());

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
                <pre class="sched-log-panel">
                    {move || {
                        // Newest first — reversed so the latest line is on top and
                        // incoming updates never shift the reader's scroll position.
                        scheduler.get().log
                            .lines()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n")
                    }}
                </pre>
            </Show>
        </div>
    }
}
