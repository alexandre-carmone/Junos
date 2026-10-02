//! A frame full screen: wheel / pinch zooms about the pointer, drag pans,
//! double-click resets, and a click at fit size closes. Imaging shows KStars'
//! transmitted preview (stretched JPEGs, media.cpp), Files the server's
//! preview render of a capture.

use std::collections::HashMap;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{PointerEvent, WheelEvent};

use crate::i18n::{t, Lang};

pub fn frame_zoom(url: Signal<Option<String>>, open: RwSignal<bool>, lang: RwSignal<Lang>) -> impl IntoView {
    // Image transform: scale (1 = fit) and translation from the centre, px.
    let scale = RwSignal::new(1.0_f64);
    let tx = RwSignal::new(0.0_f64);
    let ty = RwSignal::new(0.0_f64);
    // Gesture state, never rendered: pointers down (id → client x, y), the
    // last pinch distance, and whether the gesture moved (a drag must not
    // close the viewer).
    let ptrs = StoredValue::new(HashMap::<i32, (f64, f64)>::new());
    let pinch = StoredValue::new(None::<f64>);
    let dragged = StoredValue::new(false);

    let reset = move || {
        scale.set(1.0);
        tx.set(0.0);
        ty.set(0.0);
    };
    Effect::new(move |_| if open.get() { reset() });
    Effect::new(move |_| if url.with(Option::is_none) { open.set(false) });

    // Zoom by `f` about (cx, cy), relative to the centre, keeping that point
    // still: a point shows at `t + s·v`, so t' = c·(1 − r) + r·t, r = s'/s.
    let zoom = move |f: f64, cx: f64, cy: f64| {
        let s = scale.get_untracked();
        let s2 = (s * f).clamp(1.0, 20.0);
        if s2 <= 1.000_1 {
            return reset();
        }
        let r = s2 / s;
        scale.set(s2);
        tx.update(|t| *t = cx * (1.0 - r) + r * *t);
        ty.update(|t| *t = cy * (1.0 - r) + r * *t);
    };

    let on_wheel = move |ev: WheelEvent| {
        ev.prevent_default();
        let (cx, cy) = center(&ev);
        zoom(if ev.delta_y() < 0.0 { 1.15 } else { 1.0 / 1.15 }, ev.client_x() as f64 - cx, ev.client_y() as f64 - cy);
    };
    let on_down = move |ev: PointerEvent| {
        ev.prevent_default();
        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
            let _ = el.set_pointer_capture(ev.pointer_id());
        }
        dragged.set_value(false);
        pinch.set_value(None);
        ptrs.update_value(|m| { m.insert(ev.pointer_id(), (ev.client_x() as f64, ev.client_y() as f64)); });
    };
    let on_move = move |ev: PointerEvent| {
        let id = ev.pointer_id();
        let Some((ox, oy)) = ptrs.with_value(|m| m.get(&id).copied()) else { return };
        let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
        ptrs.update_value(|m| { m.insert(id, (x, y)); });
        dragged.set_value(true);
        match ptrs.with_value(|m| m.values().copied().collect::<Vec<_>>())[..] {
            [_] => {
                tx.update(|t| *t += x - ox);
                ty.update(|t| *t += y - oy);
            }
            [(ax, ay), (bx, by)] => {
                let dist = (ax - bx).hypot(ay - by);
                if let Some(prev) = pinch.get_value().filter(|p| *p > 0.0) {
                    let (cx, cy) = center(&ev);
                    zoom(dist / prev, (ax + bx) / 2.0 - cx, (ay + by) / 2.0 - cy);
                }
                pinch.set_value(Some(dist));
            }
            _ => {}
        }
    };
    let on_up = move |ev: PointerEvent| {
        ptrs.update_value(|m| { m.remove(&ev.pointer_id()); });
        pinch.set_value(None);
    };
    // Zoomed in, a click is for looking: only close at fit size.
    let on_click = move |_| {
        if !dragged.get_value() && scale.get_untracked() <= 1.000_1 {
            open.set(false);
        }
    };

    view! {
        <Show when=move || open.get()>
            <div class="fixed inset-0 md:right-[64px] z-[80] bg-[rgba(0,0,0,0.92)]">
                <div class="absolute inset-0 flex items-center justify-center overflow-hidden \
                            select-none cursor-move [touch-action:none]"
                     on:wheel=on_wheel on:pointerdown=on_down on:pointermove=on_move
                     on:pointerup=on_up on:pointercancel=on_up
                     on:dblclick=move |_| reset() on:click=on_click>
                    <img class="max-w-full max-h-full object-contain [image-rendering:pixelated] \
                                pointer-events-none select-none will-change-transform"
                         style:transform=move || format!("translate({}px,{}px) scale({})", tx.get(), ty.get(), scale.get())
                         src=move || url.get().unwrap_or_default() />
                </div>
                <button class="btn-icon absolute right-3 top-[max(0.75rem,env(safe-area-inset-top))]"
                        title=move || t(lang.get()).info_close on:click=move |_| open.set(false)>
                    "\u{2716}"
                </button>
            </div>
        </Show>
    }
}

/// Client coordinates of the event target's centre.
fn center(ev: &web_sys::MouseEvent) -> (f64, f64) {
    ev.current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .map(|el| {
            let r = el.get_bounding_client_rect();
            (r.left() + r.width() * 0.5, r.top() + r.height() * 0.5)
        })
        .unwrap_or((0.0, 0.0))
}
