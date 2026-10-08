//! One job card of the sequence editor.
//!
//! Collapsed, the card is a single tappable summary line
//! ("Light · Ha   120 s × 10   20 min"). Open, it shows every field of the
//! row plus the move / duplicate / delete actions. Every field reads the row
//! reactively from `frames` and writes back on input, so typing never
//! rebuilds the card (and never steals focus).

use leptos::prelude::*;

use crate::compat::{CameraSnapshot, FilterWheelSnapshot};
use crate::components::frame_type::{frame_type_options, frame_type_pills, frame_type_visual};
use crate::dom::event_target_value;
use crate::i18n::{t, Lang, Translations};

use super::model::{fmt_duration, Dither, SeqFrame};

/// One-tap exposure presets in seconds. Longer than the one-shot panel's
/// focus-oriented list: these are typical sub lengths.
const SEQ_EXPOSURE_PRESETS: &[u32] = &[1, 5, 30, 60, 120, 180, 300, 600];
/// Square binning factors offered by the picker.
const BIN_FACTORS: &[u32] = &[1, 2, 3, 4];

const INPUT: &str = "input w-full min-w-0 font-mono";
pub(super) const LABEL: &str = "text-text-blue text-xs uppercase tracking-[0.06em] truncate";
/// Segmented-control pill; add `class:btn--active` for the selected one.
pub(super) const PILL: &str = "btn btn--sm btn-ghost font-mono max-[479px]:h-9";
/// Field grids size by the space the editor actually gets (it is embedded
/// in containers of very different widths), not by the viewport.
const GRID_2: &str = "grid grid-cols-[repeat(auto-fit,minmax(140px,1fr))] gap-sp-3";
const GRID_4: &str = "grid grid-cols-[repeat(auto-fit,minmax(120px,1fr))] gap-sp-3";

/// Apply `edit` to row `idx`, if it still exists.
fn update_row(frames: RwSignal<Vec<SeqFrame>>, idx: usize, edit: impl FnOnce(&mut SeqFrame)) {
    frames.update(|fs| if let Some(f) = fs.get_mut(idx) { edit(f) });
}

/// Label stacked above its editor, so the editor gets the full column width.
fn labeled(label: impl Fn() -> &'static str + Send + Sync + 'static, editor: impl IntoView) -> impl IntoView {
    view! {
        <label class="flex flex-col gap-[3px] min-w-0">
            <span class=LABEL>{label}</span>
            {editor}
        </label>
    }
}

/// Free-text input. `type="text"` + `inputmode` gives phones a number pad
/// without the browser rewriting half-typed values the way `type="number"`
/// does; `invalid` paints the border red.
fn text_input(
    inputmode: &'static str,
    value: impl Fn() -> String + Send + Sync + 'static,
    set: impl Fn(String) + Send + Sync + 'static,
    invalid: impl Fn() -> bool + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <input type="text" inputmode=inputmode autocomplete="off"
               class=move || if invalid() { format!("{INPUT} !border-state-err") } else { INPUT.to_string() }
               prop:value=value
               on:input=move |ev| set(event_target_value(&ev)) />
    }
}

/// `<select>` over device-reported options, falling back to a free-text
/// input while the device hasn't reported any yet (camera not streaming its
/// switch property). A current value missing from the list is kept as a
/// disabled placeholder option instead of being silently replaced.
///
/// `none` labels a leading entry for the empty value — the job leaves that
/// setting to the camera. Without it an empty value would show the first
/// option as picked while the job carries nothing. Fields that always need
/// a value pass `None`.
fn choice_input(
    options: impl Fn() -> Vec<String> + Send + Sync + 'static,
    value: impl Fn() -> String + Copy + Send + Sync + 'static,
    set: impl Fn(String) + Copy + Send + Sync + 'static,
    none: Option<Signal<String>>,
) -> impl IntoView {
    // Memo: snapshots update often (temperature, state…); only rebuild the
    // <select> when the option list itself changes.
    let options = Memo::new(move |_| options());
    move || {
        let opts = options.get();
        if opts.is_empty() {
            return text_input("text", value, set, || false).into_any();
        }
        let known = opts.clone();
        let unknown = move || {
            let cur = value();
            (!cur.is_empty() && !known.contains(&cur)).then(|| {
                let label = cur.clone();
                view! { <option value=cur disabled=true prop:selected=true>{label}</option> }
            })
        };
        // `selected` is set per option (not `prop:value` on the select): the
        // options don't exist yet when the select's own props are applied.
        view! {
            <select class=INPUT on:change=move |ev| set(event_target_value(&ev))>
                {none.map(|label| view! {
                    <option value="" prop:selected=move || value().is_empty()>{label}</option>
                })}
                {unknown}
                {opts.into_iter().map(|o| {
                    let (name, label) = (o.clone(), o.clone());
                    view! { <option value=o prop:selected=move || value() == name>{label}</option> }
                }).collect::<Vec<_>>()}
            </select>
        }.into_any()
    }
}

