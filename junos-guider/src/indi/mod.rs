//! The INDI client side: devices reached through `indiserver`'s XML protocol,
//! the way PHD2 reaches them through libindiclient on Linux.
//!
//! [`xml`] turns the byte stream into typed messages and writes the client's
//! requests; [`store`] keeps the devices' properties, with a sequence number
//! per change for the pulse waits, and hands out the decoded BLOBs. Pure
//! code: the socket task comes with the client.

pub mod store;
pub mod xml;
