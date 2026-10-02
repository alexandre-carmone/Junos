//! Shared capture-sequence editor used by the Imaging tab, the Mosaic
//! Planner and the Scheduler "Add job" form.
//!
//! - `model.rs` — the `SeqFrame` row and its validation / duration helpers.
//! - `esq.rs`   — `build_esq_xml`, the ESQ serializer callers feed to KStars.
//! - `card.rs`  — `JobCard`, one collapsible card per row.
//!
//! This file is the editor shell: the card list (one card open at a time),
//! the destination folder and the sticky totals / "Add exposure" footer.

mod card;
mod esq;
mod model;

pub use esq::build_esq_xml;
pub use model::{SeqFrame, fmt_duration};

use leptos::prelude::*;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::dom::event_target_value;
use crate::i18n::{Lang, t};

use card::JobCard;

#[component]
pub fn SequenceEditor(
    /// Caller-owned row list. The editor reads and mutates it directly so the
    /// caller can serialize it (e.g. via `build_esq_xml`) on submit.
    frames: RwSignal<Vec<SeqFrame>>,
    /// Caller-owned destination folder. Written into each job's
    /// `<FITSDirectory>` on serialize. Defaults from `CaptureDirCtx`.
    fits_dir: RwSignal<String>,
    #[prop(into)] camera:       Signal<CameraSnapshot>,
    #[prop(into)] filter_wheel: Signal<FilterWheelSnapshot>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Pre-fill the destination folder from the server captures dir once it
    // loads, but only while the user hasn't typed a path of their own.
    let capture_dir = use_context::<crate::CaptureDirCtx>();
    Effect::new(move |_| {
        if let Some(cd) = capture_dir {
            let d = cd.0.get();
            if !d.is_empty() && fits_dir.with(|v| v.is_empty()) {
                fits_dir.set(d);
            }
        }
    });

    // Accordion: index of the single open card. The first card starts open
    // so a fresh form is immediately editable.
    let open: RwSignal<Option<usize>> = RwSignal::new(Some(0));

    let add_row = move |_| {
        let n = frames.with(Vec::len);
        frames.update(|fs| fs.push(SeqFrame::default()));
        open.set(Some(n));
    };

    // "Total · 50 frames · 1 h 12 min" over the valid rows.
    let totals = move || frames.with(|fs| {
        let n_frames: u32 = fs.iter().filter_map(SeqFrame::count_n).sum();
        let secs: f64 = fs.iter().filter_map(SeqFrame::duration_secs).sum();
        let s = tr();
        let unit = if n_frames == 1 { s.seq_frame_unit } else { s.seq_frames_unit };
        format!("{} \u{00b7} {n_frames} {unit} \u{00b7} {}", s.seq_total, fmt_duration(secs))
    });

    view! {
        <div class="flex flex-col gap-sp-2">
            // Cards are keyed by index: adding / removing a row mounts or
            // unmounts only the last card, and each card reads its row
            // reactively, so edits and reorders never rebuild the list.
            <For each=move || 0..frames.with(Vec::len) key=|i| *i let:idx>
                <JobCard idx=idx frames=frames open=open camera=camera filter_wheel=filter_wheel />
            </For>

            // Destination folder — applies to every job in this form.
            <label class="flex flex-col gap-[3px] mt-sp-1">
                <span class="text-text-blue text-xs uppercase tracking-[0.06em]">{move || tr().seq_dest_folder}</span>
                <input type="text"
                       class="input w-full min-w-0 font-mono"
                       placeholder="/home/user/Pictures"
                       prop:value=move || fits_dir.get()
                       on:input=move |ev| fits_dir.set(event_target_value(&ev)) />
            </label>

            // Sticky inside whatever scroll container hosts the editor, so
            // the totals and "Add exposure" stay reachable on long lists.
            <div class="sticky bottom-0 z-[1] flex flex-wrap items-center justify-between gap-sp-2 py-sp-2 \
                        border-t border-border-base bg-bg">
                <span class="min-w-0 font-mono text-sm text-text-muted">{totals}</span>
                <button type="button" class="btn btn-primary shrink-0" on:click=add_row>
                    {move || tr().imaging_add_exposure}
                </button>
            </div>
        </div>
    }
}
