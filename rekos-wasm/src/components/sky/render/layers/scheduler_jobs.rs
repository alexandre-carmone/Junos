//! Scheduler job markers: one per scheduled target on the sky.

use web_sys::CanvasRenderingContext2d;

use super::super::layer::{Frame, SkyLayer};
use super::super::render_scheduler_jobs;

pub struct SchedulerJobsLayer;

impl SkyLayer for SchedulerJobsLayer {
    fn name(&self) -> &'static str { "scheduler_jobs" }
    fn enabled(&self, f: &Frame) -> bool { f.toggles.scheduler_jobs_on }
    fn draw_canvas2d(&self, f: &Frame, ctx: &CanvasRenderingContext2d) {
        let proj = |alt: f64, az: f64| f.project(alt, az);
        render_scheduler_jobs(ctx, f.legacy_params, &proj);
    }
}
