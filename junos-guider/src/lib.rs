//! `junos-guider` — a PHD2-compatible autoguider.
//!
//! A port of PHD2 2.6.14 (see `THIRD_PARTY.md`): the guiding engine lives in
//! [`engine`] as pure, synchronous, trait-driven code (no I/O, no async) so it
//! runs headless against the gear simulator in the tests; the daemon around it
//! (INDI client, PHD2 JSON-RPC event server on :4400, the local WebSocket the
//! junos-server relay dials) is added module by module.
//!
//! Reference tree: `phd2/` at the repo root (read-only, gitignored), the same
//! way `kstars/` documents the Ekos Live wire format.

pub mod config;
pub mod engine;
pub mod indi;
pub mod profile;
pub mod server;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