#[component]
pub fn JobCard(
    idx: usize,
    frames: RwSignal<Vec<SeqFrame>>,
    /// Index of the open card (accordion: at most one open).
    open: RwSignal<Option<usize>>,
    camera: Signal<CameraSnapshot>,
    filter_wheel: Signal<FilterWheelSnapshot>,
) -> impl IntoView {
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    // Reactive copy of this row. `unwrap_or_default` covers the instant
    // between a row being removed and its card being unmounted.
    let frame = move || frames.with(|fs| fs.get(idx).cloned().unwrap_or_default());
    let rows = move || frames.with(Vec::len);
    // Memo: only the cards whose open state actually changes re-render.
    let is_open = Memo::new(move |_| open.get() == Some(idx));

    // ── Header: summary line, toggles the card ────────────────────────────
    let title = move || {
        let f = frame();
        if f.filter.is_empty() { f.frame_type } else { format!("{} \u{00b7} {}", f.frame_type, f.filter) }
    };
    let exp_count = move || {
        let f = frame();
        format!("{} s \u{00d7} {}", f.exposure.trim(), f.count.trim())
    };
    let duration = move || frame().duration_secs().map(fmt_duration).unwrap_or_else(|| "\u{2014}".into());
    // Left accent bar: frame-type color, red while the row is invalid.
    let accent = move || {
        let f = frame();
        let color = if f.is_valid() { frame_type_visual(&f.frame_type).1 } else { "var(--state-err)" };
        format!("--card-accent:{color}")
    };

    let header = view! {
        <button type="button"
                class="w-full min-h-11 flex items-center gap-sp-2 px-sp-3 text-left bg-transparent border-0 text-text \
                       hover:bg-bg-elev-2 transition-colors"
                aria-expanded=move || is_open.get().to_string()
                on:click=move |_| open.update(|o| *o = if *o == Some(idx) { None } else { Some(idx) })>
            <span class="inline-flex shrink-0"
                  style=move || format!("color:{}", frame_type_visual(&frame().frame_type).1)
                  inner_html=move || frame_type_visual(&frame().frame_type).0 />
            <span class="flex-1 min-w-0 truncate text-md text-text">{title}</span>
            <span class="font-mono text-sm text-text-muted whitespace-nowrap">{exp_count}</span>
            <span class="font-mono text-sm text-text-dim whitespace-nowrap min-w-[56px] text-right">{duration}</span>
            <span class="text-text-blue">{move || if is_open.get() { "\u{25B4}" } else { "\u{25BE}" }}</span>
        </button>
    };

    // ── Body sections ──────────────────────────────────────────────────────
    // Each section is a builder closure: the body is rebuilt every time the
    // card opens, and stays mounted (no rebuild) while it is being edited.
    let frame_type = move || frame_type_pills(
        move || frame_type_options(camera.with(|c| c.frame_type_options.clone())),
        move || frame().frame_type,
        move |v| update_row(frames, idx, |f| f.frame_type = v),
    );

    let no_change = Signal::derive(move || tr().seq_no_change.to_string());
    let filter = move || labeled(move || tr().field_filter, choice_input(
        move || filter_wheel.with(|fw| fw.filter_names.clone()),
        move || frame().filter,
        move |v| update_row(frames, idx, |f| f.filter = v),
        Some(no_change),
    ));

    let exposure = move || labeled(move || tr().field_exposure_s, text_input(
        "decimal",
        move || frame().exposure,
        move |v| update_row(frames, idx, |f| f.exposure = v),
        move || frame().exposure_secs().is_none(),
    ));

    let step = move |delta: i64| update_row(frames, idx, |f| {
        let n = f.count_n().map_or(0, i64::from) + delta;
        f.count = n.max(1).to_string();
    });
    let count = move || labeled(move || tr().field_count, view! {
        <div class="flex gap-[4px]">
            <button type="button" class="btn-icon shrink-0"
                    disabled=move || { frame().count_n().map_or(true, |n| n <= 1) }
                    on:click=move |_| step(-1)>"\u{2212}"</button>
            {text_input(
                "numeric",
                move || frame().count,
                move |v| update_row(frames, idx, |f| f.count = v),
                move || frame().count_n().is_none(),
            )}
            <button type="button" class="btn-icon shrink-0" on:click=move |_| step(1)>"+"</button>
        </div>
    });

    let presets = move || view! {
        <div class="flex flex-wrap gap-[6px]">
            {SEQ_EXPOSURE_PRESETS.iter().map(|&p| view! {
                <button type="button" class=PILL
                        class:btn--active=move || frame().exposure_secs() == Some(f64::from(p))
                        on:click=move |_| update_row(frames, idx, |f| f.exposure = p.to_string())>
                    {format!("{p}s")}
                </button>
            }).collect::<Vec<_>>()}
        </div>
    };

    // Flat calibration — always visible on Flat rows, not hidden under "More".
    let adu_invalid = move || frame().flat_adu_target().is_none();
    let is_flat = Memo::new(move |_| frame().frame_type == "Flat");
    let flat = move || is_flat.get().then(|| view! {
        <div class="flex flex-col gap-sp-2 rounded-md border border-border-base p-sp-2">
            <span class=LABEL>{move || tr().seq_flat_duration}</span>
            <div class="flex gap-[4px]">
                <button type="button" class=PILL
                        class:btn--active=move || !frame().flat_adu_mode
                        on:click=move |_| update_row(frames, idx, |f| f.flat_adu_mode = false)>
                    {move || tr().seq_flat_manual}
                </button>
                <button type="button" class=PILL
                        class:btn--active=move || frame().flat_adu_mode
                        on:click=move |_| update_row(frames, idx, |f| f.flat_adu_mode = true)>
                    "ADU"
                </button>
            </div>
            <Show when=move || frame().flat_adu_mode>
                <div class=GRID_2>
                    {labeled(move || tr().seq_target_adu, text_input(
                        "numeric",
                        move || frame().flat_adu,
                        move |v| update_row(frames, idx, |f| f.flat_adu = v),
                        adu_invalid,
                    ))}
                    {labeled(move || tr().seq_adu_tolerance, text_input(
                        "numeric",
                        move || frame().flat_tolerance,
                        move |v| update_row(frames, idx, |f| f.flat_tolerance = v),
                        adu_invalid,
                    ))}
                </div>
            </Show>
        </div>
    });

    // Dithering — Light rows only, the only frames KStars dithers.
    let is_light = Memo::new(move |_| frame().frame_type == "Light");
    let dither_pill = move |mode: Dither, label: fn(&Translations) -> &'static str| view! {
        <button type="button" class=PILL
                class:btn--active=move || frame().dither == mode
                on:click=move |_| update_row(frames, idx, |f| f.dither = mode)>
            {move || label(tr())}
        </button>
    };
    let dither = move || is_light.get().then(|| view! {
        <div class="flex flex-col gap-[3px]">
            <span class=LABEL>{move || tr().seq_dither}</span>
            <div class="flex flex-wrap items-center gap-[4px]">
                {dither_pill(Dither::Guide, |s| s.seq_dither_guide)}
                {dither_pill(Dither::Every, |s| s.seq_dither_every)}
                {dither_pill(Dither::Off, |s| s.seq_dither_off)}
                <Show when=move || frame().dither == Dither::Every>
                    <span class="flex items-center gap-[6px] ml-sp-1">
                        <span class="w-16">{text_input(
                            "numeric",
                            move || frame().dither_every,
                            move |v| update_row(frames, idx, |f| f.dither_every = v),
                            move || frame().dither_per_job().is_err(),
                        )}</span>
                        <span class="text-sm text-text-muted">{move || tr().seq_frames_unit}</span>
                    </span>
                </Show>
            </div>
        </div>
    });

    // "More": rarely changed settings, with their current values summarised
    // on the closed line so non-defaults are visible without opening it.
    let more_summary = move || {
        let (s, f) = (tr(), frame());
        let mut parts = vec![format!("{} {}", s.bin, f.bin_label())];
        for (label, value) in [(s.field_gain, &f.gain), (s.field_offset, &f.offset), (s.field_iso, &f.iso)] {
            if !value.is_empty() { parts.push(format!("{label} {value}")); }
        }
        if !f.format.is_empty() { parts.push(f.format.clone()); }
        if !matches!(f.delay.trim(), "" | "0") { parts.push(format!("{} {}", s.field_delay_s, f.delay)); }
        parts.join(" \u{00b7} ")
    };
    let binning = move || view! {
        <div class="col-span-full flex flex-col gap-[3px]">
            <span class=LABEL>{move || tr().seq_binning}</span>
            <div class="flex gap-[4px]">
                {BIN_FACTORS.iter().map(|&n| view! {
                    <button type="button" class=PILL
                            class:btn--active=move || frame().bin_label() == format!("{n}\u{00d7}{n}")
                            on:click=move |_| update_row(frames, idx, |f| f.set_bin(n))>
                        {format!("{n}\u{00d7}{n}")}
                    </button>
                }).collect::<Vec<_>>()}
            </div>
        </div>
    };
    let show_iso = move || !camera.with(|c| c.iso_options.is_empty()) || !frame().iso.is_empty();
    // An unset format is the camera's current one: name it.
    let format_current = Signal::derive(move || match camera.with(|c| c.capture_format.clone()) {
        Some(cur) => format!("{} ({cur})", tr().seq_no_change),
        None => tr().seq_no_change.to_string(),
    });
    let more = move || view! {
        <details class="group rounded-md border border-border-base">
            <summary class="list-none cursor-pointer flex items-center gap-sp-2 min-h-9 px-sp-2 text-sm select-none [&::-webkit-details-marker]:hidden">
                <span class="text-text-blue transition-transform group-open:rotate-90">"\u{25B8}"</span>
                <span class="text-text-blue">{move || tr().seq_more}</span>
                <span class="flex-1 min-w-0 truncate font-mono text-text-muted">{more_summary}</span>
            </summary>
            <div class=format!("{GRID_4} px-sp-2 pb-sp-2")>
                {binning()}
                {labeled(move || tr().field_gain, text_input(
                    "decimal",
                    move || frame().gain,
                    move |v| update_row(frames, idx, |f| f.gain = v),
                    move || frame().gain_value().is_err(),
                ))}
                {labeled(move || tr().field_offset, text_input(
                    "decimal",
                    move || frame().offset,
                    move |v| update_row(frames, idx, |f| f.offset = v),
                    move || frame().offset_value().is_err(),
                ))}
                <Show when=show_iso>
                    {labeled(move || tr().field_iso, choice_input(
                        move || camera.with(|c| c.iso_options.clone()),
                        move || frame().iso,
                        move |v| update_row(frames, idx, |f| f.iso = v),
                        Some(no_change),
                    ))}
                </Show>
                {labeled(move || tr().field_format, choice_input(
                    move || camera.with(|c| c.capture_format_options.clone()),
                    move || frame().format,
                    move |v| update_row(frames, idx, |f| f.format = v),
                    Some(format_current),
                ))}
                {labeled(move || tr().field_encoding, choice_input(
                    move || camera.with(|c| c.transfer_format_options.clone()),
                    move || frame().encoding,
                    move |v| update_row(frames, idx, |f| f.encoding = v),
                    None,
                ))}
                {labeled(move || tr().field_delay_s, text_input(
                    "numeric",
                    move || frame().delay,
                    move |v| update_row(frames, idx, |f| f.delay = v),
                    move || frame().delay_secs().is_err(),
                ))}
            </div>
        </details>
    };

    // ── Row actions ────────────────────────────────────────────────────────
    // Note: closures with `<` / `>` comparisons in `view!` attributes are
    // wrapped in braces — unbraced, the macro reads `>` as the end of the tag.
    let move_to = move |to: usize| {
        if to >= rows() { return; }
        frames.update(|fs| fs.swap(idx, to));
        open.set(Some(to));
    };
    let duplicate = move |_| {
        frames.update(|fs| if let Some(f) = fs.get(idx).cloned() { fs.insert(idx + 1, f) });
        open.set(Some(idx + 1));
    };
    let delete = move |_| {
        frames.update(|fs| if fs.len() > 1 { fs.remove(idx); });
        open.set(None);
    };
    let actions = move || view! {
        <div class="flex items-center gap-sp-2">
            <button type="button" class="btn-icon" title=move || tr().seq_move_up
                    disabled=move || idx == 0
                    on:click=move |_| if let Some(to) = idx.checked_sub(1) { move_to(to) }>"\u{2191}"</button>
            <button type="button" class="btn-icon" title=move || tr().seq_move_down
                    disabled=move || { idx + 1 >= rows() }
                    on:click=move |_| move_to(idx + 1)>"\u{2193}"</button>
            <span class="flex-1"></span>
            <button type="button" class="btn btn-ghost" on:click=duplicate>{move || tr().seq_duplicate}</button>
            <button type="button" class="btn btn-danger"
                    disabled=move || { rows() <= 1 }
                    on:click=delete>{move || tr().seq_delete}</button>
        </div>
    };

    view! {
        <div class="rounded-md border border-border-base border-l-[3px] border-l-[color:var(--card-accent)] bg-bg-elev-1 overflow-hidden"
             style=accent>
            {header}
            <Show when=move || is_open.get()>
                <div class="flex flex-col gap-sp-3 px-sp-3 pb-sp-3 pt-sp-2 border-t border-border-base">
                    {frame_type()}
                    {filter()}
                    <div class=GRID_2>{exposure()}{count()}</div>
                    {presets()}
                    {dither}
                    {flat}
                    {more()}
                    {actions()}
                </div>
            </Show>
        </div>
    }
}
