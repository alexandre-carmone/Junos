//! PHD2's secondary tools, as state machines the engine drives frame by
//! frame and the UI reads back. Each tool keeps its own state; the engine
//! hands it what PHD2 read from the guider and the mount, and carries out
//! what it asks for (slews, lock moves, guide outputs on or off).
//!
//! Ported from PHD2 `src/polardrift_toolwin.cpp`, `src/staticpa_toolwin.cpp` — BSD-3-Clause, see THIRD_PARTY.md

pub mod polar_drift;
pub mod static_pa;

/// The checks both polar tools make before their window opens
/// (`CreatePolarDriftToolWindow`, `CreateStaticPaToolWindow`). PHD2 then
/// asks for confirmation when the pixel scale is unknown (1.0), which is the
/// UI's call, and starts looping when the camera is idle, which is the
/// engine's.
pub fn open_check(camera_connected: bool, calibrating_or_guiding: bool) -> Result<(), &'static str> {
    if !camera_connected {
        return Err("Please connect a camera first.");
    }
    if calibrating_or_guiding {
        return Err("Please wait till Calibration is done and stop guiding");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_check_order() {
        assert_eq!(open_check(false, true), Err("Please connect a camera first."));
        assert_eq!(open_check(true, true), Err("Please wait till Calibration is done and stop guiding"));
        assert_eq!(open_check(true, false), Ok(()));
    }
}
