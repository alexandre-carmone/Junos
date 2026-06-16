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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{MouseEvent, PointerEvent, WheelEvent};

#[derive(Clone, Copy)]
struct DragState {
    cx: f64,
    cy: f64,
    start_angle_deg: f32,
    anchor_idx: usize,
    moved: bool,
}

use crate::i18n::{Lang, t};
use crate::{ActiveTabCtx, Tab, TabLabelsCtx};

use crate::components::tab_wheel_icons::tab_icon;

pub const TABS: [Tab; 11] = [
    Tab::Profiles,
    Tab::Sky,
    Tab::Mount,
    Tab::Focus,
    Tab::Imaging,
    Tab::Files,
    Tab::PolarAlign,
    Tab::Guide,
    Tab::Scheduler,
    Tab::Mosaic,
    Tab::FlatCal,
];

const N: usize = TABS.len();
const ARC_START_DEG: f32 = 90.0;          // top
const ARC_END_DEG: f32 = 270.0;           // bottom (going through left = 180°)
const RADIUS_PX: f32 = 115.0;
const BOX_PX: f32 = 290.0;
const KNOB_PX: f32 = 68.0;
const COLLAPSE_MS: i32 = 1000;
// Negative `right` offset so the wheel's center sits just inside the right
// edge — the knob hugs the border and the arc fans into the screen.
const RIGHT_OFFSET_PX: f32 = -(BOX_PX * 0.5) + KNOB_PX * 0.5 + 4.0;

fn tab_index(t: Tab) -> usize {
    TABS.iter().position(|x| *x == t).unwrap_or(0)
}

fn base_angle(i: usize) -> f32 {
    let step = (ARC_END_DEG - ARC_START_DEG) / (N as f32 - 1.0);
    ARC_START_DEG + (i as f32) * step
}

pub fn tab_title(tab: Tab, s: &crate::i18n::Translations) -> &'static str {
    match tab {
        Tab::Sky        => s.tab_sky,
        Tab::Mount      => s.tab_mount,
        Tab::Focus      => s.tab_focus,
        Tab::Imaging    => s.tab_imaging,
        Tab::Files      => s.tab_files,
        Tab::PolarAlign => s.tab_polar_align,
        Tab::Guide      => s.tab_guide,
        Tab::Scheduler  => s.tab_scheduler,
        Tab::Mosaic     => s.tab_mosaic,
        Tab::FlatCal    => s.tab_flat_cal,
        Tab::Profiles   => s.tab_profiles,
    }
}

