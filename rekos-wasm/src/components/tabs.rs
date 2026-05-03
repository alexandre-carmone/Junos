//! Centralized tab routing.
//!
//! `TabContent` reads the active tab from `ActiveTabCtx` and renders the
//! matching tab pane. SkyTab is kept mounted (display:none) so its WebGPU
//! context survives tab switches; the others are mounted lazily via `<Show>`.

use std::sync::Arc;

use leptos::prelude::*;

use crate::compat::{self, FilterWheelSnapshot, MosaicSnapshot, MountSnapshot, SiteSnapshot, SolveSnapshot};
use crate::components::files::FilesTab;
use crate::components::focus::FocusTab;
use crate::components::guide::GuideTab;
use crate::components::imaging::ImagingTab;
use crate::components::mosaic_tab::MosaicTab;
use crate::components::mount::MountTab;
use crate::components::polar_align::PolarAlignTab;
use crate::components::profiles::ProfilesTab;
use crate::components::scheduler::SchedulerTab;
use crate::components::sky::SkyTab;
use crate::ws::{DeviceStore, SendCmd};
use crate::{ActiveTabCtx, Tab};

#[component]
pub fn TabContent(
    store: DeviceStore,
    send: SendCmd,
    site: Signal<SiteSnapshot>,
    sky_center_alt: RwSignal<f64>,
    sky_center_az: RwSignal<f64>,
    sky_fov_radius: RwSignal<f64>,
    sky_follow_mount: RwSignal<bool>,
) -> impl IntoView {
    let active_tab = use_context::<ActiveTabCtx>()
        .expect("ActiveTabCtx not provided")
        .0;

    // Derive snapshots from the store once.
    let mount: Signal<MountSnapshot> = compat::derive_mount(&store);
    let camera = compat::derive_camera(&store);
    let filter_wheel: Signal<FilterWheelSnapshot> = compat::derive_filter_wheel(&store);
    let solve: Signal<SolveSnapshot> = compat::derive_solve(&store);
    let mosaic: Signal<MosaicSnapshot> = compat::derive_mosaic(&store);
    let focus_snapshot = compat::derive_focus(&store);
    let capture_snapshot = compat::derive_capture(&store);
    let polar_snapshot = compat::derive_polar_align(&store);
    let guide_snapshot = compat::derive_guide(&store);
    let scheduler_snapshot = compat::derive_scheduler(&store);
    let focal_length_mm = {
        let ts = store.telescope_settings;
        Signal::derive(move || ts.get().focal_length_mm)
    };
    let home_dir = {
        let hd = store.home_dir;
        Signal::derive(move || hd.get())
    };

    let send_sky       = Arc::clone(&send);
    let send_mount     = Arc::clone(&send);
    let send_focus     = Arc::clone(&send);
    let send_imaging   = Arc::clone(&send);
    let send_polar     = Arc::clone(&send);
    let send_guide     = Arc::clone(&send);
    let send_scheduler = Arc::clone(&send);
    let send_files     = Arc::clone(&send);
    let send_profiles  = Arc::clone(&send);
    let send_mosaic    = send;

    let sky_visible       = move || active_tab.get() == Tab::Sky;
    let mount_visible     = move || active_tab.get() == Tab::Mount;
    let focus_visible     = move || active_tab.get() == Tab::Focus;
    let imaging_visible   = move || active_tab.get() == Tab::Imaging;
    let files_visible     = move || active_tab.get() == Tab::Files;
    let polar_visible     = move || active_tab.get() == Tab::PolarAlign;
    let guide_visible     = move || active_tab.get() == Tab::Guide;
    let scheduler_visible = move || active_tab.get() == Tab::Scheduler;
    let mosaic_visible    = move || active_tab.get() == Tab::Mosaic;
    let profiles_visible  = move || active_tab.get() == Tab::Profiles;

    view! {
        <div style=move || format!(
            "position:absolute; inset:0; {}",
            if sky_visible() { "" } else { "display:none;" }
        )>
            <SkyTab
                mount=mount
                camera=camera
                site=site
                solve=solve
                focal_length_mm=focal_length_mm
                scheduler=scheduler_snapshot
                mosaic=mosaic
                send=send_sky
                center_alt=sky_center_alt
                center_az=sky_center_az
                fov_radius=sky_fov_radius
                follow_mount=sky_follow_mount
            />
        </div>
        <Show when=mount_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <MountTab mount=mount send=Arc::clone(&send_mount) />
            </div>
        </Show>
        <Show when=focus_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <FocusTab focus=focus_snapshot camera=camera send=Arc::clone(&send_focus) />
            </div>
        </Show>
        <Show when=imaging_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <ImagingTab capture=capture_snapshot camera=camera filter_wheel=filter_wheel send=Arc::clone(&send_imaging) />
            </div>
        </Show>
        <Show when=files_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <FilesTab
                    livestacker_state=store.livestacker_state
                    livestacker_settings=store.livestacker_settings
                    send=Arc::clone(&send_files)
                />
            </div>
        </Show>
        <Show when=polar_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <PolarAlignTab polar=polar_snapshot mount=mount send=Arc::clone(&send_polar) />
            </div>
        </Show>
        <Show when=guide_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <GuideTab guide=guide_snapshot send=Arc::clone(&send_guide) />
            </div>
        </Show>
        <Show when=scheduler_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <SchedulerTab scheduler=scheduler_snapshot camera=camera filter_wheel=filter_wheel send=Arc::clone(&send_scheduler) />
            </div>
        </Show>
        <Show when=mosaic_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <MosaicTab
                    camera=camera
                    filter_wheel=filter_wheel
                    focal_length_mm=focal_length_mm
                    home_dir=home_dir
                    mosaic_tiles=store.mosaic_tiles
                    send=Arc::clone(&send_mosaic)
                />
            </div>
        </Show>
        <Show when=profiles_visible>
            <div style="position:absolute; inset:0; z-index:40;">
                <ProfilesTab
                    profiles=store.profiles
                    selected_profile=store.selected_profile
                    drivers=store.drivers
                    online=store.online
                    connected=store.connected
                    send=Arc::clone(&send_profiles)
                />
            </div>
        </Show>
    }
}
