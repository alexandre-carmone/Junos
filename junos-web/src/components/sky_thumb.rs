//! A small sky-map cutout of an object — the Targets tab's thumbnails.
//!
//! A `<canvas>` rendered through a shared `SkyMapSampler`
//! (`sky/skymap_sampler.rs`): once on mount and again on each tile landing
//! (`epoch`) until every tile under the field is in. The sampler is shared
//! by all the thumbnails of a tab so the tiles of neighbouring objects are
//! fetched once.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::sky::skymap_sampler::{CutoutSpec, SkyMapSampler};

/// The sampler the thumbnails of a tab share, created once the server has
/// described its map (`None` until then, or without a map).
pub type SharedSampler = StoredValue<Rc<RefCell<Option<SkyMapSampler>>>, LocalStorage>;

/// Field of an object's thumbnail, degrees: twice its size, at least 0.1°.
pub fn thumb_fov_deg(size_arcmin: f32) -> f64 {
    (f64::from(size_arcmin) / 60.0 * 2.0).max(0.1)
}

#[component]
pub fn SkyThumb(
    spec: CutoutSpec,
    sampler: SharedSampler,
    epoch: RwSignal<u32>,
    #[prop(into)] class: String,
) -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let done = Rc::new(Cell::new(false));
    Effect::new(move |_| {
        epoch.track();
        if done.get() {
            return;
        }
        let Some(canvas) = canvas_ref.get() else { return };
        let el: web_sys::HtmlCanvasElement = canvas.into();
        let Ok(Some(ctx)) = el.get_context("2d") else { return };
        let Ok(ctx) = ctx.dyn_into::<web_sys::CanvasRenderingContext2d>() else { return };
        sampler.with_value(|s| {
            let Ok(mut s) = s.try_borrow_mut() else { return };
            let Some(s) = s.as_mut() else { return };
            let (w, h) = (f64::from(spec.width), f64::from(spec.height));
            if let Some(st) = s.render_into(&ctx, &spec, 0.0, 0.0, w, h) {
                if st.complete {
                    done.set(true);
                }
            }
        });
    });
    view! {
        <canvas node_ref=canvas_ref width=spec.width height=spec.height class=class />
    }
}
