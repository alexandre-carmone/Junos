//! PHD2's secondary tools, as state machines the engine drives frame by
//! frame and the UI reads back. Each tool keeps its own state; the engine
//! hands it what PHD2 read from the guider and the mount, and carries out
//! what it asks for (slews, lock moves, guide outputs on or off).
//!
//! Ported from PHD2 `src/polardrift_toolwin.cpp`, `src/staticpa_toolwin.cpp` — BSD-3-Clause, see THIRD_PARTY.md

pub mod bookmarks;
pub mod cal_assistant;
pub mod comet;
pub mod polar_drift;
pub mod starcross;
pub mod static_pa;

/// The checks both polar tools make before their window opens
/// (`CreatePolarDriftToolWindow`, `polardrift_toolwin.cpp:64-84`;
/// `CreateStaticPaToolWindow`, `staticpa_toolwin.cpp:108-128`). Between the
/// two PHD2 asks for confirmation when the pixel scale is unknown (1.0),
/// unless [`PIXSCALE_CONFIRM_KEY`] says not to ask again: the camera check,
/// then the prompt ([`POLAR_DRIFT_PIXSCALE_PROMPT`] or
/// [`STATIC_PA_PIXSCALE_PROMPT`]), then the calibrating / guiding check.
/// Once open, the window starts looping when the camera is idle, which is
/// the engine's job.
pub fn open_check(camera_connected: bool, calibrating_or_guiding: bool) -> Result<(), &'static str> {
    if !camera_connected {
        return Err("Please connect a camera first.");
    }
    if calibrating_or_guiding {
        return Err("Please wait till Calibration is done and stop guiding");
    }
    Ok(())
}

/// The global key (`/Confirm` + the name, confirm_dialog.cpp:66) set when
/// the user ticks "don't ask again" on the pixel-scale prompt; both tools
/// share it.
pub const PIXSCALE_CONFIRM_KEY: &str = "/Confirm/rotate_tool_without_pixscale";

/// The Polar Drift tool's pixel-scale prompt (`polardrift_toolwin.cpp:66`).
pub const POLAR_DRIFT_PIXSCALE_PROMPT: &str = "The Polar Drift Align tool is most effective when PHD2 knows your guide\n\
    scope focal length and camera pixel size.\n\
    \n\
    Enter your guide scope focal length on the Global tab in the Brain.\n\
    Enter your camera pixel size on the Camera tab in the Brain.\n\
    \n\
    Would you like to run the tool anyway?";

/// The Static PA tool's (`staticpa_toolwin.cpp:110`).
pub const STATIC_PA_PIXSCALE_PROMPT: &str = "The Static Align tool is most effective when PHD2 knows your guide\n\
    scope focal length and camera pixel size.\n\
    \n\
    Enter your guide scope focal length on the Global tab in the Brain.\n\
    Enter your camera pixel size on the Camera tab in the Brain.\n\
    \n\
    Would you like to run the tool anyway?";

/// What both tools' tests share: an `i32`-only profile and the harness's
/// log hash.
#[cfg(test)]
mod test_util {
    use std::collections::HashMap;

    use crate::engine::calibration::ProfileAccess;

    #[derive(Default)]
    pub struct Mem(pub HashMap<String, i32>);

    impl ProfileAccess for Mem {
        fn has(&self, key: &str) -> bool {
            self.0.contains_key(key)
        }
        fn get_f64(&self, _: &str, default: f64) -> f64 {
            default
        }
        fn get_i32(&self, key: &str, default: i32) -> i32 {
            self.0.get(key).copied().unwrap_or(default)
        }
        fn get_str(&self, _: &str, default: &str) -> String {
            default.to_string()
        }
        fn set_f64(&mut self, _: &str, _: f64) {}
        fn set_i32(&mut self, key: &str, value: i32) {
            self.0.insert(key.to_string(), value);
        }
        fn set_str(&mut self, _: &str, _: &str) {}
    }

    /// FNV-1a 64 of the lines joined by `\n`, as the harness hashes them.
    pub fn fnv1a(lines: &[String]) -> u64 {
        lines.join("\n").bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_check_order() {
        assert_eq!(open_check(false, true), Err("Please connect a camera first."));
        assert_eq!(open_check(true, true), Err("Please wait till Calibration is done and stop guiding"));
        assert_eq!(open_check(true, false), Ok(()));
        // The two prompts differ only in the tool's name.
        assert_eq!(
            POLAR_DRIFT_PIXSCALE_PROMPT.replace("Polar Drift Align", "Static Align"),
            STATIC_PA_PIXSCALE_PROMPT
        );
        assert!(STATIC_PA_PIXSCALE_PROMPT.starts_with(
            "The Static Align tool is most effective when PHD2 knows your guide\nscope focal length and camera pixel \
             size.\n\nEnter your guide scope"
        ));
        assert!(STATIC_PA_PIXSCALE_PROMPT.ends_with("in the Brain.\n\nWould you like to run the tool anyway?"));
    }
}
