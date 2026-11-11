//! The Gaussian-process guider (PHD2's Predictive PEC): the dense linear
//! algebra and the FFT it rests on first, the GP math and the guider later.

pub mod gp;
pub mod kernels;
pub mod linalg;
pub mod math;
