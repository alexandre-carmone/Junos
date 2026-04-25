//! Rotary tab wheel — right-edge tab navigator.
//!
//! Collapsed: a small circular knob showing the active tab's abbreviation.
//! Expanded:  an arc of tab buttons on the left half of a 200 px disc, with
//!            the active tab snapped to the 9 o'clock position. Clicking a
//!            tab rotates the wheel so the chosen tab slides to the active
//!            slot, then commits via `ActiveTabCtx`. Mouse wheel cycles ±1.
//!
//! Auto-collapses 2 s after pointer leaves; re-expands on hover (mouse) or
//! tap on the knob (touch). The lang toggle lives in the wheel hub so the
//! bottom of the screen is fully clear.

use std::cell::Cell;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MouseEvent, PointerEvent, WheelEvent};

use crate::i18n::{Lang, t};
use crate::{ActiveTabCtx, Tab};

const TABS: [Tab; 8] = [
    Tab::Sky,
    Tab::Mount,
    Tab::Focus,
    Tab::Imaging,
    Tab::PolarAlign,
    Tab::Guide,
    Tab::Scheduler,
    Tab::Mosaic,
];

const N: usize = TABS.len();
const ARC_START_DEG: f32 = 90.0;          // top
const ARC_END_DEG: f32 = 270.0;           // bottom (going through left = 180°)
const RADIUS_PX: f32 = 78.0;
const BOX_PX: f32 = 200.0;
const KNOB_PX: f32 = 48.0;
const COLLAPSE_MS: i32 = 2000;

fn tab_index(t: Tab) -> usize {
    TABS.iter().position(|x| *x == t).unwrap_or(0)
}

fn base_angle(i: usize) -> f32 {
    let step = (ARC_END_DEG - ARC_START_DEG) / (N as f32 - 1.0);
    ARC_START_DEG + (i as f32) * step
}

fn tab_abbr(tab: Tab, s: &crate::i18n::Translations) -> &'static str {
    match tab {
        Tab::Sky        => s.tab_sky_abbr,
        Tab::Mount      => s.tab_mount_abbr,
        Tab::Focus      => s.tab_focus_abbr,
        Tab::Imaging    => s.tab_imaging_abbr,
        Tab::PolarAlign => s.tab_polar_abbr,
        Tab::Guide      => s.tab_guide_abbr,
        Tab::Scheduler  => s.tab_scheduler_abbr,
        Tab::Mosaic     => s.tab_mosaic_abbr,
    }
}

fn tab_title(tab: Tab, s: &crate::i18n::Translations) -> &'static str {
    match tab {
        Tab::Sky        => s.tab_sky,
        Tab::Mount      => s.tab_mount,
        Tab::Focus      => s.tab_focus,
        Tab::Imaging    => s.tab_imaging,
        Tab::PolarAlign => s.tab_polar_align,
        Tab::Guide      => s.tab_guide,
        Tab::Scheduler  => s.tab_scheduler,
        Tab::Mosaic     => s.tab_mosaic,
    }
}

