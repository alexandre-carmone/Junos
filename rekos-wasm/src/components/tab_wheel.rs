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
use web_sys::{MouseEvent, PointerEvent};

#[derive(Clone, Copy)]
struct DragState {
    cx: f64,
    cy: f64,
    start_angle_deg: f32,
    anchor_idx: usize,
    moved: bool,
}

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
const RADIUS_PX: f32 = 115.0;
const BOX_PX: f32 = 290.0;
const KNOB_PX: f32 = 68.0;
const COLLAPSE_MS: i32 = 2500;
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

// Inline SVG icons — `currentColor` so they inherit the button's text color.
// 24x24 viewBox; sized at the call site via the wrapping <span>.
fn tab_icon(tab: Tab) -> &'static str {
    match tab {
        // 4-point star
        Tab::Sky => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 2 L13.6 10.4 L22 12 L13.6 13.6 L12 22 L10.4 13.6 L2 12 L10.4 10.4 Z"/><circle cx="18" cy="5" r="0.8" fill="currentColor"/><circle cx="5" cy="18" r="0.8" fill="currentColor"/></svg>"##,
        // Equatorial mount: tripod + tilted RA axis with counterweight bar
        Tab::Mount => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21 L7 14 M12 21 L17 14 M12 21 L12 15"/><path d="M5 6 L19 16" /><circle cx="12" cy="11" r="2.2" fill="currentColor" stroke="none"/><circle cx="5" cy="6" r="1.6" fill="currentColor" stroke="none"/><circle cx="19" cy="16" r="1.6" fill="currentColor" stroke="none"/></svg>"##,
        // Concentric focus rings
        Tab::Focus => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="5"/><circle cx="12" cy="12" r="1.5" fill="currentColor"/></svg>"##,
        // Camera body
        Tab::Imaging => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 8 L8 8 L9.5 5.5 L14.5 5.5 L16 8 L20 8 A1 1 0 0 1 21 9 L21 18 A1 1 0 0 1 20 19 L4 19 A1 1 0 0 1 3 18 L3 9 A1 1 0 0 1 4 8 Z"/><circle cx="12" cy="13" r="4"/></svg>"##,
        // Ursa Minor (Little Dipper): handle curving from Polaris to a 4-star
        // bowl. Polaris (top-left) and Kochab (bowl, lower-right) are drawn
        // larger as the two brightest stars.
        // Stars: Polaris(4,5) - Yildun(7.5,7.5) - eps(10.5,10.5) - zeta(13.5,12.5)
        //        bowl: zeta(13.5,12.5) - eta(18,9.5) - Pherkad(21,15) - Kochab(16,19) - back to zeta
        Tab::PolarAlign => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5 L7.5 7.5 L10.5 10.5 L13.5 12.5"/><path d="M13.5 12.5 L18 9.5 L21 15 L16 19 Z"/><circle cx="4" cy="5" r="1.6" fill="currentColor" stroke="none"/><circle cx="7.5" cy="7.5" r="0.85" fill="currentColor" stroke="none"/><circle cx="10.5" cy="10.5" r="0.85" fill="currentColor" stroke="none"/><circle cx="13.5" cy="12.5" r="0.95" fill="currentColor" stroke="none"/><circle cx="18" cy="9.5" r="0.95" fill="currentColor" stroke="none"/><circle cx="21" cy="15" r="1.0" fill="currentColor" stroke="none"/><circle cx="16" cy="19" r="1.4" fill="currentColor" stroke="none"/></svg>"##,
        // Guide: locked guide star inside a square tracking reticle (corner brackets)
        Tab::Guide => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7 L3 3 L7 3 M17 3 L21 3 L21 7 M21 17 L21 21 L17 21 M7 21 L3 21 L3 17"/><path d="M12 8.5 L12 15.5 M8.5 12 L15.5 12"/><circle cx="12" cy="12" r="2.2" fill="currentColor" stroke="none"/></svg>"##,
        // Calendar / scheduler
        Tab::Scheduler => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="16" rx="2"/><path d="M3 10 L21 10 M8 3 L8 7 M16 3 L16 7"/><circle cx="9" cy="14" r="0.9" fill="currentColor"/><circle cx="13" cy="14" r="0.9" fill="currentColor"/><circle cx="17" cy="14" r="0.9" fill="currentColor"/></svg>"##,
        // 3x3 mosaic grid
        Tab::Mosaic => r##"<svg viewBox="0 0 24 24" width="100%" height="100%" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="1"/><path d="M3 9 L21 9 M3 15 L21 15 M9 3 L9 21 M15 3 L15 21"/></svg>"##,
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

    // Free-rotation offset applied on top of the active tab's snap angle
    // while the user is dragging the disc. Reset to 0 on release; the snap
    // animates because `active` updates and the CSS transition kicks back in.
    let drag_offset_deg = RwSignal::new(0.0_f32);
    let dragging = RwSignal::new(false);

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
            // Live, smooth rotation — no quantisation. We commit a tab on
            // release.
            drag_offset_deg.set(delta);
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
                    let n = N as i32;
                    let new_idx = (((st.anchor_idx as i32 - steps) % n) + n) % n;
                    active.set(TABS[new_idx as usize]);
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
    view! {
        <div
            style=move || format!(
                "position:absolute; right:{r}px; top:50%; transform:translateY(-50%); \
                 z-index:60; pointer-events:none; \
                 width:{w}px; height:{w}px; \
                 display:flex; align-items:center; justify-content:center;",
                r = RIGHT_OFFSET_PX, w = BOX_PX,
            )
            on:pointerenter=on_pointer_enter
            on:pointerleave=on_pointer_leave
            on:pointerdown=on_pointer_down
            on:pointerup=on_pointer_up
            on:click=|ev: MouseEvent| ev.stop_propagation()
        >
            // Rotating arc layer — visible only when expanded.
            <div
                node_ref=disc_ref
                style=move || format!(
                    "position:absolute; left:0; top:0; width:{box_}px; height:{box_}px; \
                     border-radius:50%; \
                     background:rgba(6,6,15,0.55); border:1px solid #222; \
                     transform:rotate({rot}deg); \
                     transition:{tr}; \
                     opacity:{op}; pointer-events:{pe}; \
                     touch-action:none;",
                    box_ = BOX_PX,
                    rot  = rotation.get(),
                    tr   = if dragging.get() { "opacity 0.15s" } else { "transform 0.25s ease, opacity 0.15s" },
                    op   = if expanded.get() { 1.0 } else { 0.0 },
                    pe   = if expanded.get() { "auto" } else { "none" },
                )
                on:pointerdown=on_disc_pointer_down
                on:pointermove=on_disc_pointer_move
                on:pointerup=on_disc_pointer_up
                on:pointercancel=on_disc_pointer_cancel
            >
                {(0..N).map(|i| {
                    let tab = TABS[i];
                    let ang_rad = base_angle(i).to_radians();
                    let cx = BOX_PX * 0.5 + RADIUS_PX * ang_rad.cos();
                    let cy = BOX_PX * 0.5 + RADIUS_PX * ang_rad.sin();
                    let btn_w = 62.0_f32;
                    let btn_h = 38.0_f32;
                    let arm_timer = Rc::clone(&arm_timer);
                    let was_dragging = Rc::clone(&was_dragging);
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
                             font:600 13px/1 ui-monospace,monospace; letter-spacing:0.05em; \
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
                                if was_dragging.replace(false) { return; }
                                active.set(tab);
                                arm_timer();
                            }
                        >
                            <span
                                style="display:inline-block; width:60%; height:60%; \
                                       pointer-events:none;"
                                inner_html=tab_icon(tab)
                            />
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
                     font:700 15px/1 ui-monospace,monospace; letter-spacing:0.06em; \
                     cursor:pointer; touch-action:manipulation; \
                     -webkit-tap-highlight-color:transparent; \
                     box-shadow:0 0 10px rgba(0,0,0,0.5); \
                     pointer-events:auto; \
                     display:flex; align-items:center; justify-content:center;",
                    k = KNOB_PX,
                )
                title=move || tab_title(active.get(), &tr())
                on:click=on_knob_click
            >
                <span
                    style="display:inline-block; width:60%; height:60%; \
                           pointer-events:none;"
                    inner_html=move || tab_icon(active.get())
                />
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
