//! The guiding engine: PHD2's worker-thread logic, ported as pure code.
//!
//! Nothing in here does I/O. Devices come in through the traits of
//! [`traits`]; time through [`traits::Clock`]; everything the engine tells the
//! outside leaves as events. The daemon owns the thread and the channels.

pub mod algo;
pub mod calibration;
pub mod fits;
pub mod image;
pub mod log;
pub mod rng;
pub mod star;
pub mod stats;
pub mod traits;