#[component]
pub fn TabWheel() -> impl IntoView {
    let active = use_context::<ActiveTabCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(Tab::Sky));
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());

    let expanded = RwSignal::new(false);

    // Idle-collapse timer. We hold the active timeout id in a Cell so we can
    // clear it before arming a new one. Closures are stored in Rc so the
    // collapse logic is reusable from pointer/touch handlers.
    let timeout_id: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));

    let clear_timer = {
        let timeout_id = Rc::clone(&timeout_id);
        Rc::new(move || {
            if let Some(id) = timeout_id.take() {
                if let Some(w) = web_sys::window() {
                    w.clear_timeout_with_handle(id);
                }
            }
        })
    };

    let arm_timer = {
        let timeout_id = Rc::clone(&timeout_id);
        let clear_timer = Rc::clone(&clear_timer);
        Rc::new(move || {
            clear_timer();
            let cb = Closure::<dyn FnMut()>::new(move || {
                expanded.set(false);
            });
            if let Some(w) = web_sys::window() {
                if let Ok(id) = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                    cb.as_ref().unchecked_ref(),
                    COLLAPSE_MS,
                ) {
                    timeout_id.set(Some(id));
                }
            }
            cb.forget();
        })
    };

    // Rotation derived from the active tab so it always sits at 9 o'clock (180°).
    let rotation = Signal::derive(move || 180.0_f32 - base_angle(tab_index(active.get())));

    let on_pointer_enter = {
        let clear_timer = Rc::clone(&clear_timer);
        move |_ev: PointerEvent| {
            expanded.set(true);
            clear_timer();
        }
    };
    let on_pointer_leave = {
        let arm_timer = Rc::clone(&arm_timer);
        move |_ev: PointerEvent| arm_timer()
    };

    let on_knob_click = {
        let clear_timer = Rc::clone(&clear_timer);
        let arm_timer = Rc::clone(&arm_timer);
        move |_ev: MouseEvent| {
            expanded.update(|v| *v = !*v);
            if expanded.get_untracked() {
                clear_timer();
            } else {
                arm_timer();
            }
        }
    };

    let on_wheel = {
        let arm_timer = Rc::clone(&arm_timer);
        move |ev: WheelEvent| {
            ev.prevent_default();
            let delta = ev.delta_y();
            if delta == 0.0 { return; }
            let cur = tab_index(active.get_untracked());
            let next = if delta > 0.0 {
                (cur + 1) % N
            } else {
                (cur + N - 1) % N
            };
            active.set(TABS[next]);
            arm_timer();
        }
    };

    // Container — uses pointer events so hover works on both mouse and touch.
    view! {
        <div
            style=move || format!(
                "position:absolute; right:12px; top:50%; transform:translateY(-50%); \
                 z-index:60; pointer-events:auto; \
                 width:{}px; height:{}px; \
                 display:flex; align-items:center; justify-content:center;",
                BOX_PX, BOX_PX,
            )
            on:pointerenter=on_pointer_enter
            on:pointerleave=on_pointer_leave
            on:wheel=on_wheel
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // Rotating arc layer — visible only when expanded.
            <div
                style=move || format!(
                    "position:absolute; left:0; top:0; width:{box_}px; height:{box_}px; \
                     border-radius:50%; \
                     background:rgba(6,6,15,0.55); border:1px solid #222; \
                     transform:rotate({rot}deg); transition:transform 0.25s ease, opacity 0.15s; \
                     opacity:{op}; pointer-events:{pe}; \
                     touch-action:none;",
                    box_ = BOX_PX,
                    rot  = rotation.get(),
                    op   = if expanded.get() { 1.0 } else { 0.0 },
                    pe   = if expanded.get() { "auto" } else { "none" },
                )
            >
                {(0..N).map(|i| {
                    let tab = TABS[i];
                    let ang_rad = base_angle(i).to_radians();
                    let cx = BOX_PX * 0.5 + RADIUS_PX * ang_rad.cos();
                    let cy = BOX_PX * 0.5 + RADIUS_PX * ang_rad.sin();
                    let btn_w = 44.0_f32;
                    let btn_h = 28.0_f32;
                    let arm_timer = Rc::clone(&arm_timer);
                    let style = move || {
                        let on = active.get() == tab;
                        let (bg, border, color) = if on {
                            ("rgba(40,60,110,0.95)", "#88aaff", "#cfe0ff")
                        } else {
                            ("rgba(12,14,24,0.9)", "#2a2a35", "#88aaff")
                        };
                        format!(
                            "position:absolute; left:{l}px; top:{t}px; \
                             width:{w}px; height:{h}px; \
                             transform:translate(-50%,-50%) rotate({cr}deg); \
                             border-radius:6px; border:1px solid {border}; \
                             background:{bg}; color:{color}; \
                             font:600 11px/1 ui-monospace,monospace; letter-spacing:0.05em; \
                             cursor:pointer; touch-action:manipulation; \
                             -webkit-tap-highlight-color:transparent; \
                             padding:0; \
                             transition:background 0.15s, border-color 0.15s;",
                            l = cx, t = cy, w = btn_w, h = btn_h,
                            cr = -rotation.get(),
                            bg = bg, border = border, color = color,
                        )
                    };
                    view! {
                        <button
                            style=style
                            title=move || tab_title(tab, &tr())
                            on:click=move |ev: MouseEvent| {
                                ev.stop_propagation();
                                active.set(tab);
                                arm_timer();
                            }
                        >
                            {move || tab_abbr(tab, &tr())}
                        </button>
                    }
                }).collect_view()}
            </div>

            // Active-slot indicator — small notch at 9 o'clock, outside the
            // rotating layer so it stays put while the wheel turns.
            <div
                style=move || format!(
                    "position:absolute; left:{l}px; top:50%; transform:translateY(-50%); \
                     width:6px; height:18px; border-radius:2px; \
                     background:#88aaff; box-shadow:0 0 6px rgba(136,170,255,0.6); \
                     opacity:{op}; pointer-events:none;",
                    l = (BOX_PX * 0.5 - RADIUS_PX - 12.0).max(0.0),
                    op = if expanded.get() { 1.0 } else { 0.0 },
                )
            />

            // Centre knob — always visible. Shows the active tab's
            // abbreviation; tapping toggles expanded state.
            <button
                style=move || format!(
                    "position:absolute; left:50%; top:50%; \
                     transform:translate(-50%,-50%); \
                     width:{k}px; height:{k}px; \
                     border-radius:50%; \
                     border:2px solid #88aaff; background:rgba(12,14,24,0.92); \
                     color:#cfe0ff; \
                     font:700 12px/1 ui-monospace,monospace; letter-spacing:0.06em; \
                     cursor:pointer; touch-action:manipulation; \
                     -webkit-tap-highlight-color:transparent; \
                     box-shadow:0 0 10px rgba(0,0,0,0.5); \
                     display:flex; align-items:center; justify-content:center;",
                    k = KNOB_PX,
                )
                title=move || tab_title(active.get(), &tr())
                on:click=on_knob_click
            >
                {move || tab_abbr(active.get(), &tr())}
            </button>

            // Lang toggle — small chip just below the knob, only visible
            // when expanded.
            <button
                style=move || format!(
                    "position:absolute; left:50%; top:calc(50% + {off}px); \
                     transform:translate(-50%, 0); \
                     min-width:32px; height:22px; padding:0 8px; border-radius:11px; \
                     border:1px solid #88aaff; background:rgba(12,14,24,0.9); \
                     color:#88aaff; font:600 10px/1 ui-monospace,monospace; \
                     cursor:pointer; touch-action:manipulation; \
                     -webkit-tap-highlight-color:transparent; \
                     letter-spacing:0.05em; \
                     opacity:{op}; pointer-events:{pe}; \
                     transition:opacity 0.15s;",
                    off = KNOB_PX * 0.5 + 8.0,
                    op  = if expanded.get() { 1.0 } else { 0.0 },
                    pe  = if expanded.get() { "auto" } else { "none" },
                )
                title=move || lang.get().toggle().label()
                on:click=move |ev: MouseEvent| {
                    ev.stop_propagation();
                    lang.update(|l| *l = l.toggle());
                }
            >
                {move || lang.get().label()}
            </button>
        </div>
    }
}
