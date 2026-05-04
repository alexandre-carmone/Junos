//! Layer impls for the new render pipeline. Each module is one
//! `SkyLayer` impl, migrated incrementally from the legacy free fns in
//! `super::mod` (`render_*`).

pub mod center_crosshair;
pub mod fov_reticle;
pub mod grids;
pub mod ground;
pub mod info_overlay;
pub mod mosaic;
pub mod mount_crosshair;
pub mod scheduler_jobs;
pub mod slew_trail;
pub mod solve_marker;
pub mod zenith;