#[component]
pub fn TabWheel() -> impl IntoView {
    let active = use_context::<ActiveTabCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(Tab::Sky));
    let lang = use_context::<RwSignal<Lang>>().unwrap_or_else(|| RwSignal::new(Lang::En));
    let tr = move || t(lang.get());
    let show_labels = use_context::<TabLabelsCtx>()
        .map(|c| c.0)
        .unwrap_or_else(|| RwSignal::new(false));

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

    // Free-rotation offset applied on top of the active tab's snap angle
    // while the user is dragging the disc. Reset to 0 on release; the snap
    // animates because `active` updates and the CSS transition kicks back in.
    let drag_offset_deg = RwSignal::new(0.0_f32);
    let dragging = RwSignal::new(false);
    // Brief "you've hit the end" jitter when a drag is clamped at a boundary.
    let bumped = RwSignal::new(false);

    // Rotation derived from the active tab so it always sits at 9 o'clock
    // (180°), plus the live drag offset.
    let rotation = Signal::derive(move || {
        180.0_f32 - base_angle(tab_index(active.get())) + drag_offset_deg.get()
    });

    // Hover-to-expand for mouse only. On touch, `pointerenter` fires at the
    // start of a tap and `pointerleave` fires immediately on touchend — using
    // those would race the click handler and leave the wheel collapsed after
    // a single tap. Touch users expand via the knob click.
    let on_pointer_enter = {
        let clear_timer = Rc::clone(&clear_timer);
        move |ev: PointerEvent| {
            if ev.pointer_type() != "mouse" { return; }
            expanded.set(true);
            clear_timer();
        }
    };
    let on_pointer_leave = {
        let arm_timer = Rc::clone(&arm_timer);
        move |ev: PointerEvent| {
            if ev.pointer_type() != "mouse" { return; }
            arm_timer();
        }
    };

    // Any pointer-down inside the widget keeps it alive; pointer-up re-arms
    // the idle timer so touch interactions can chain (tap knob → tap tab).
    let on_pointer_down = {
        let clear_timer = Rc::clone(&clear_timer);
        move |_ev: PointerEvent| clear_timer()
    };
    let on_pointer_up = {
        let arm_timer = Rc::clone(&arm_timer);
        move |_ev: PointerEvent| arm_timer()
    };

    // ── Wheel: cycle ±1 tab. Trackpads emit many small deltas per swipe so
    // we accumulate until a notch threshold is crossed; mouse-wheel notches
    // typically deliver |delta_y| ≈ 100 in one event and trip immediately.
    let wheel_accum: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    let on_wheel: Rc<dyn Fn(WheelEvent)> = {
        let clear_timer = Rc::clone(&clear_timer);
        let arm_timer = Rc::clone(&arm_timer);
        let wheel_accum = Rc::clone(&wheel_accum);
        Rc::new(move |ev: WheelEvent| {
            ev.prevent_default();
            ev.stop_propagation();
            const THRESHOLD: f64 = 50.0;
            let acc = wheel_accum.get() + ev.delta_y();
            if acc.abs() < THRESHOLD {
                wheel_accum.set(acc);
                return;
            }
            wheel_accum.set(0.0);
            let cur = tab_index(active.get_untracked());
            let new_idx = if acc > 0.0 {
                (cur + 1).min(N - 1)
            } else {
                cur.saturating_sub(1)
            };
            if new_idx != cur {
                active.set(TABS[new_idx]);
            }
            // Pop the wheel open briefly so the user sees what changed.
            expanded.set(true);
            clear_timer();
            arm_timer();
        })
    };

    let on_knob_click = {
        let clear_timer = Rc::clone(&clear_timer);
        let arm_timer = Rc::clone(&arm_timer);
        move |ev: MouseEvent| {
            ev.stop_propagation();
            expanded.update(|v| *v = !*v);
            if expanded.get_untracked() {
                clear_timer();
            } else {
                arm_timer();
            }
        }
    };

