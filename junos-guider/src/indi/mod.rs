//! The INDI client side: devices reached through `indiserver`'s XML protocol,
//! the way PHD2 reaches them through libindiclient on Linux.
//!
//! [`xml`] turns the byte stream into typed messages and writes the client's
//! requests; [`store`] keeps the devices' properties, with a sequence number
//! per change for the pulse waits, and hands out the decoded BLOBs. Both are
//! pure. [`client`] owns the socket: a reader thread feeding the store, the
//! writes, the reconnects and the device chooser. The device wrappers
//! ([`camera`], [`mount`], [`ao`], [`rotator`]) implement the engine's
//! device traits over it.

pub mod ao;
pub mod camera;
pub mod client;
pub mod mount;
pub mod rotator;
pub mod store;
pub mod xml;

#[cfg(test)]
pub(crate) mod fake;
