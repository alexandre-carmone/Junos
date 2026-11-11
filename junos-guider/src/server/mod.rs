//! PHD2's event server: the JSON-RPC 2.0 socket (port 4400 + instance − 1)
//! that Ekos, NINA and every other PHD2 client drive the guider through.
//!
//! [`rpc`] holds the protocol itself, free of sockets: the line framing, the
//! request parsing, the method table, the parameter getters and the replies.

pub mod rpc;