// ── Drag-to-rotate ────────────────────────────────────────────────────
    // Touch users can't fire `wheel` events. While the user drags the disc,
    // the wheel follows the finger continuously (`drag_offset_deg`); on
    // release we snap `active` to the tab whose final position is closest
    // to the 9 o'clock indicator. The snap animates because the CSS
    // transition is only enabled when `dragging` is false.
    let disc_ref: NodeRef<Div> = NodeRef::new();
    let drag: Rc<RefCell<Option<DragState>>> = Rc::new(RefCell::new(None));
    let was_dragging: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Movement threshold (degrees) below which we treat the gesture as a tap.
    const DRAG_DEAD_ZONE_DEG: f32 = 4.0;

    let on_disc_pointer_down = {
        let drag = Rc::clone(&drag);
        let clear_timer = Rc::clone(&clear_timer);
        move |ev: PointerEvent| {
            if let Some(el) = disc_ref.get() {
                let rect = el.get_bounding_client_rect();
                let cx = rect.left() + rect.width() * 0.5;
                let cy = rect.top() + rect.height() * 0.5;
                let dx = ev.client_x() as f64 - cx;
                let dy = ev.client_y() as f64 - cy;
                let start_angle = (dy.atan2(dx).to_degrees()) as f32;
                *drag.borrow_mut() = Some(DragState {
                    cx, cy, start_angle_deg: start_angle,
                    anchor_idx: tab_index(active.get_untracked()),
                    moved: false,
                });
                let _ = el.set_pointer_capture(ev.pointer_id());
            }
            clear_timer();
        }
    };

    let on_disc_pointer_move = {
        let drag = Rc::clone(&drag);
        move |ev: PointerEvent| {
            let mut d = drag.borrow_mut();
            let st = match d.as_mut() { Some(s) => s, None => return };
            let dx = ev.client_x() as f64 - st.cx;
            let dy = ev.client_y() as f64 - st.cy;
            let cur_angle = (dy.atan2(dx).to_degrees()) as f32;
            let mut delta = cur_angle - st.start_angle_deg;
            while delta > 180.0  { delta -= 360.0; }
            while delta <= -180.0 { delta += 360.0; }
            // Dead zone — ignore tiny finger jitter so a tap doesn't drift
            // the wheel a fraction of a degree before the click fires.
            if !st.moved && delta.abs() < DRAG_DEAD_ZONE_DEG { return; }
            if !st.moved {
                st.moved = true;
                dragging.set(true);
            }
            // Clamp so the disc can't rotate past the first or last tab —
            // otherwise the empty (un-buttoned) right half of the circle
            // sweeps into view at the top or bottom of the half-disc.
            let step_deg = (ARC_END_DEG - ARC_START_DEG) / (N as f32 - 1.0);
            let anchor = st.anchor_idx as f32;
            // Sign mirrors the release math: positive offset → previous tab.
            let min_off = -(N as f32 - 1.0 - anchor) * step_deg;
            let max_off = anchor * step_deg;
            let clamped = delta.clamp(min_off, max_off);
            if clamped != delta && !bumped.get_untracked() {
                bumped.set(true);
                let cb = Closure::<dyn FnMut()>::new(move || bumped.set(false));
                if let Some(w) = web_sys::window() {
                    let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(
                        cb.as_ref().unchecked_ref(), 140,
                    );
                }
                cb.forget();
            }
            drag_offset_deg.set(clamped);
        }
    };

    let make_on_disc_pointer_up = || {
        let drag = Rc::clone(&drag);
        let was_dragging = Rc::clone(&was_dragging);
        let arm_timer = Rc::clone(&arm_timer);
        move |_ev: PointerEvent| {
            let st = drag.borrow_mut().take();
            if let Some(st) = st {
                if st.moved {
                    // Snap: total signed rotation since drag started =
                    // drag_offset_deg. Convert that to a tab-step shift.
                    let step_deg = (ARC_END_DEG - ARC_START_DEG) / (N as f32 - 1.0);
                    // Sign flip: positive `drag_offset_deg` rotates the disc
                    // clockwise, which brings the *previous* tab into the
                    // 9 o'clock slot (since `base_angle` increases clockwise).
                    let steps = (drag_offset_deg.get_untracked() / step_deg).round() as i32;
                    let new_idx = (st.anchor_idx as i32 - steps)
                        .clamp(0, N as i32 - 1) as usize;
                    active.set(TABS[new_idx]);
                    was_dragging.set(true);
                }
            }
            // Reset offset *after* `dragging` flips so the CSS transition
            // animates the snap from the current finger position to the
            // chosen tab's slot.
            dragging.set(false);
            drag_offset_deg.set(0.0);
            arm_timer();
        }
    };
    let on_disc_pointer_up = make_on_disc_pointer_up();
    let on_disc_pointer_cancel = make_on_disc_pointer_up();

    // Container — uses pointer events so hover works on both mouse and touch.
    // Tailwind utilities own static layout/colours; the dynamic transform /
    // opacity for the rotating disc is passed via CSS custom properties
    // (--tw-rot, --tw-bx) and class toggles.
    view! {
        <div
            class="absolute right-[-107px] top-1/2 -translate-y-1/2 z-[60] pointer-events-none w-[290px] h-[290px] flex items-center justify-center md:hidden"
            on:pointerenter=on_pointer_enter
            on:pointerleave=on_pointer_leave
            on:pointerdown=on_pointer_down
            on:pointerup=on_pointer_up
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // Rotating arc layer — visible only when expanded. The bouncy
            // spring transition is swapped for a tight 80 ms ease-out while
            // the user drags so the disc tracks the finger 1:1.
            <div
                node_ref=disc_ref
                class=move || {
                    let base = "absolute left-0 top-0 w-[290px] h-[290px] rounded-full \
                                border border-glass-border backdrop-blur-glass shadow-3 touch-none \
                                bg-[radial-gradient(circle_at_30%_50%,rgba(28,34,60,0.55),rgba(8,10,20,0.45))] \
                                [transform:translateX(var(--tw-bx,0px))_rotate(var(--tw-rot,0deg))]";
                    let visibility = if expanded.get() {
                        "opacity-100 pointer-events-auto"
                    } else {
                        "opacity-0 pointer-events-none"
                    };
                    let transition = if dragging.get() {
                        "transition-[transform,opacity] duration-[80ms] ease-out"
                    } else {
                        "transition-[transform,opacity] duration-[320ms] ease-[cubic-bezier(0.34,1.56,0.64,1)]"
                    };
                    format!("{base} {visibility} {transition}")
                }
                style=move || format!(
                    "--tw-rot:{rot}deg; --tw-bx:{bx}px;",
                    rot = rotation.get(),
                    bx  = if bumped.get() { 3.0 } else { 0.0 },
                )
                on:pointerdown=on_disc_pointer_down
                on:pointermove=on_disc_pointer_move
                on:pointerup=on_disc_pointer_up
                on:pointercancel=on_disc_pointer_cancel
                on:wheel={ let w = Rc::clone(&on_wheel); move |ev| w(ev) }
            >
                {(0..N).map(|i| {
                    let tab = TABS[i];
                    let ang_rad = base_angle(i).to_radians();
                    let cx = BOX_PX * 0.5 + RADIUS_PX * ang_rad.cos();
                    let cy = BOX_PX * 0.5 + RADIUS_PX * ang_rad.sin();
                    let arm_timer = Rc::clone(&arm_timer);
                    let was_dragging = Rc::clone(&was_dragging);
                    let btn_style = move || format!(
                        "left:{l}px; top:{t}px; --tw-cr:{cr}deg;",
                        l = cx, t = cy, cr = -rotation.get(),
                    );
                    view! {
                        <button
                            class=move || {
                                let base = "absolute rounded-lg p-0 \
                                            font-mono font-semibold text-md leading-none tracking-[0.05em] \
                                            min-w-0 flex flex-col items-center justify-center gap-0.5 cursor-pointer touch-manipulation \
                                            [-webkit-tap-highlight-color:transparent] \
                                            [transform:translate(-50%,-50%)_rotate(var(--tw-cr,0deg))] \
                                            transition-[background,border-color,box-shadow] duration-150";
                                let size = if show_labels.get() {
                                    "w-[80px] h-[52px] px-1"
                                } else {
                                    "w-[66px] h-[44px]"
                                };
                                if active.get() == tab {
                                    format!("{base} {size} bg-accent-blue-active border border-text-blue-bright text-text-dim shadow-[0_0_14px_color-mix(in_srgb,var(--text-blue)_45%,transparent)]")
                                } else {
                                    format!("{base} {size} bg-bg-wheel border border-border-strong text-text-blue shadow-none")
                                }
                            }
                            style=btn_style
                            title=move || tab_title(tab, &tr())
                            on:click=move |ev: MouseEvent| {
                                ev.stop_propagation();
                                if was_dragging.replace(false) { return; }
                                active.set(tab);
                                arm_timer();
                            }
                        >
                            <span
                                class=move || {
                                    let s = if show_labels.get() {
                                        "w-[20px] h-[20px]"
                                    } else if active.get() == tab {
                                        "w-[70%] h-[70%]"
                                    } else {
                                        "w-[56%] h-[56%]"
                                    };
                                    format!("inline-block pointer-events-none {s}")
                                }
                                inner_html=tab_icon(tab)
                            />
                            <Show when=move || show_labels.get()>
                                <span class="text-[9px] leading-none whitespace-nowrap pointer-events-none">
                                    {move || tab_title(tab, &tr())}
                                </span>
                            </Show>
                        </button>
                    }
                }).collect_view()}
            </div>

            // Active-slot indicator — a soft glow halo behind the 9 o'clock
            // slot, sitting outside the rotating layer so it stays put while
            // the wheel turns. Acts as a "dock" the active tab snaps into.
            <div
                class=move || {
                    let base = "absolute left-[30px] top-1/2 -translate-x-1/2 -translate-y-1/2 \
                                w-[78px] h-[54px] rounded-[10px] pointer-events-none \
                                border border-[color-mix(in_srgb,var(--text-blue)_55%,transparent)] \
                                bg-[radial-gradient(ellipse_at_center,color-mix(in_srgb,var(--text-blue)_18%,transparent),transparent_70%)] \
                                shadow-[inset_0_0_18px_color-mix(in_srgb,var(--text-blue)_35%,transparent)] \
                                transition-opacity duration-150";
                    let vis = if expanded.get() { "opacity-100" } else { "opacity-0" };
                    format!("{base} {vis}")
                }
            />

            // Touch-only right-edge hit strip — restores the wheel after it
            // idles out. Mouse users get hover via the container's
            // `pointerenter`; this strip ignores mouse pointers so it doesn't
            // steal sky-canvas hover.
            <div
                class="absolute right-0 top-1/2 -translate-y-1/2 w-[80px] h-[300px] pointer-events-auto"
                on:pointerdown={
                    let clear_timer = Rc::clone(&clear_timer);
                    let arm_timer = Rc::clone(&arm_timer);
                    move |ev: PointerEvent| {
                        if ev.pointer_type() != "touch" { return; }
                        expanded.set(true);
                        clear_timer();
                        arm_timer();
                    }
                }
            />

            // Centre knob — always visible (faded when idle). Tapping toggles
            // expanded state.
            <button
                class=move || {
                    let base = "absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 \
                                w-[68px] h-[68px] rounded-full border-2 border-text-blue \
                                bg-bg-wheel backdrop-blur-glass text-text-dim font-ui font-bold text-base leading-none tracking-[0.06em] \
                                cursor-pointer touch-manipulation [-webkit-tap-highlight-color:transparent] \
                                shadow-3 pointer-events-auto \
                                transition-[background,border-color,box-shadow,transform,opacity] duration-300 ease-out \
                                hover:border-accent-cyan active:scale-[0.97] \
                                flex items-center justify-center min-w-0";
                    let fade = if expanded.get() { "opacity-100" } else { "opacity-25 hover:opacity-100" };
                    format!("{base} {fade}")
                }
                title=move || tab_title(active.get(), &tr())
                on:click=on_knob_click
                on:wheel={ let w = Rc::clone(&on_wheel); move |ev| w(ev) }
            >
                <span
                    class="inline-block w-[60%] h-[60%] pointer-events-none"
                    inner_html=move || tab_icon(active.get())
                />
            </button>

            // Lang + tab-labels toggles — small chips just below the knob,
            // only visible when expanded.
            <div
                class=move || {
                    let base = "absolute left-1/2 top-[calc(50%+42px)] -translate-x-1/2 \
                                flex gap-1 transition-opacity duration-150";
                    let vis = if expanded.get() { "opacity-100 pointer-events-auto" } else { "opacity-0 pointer-events-none" };
                    format!("{base} {vis}")
                }
            >
                <button
                    class="min-w-[32px] h-[22px] min-h-[22px] px-2 \
                           rounded-[11px] border border-text-blue bg-bg-wheel text-text-blue \
                           font-mono font-semibold text-xs leading-none tracking-[0.05em] \
                           cursor-pointer touch-manipulation [-webkit-tap-highlight-color:transparent]"
                    title=move || lang.get().toggle().label()
                    on:click=move |ev: MouseEvent| {
                        ev.stop_propagation();
                        lang.update(|l| *l = l.toggle());
                    }
                >
                    {move || lang.get().label()}
                </button>
                <button
                    class=move || {
                        let base = "min-w-[30px] h-[22px] min-h-[22px] px-2 \
                                    rounded-[11px] border \
                                    font-mono font-semibold text-xs leading-none tracking-[0.05em] \
                                    cursor-pointer touch-manipulation [-webkit-tap-highlight-color:transparent]";
                        if show_labels.get() {
                            format!("{base} bg-accent-blue-active border-text-blue-bright text-text-dim")
                        } else {
                            format!("{base} bg-bg-wheel border-text-blue text-text-blue")
                        }
                    }
                    title=move || tr().tab_labels_toggle
                    on:click=move |ev: MouseEvent| {
                        ev.stop_propagation();
                        show_labels.update(|v| *v = !*v);
                    }
                >
                    "Aa"
                </button>
            </div>
        </div>
    }
}
