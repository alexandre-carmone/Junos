//! The Gaussian-process guider (PHD2's Predictive PEC): the dense linear
//! algebra and the FFT it rests on, the GP math, and the guider.

pub mod gp;
pub mod guider;
pub mod kernels;
pub mod linalg;
pub mod math;
