//! Browser-side cache of the planetarium's survey images: one sprite per
//! deep-sky object (`/api/dso_tiles/thumbs/<slug>.jpg`) and the all-sky
//! Milky Way panorama (`/api/dso_tiles/allsky.jpg`).
//!
//! The cache owns the decoded `HtmlImageElement`s, shared by both render
//! paths: the GPU `DsoImageLayer` uploads them into its texture array, the
//! Canvas2D fallback `drawImage`s them directly. Loads are asynchronous; when
//! one lands the cache bumps `epoch` so the render Effect reruns at once
//! rather than on the next idle tick. Decoded images are LRU-bounded so a
//! long pan session on a phone can't accumulate hundreds of megabytes.
//!
//! Which object has a tile at all comes from the server's `index.json`
//! (`DsoTileIndex`, keyed by catalog name); an absent cache simply means no
//! images are ever requested.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::HtmlImageElement;

use crate::dso_tiles::{DsoTile, DsoTileIndex, ALLSKY_SMALL_URL, ALLSKY_URL};

#[derive(Copy, Clone, PartialEq, Eq)]
enum LoadState {
    Loading,
    Ready,
    Failed,
}

/// One image in flight or decoded. Dropping it detaches the handlers and
/// cancels the fetch, so an evicted slot never fires into a dead closure.
struct Slot {
    img: HtmlImageElement,
    state: Rc<Cell<LoadState>>,
    last_used: u64,
    _onload: Closure<dyn FnMut()>,
    _onerror: Closure<dyn FnMut()>,
}

impl Slot {
    fn start(url: &str, loading: &Rc<Cell<usize>>, epoch: RwSignal<u32>, frame: u64) -> Option<Self> {
        let img = HtmlImageElement::new().ok()?;
        let state = Rc::new(Cell::new(LoadState::Loading));
        loading.set(loading.get() + 1);
        let mk = |done: LoadState| {
            let st = Rc::clone(&state);
            let ld = Rc::clone(loading);
            Closure::<dyn FnMut()>::new(move || {
                if st.get() != LoadState::Loading {
                    return;
                }
                st.set(done);
                ld.set(ld.get().saturating_sub(1));
                epoch.update(|v| *v = v.wrapping_add(1));
            })
        };
        let onload = mk(LoadState::Ready);
        let onerror = mk(LoadState::Failed);
        img.set_onload(Some(onload.as_ref().unchecked_ref()));
        img.set_onerror(Some(onerror.as_ref().unchecked_ref()));
        img.set_src(url);
        Some(Slot { img, state, last_used: frame, _onload: onload, _onerror: onerror })
    }

    fn is_ready(&self) -> bool {
        self.state.get() == LoadState::Ready && self.img.complete() && self.img.natural_width() > 0
    }

    fn is_loading(&self) -> bool {
        self.state.get() == LoadState::Loading
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.img.set_onload(None);
        self.img.set_onerror(None);
        if self.is_loading() {
            // Abort the fetch; the browser drops the request on src change.
            self.img.set_src("");
        }
    }
}

/// Simultaneous sprite fetches. The server reads each file synchronously, so
/// a burst of dozens would just queue there and starve the first ones.
const MAX_CONCURRENT: usize = 6;

pub struct DsoImageCache {
    index: Option<Arc<DsoTileIndex>>,
    /// Catalog index → tile index, memoised (`None` = no tile for the object).
    tile_of: HashMap<u32, Option<usize>>,
    slots: HashMap<u32, Slot>,
    loading: Rc<Cell<usize>>,
    frame: u64,
    epoch: RwSignal<u32>,
    /// Decoded sprites kept at most; beyond it the least recently drawn go.
    max_decoded: usize,
    allsky: Option<Slot>,
    allsky_small_tried: bool,
}

impl DsoImageCache {
    /// `epoch` is bumped on every load completion so the sky redraws.
    pub fn new(epoch: RwSignal<u32>, is_mobile: bool) -> Self {
        Self {
            index: None,
            tile_of: HashMap::new(),
            slots: HashMap::new(),
            loading: Rc::new(Cell::new(0)),
            frame: 0,
            epoch,
            max_decoded: if is_mobile { 64 } else { 128 },
            allsky: None,
            allsky_small_tried: false,
        }
    }

    /// Point the cache at the server's tile index (once it has arrived).
    pub fn set_index(&mut self, index: Option<Arc<DsoTileIndex>>) {
        let same = match (&self.index, &index) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.index = index;
            self.tile_of.clear();
        }
    }

    pub fn has_index(&self) -> bool {
        self.index.as_ref().is_some_and(|i| !i.tiles.is_empty())
    }

    /// Advance the LRU clock and drop decoded sprites past the cap. Call once
    /// per frame before any `image()` lookups.
    pub fn begin_frame(&mut self) {
        self.frame += 1;
        if self.slots.len() <= self.max_decoded {
            return;
        }
        let frame = self.frame;
        let mut stale: Vec<(u64, u32)> = self
            .slots
            .iter()
            .filter(|(_, s)| !s.is_loading() && s.last_used < frame)
            .map(|(id, s)| (s.last_used, *id))
            .collect();
        stale.sort_unstable();
        let excess = self.slots.len() - self.max_decoded;
        for (_, id) in stale.into_iter().take(excess) {
            self.slots.remove(&id);
        }
    }

    /// The cutout tile for catalog object `id` named `name`, if the cache
    /// holds one.
    pub fn tile_for(&mut self, id: u32, name: &str) -> Option<&DsoTile> {
        let index = self.index.as_ref()?;
        let ti = *self.tile_of.entry(id).or_insert_with(|| index.index_of(name));
        ti.map(|i| &index.tiles[i])
    }

    /// The decoded sprite for `id`, or `None` while it loads (a load is
    /// started here if none is in flight and the concurrency budget allows).
    pub fn image(&mut self, id: u32, url: &str) -> Option<HtmlImageElement> {
        let frame = self.frame;
        if let Some(slot) = self.slots.get_mut(&id) {
            slot.last_used = frame;
            return slot.is_ready().then(|| slot.img.clone());
        }
        if self.loading.get() >= MAX_CONCURRENT {
            return None;
        }
        let slot = Slot::start(url, &self.loading, self.epoch, frame)?;
        self.slots.insert(id, slot);
        None
    }

    /// The Milky Way panorama once decoded. The first call starts the load:
    /// the 2048 px file on phones, else the 4096 px one, falling back to the
    /// small one if the large is missing (a cache built with an older script).
    pub fn allsky_image(&mut self, prefer_small: bool) -> Option<HtmlImageElement> {
        enum Next {
            Start(&'static str),
            Ready(HtmlImageElement),
            Wait,
        }
        let next = match self.allsky.as_ref() {
            None => Next::Start(if prefer_small { ALLSKY_SMALL_URL } else { ALLSKY_URL }),
            Some(s) if s.is_ready() => Next::Ready(s.img.clone()),
            Some(s) if s.state.get() == LoadState::Failed && !self.allsky_small_tried => {
                Next::Start(ALLSKY_SMALL_URL)
            }
            Some(_) => Next::Wait,
        };
        match next {
            Next::Start(url) => {
                self.allsky_small_tried |= url == ALLSKY_SMALL_URL;
                self.allsky = Slot::start(url, &self.loading, self.epoch, self.frame);
                None
            }
            Next::Ready(img) => Some(img),
            Next::Wait => None,
        }
    }
}
