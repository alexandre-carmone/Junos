//! INDIGO backend: an Ekos Live emulation on top of an `indigo_server`.
//!
//! Where the Ekos backend relays KStars verbatim, this one dials *out* to an
//! INDIGO server (JSON over WebSocket), mirrors its properties, and answers the
//! browser's Ekos Live commands itself — so the WASM client needs no second
//! protocol. Phase 1 only wires the seam: the session is filled in next.

use std::sync::Arc;

use tracing::info;

use crate::config::Config;
use crate::hub::Hub;

/// Entry point spawned by `main` with `--backend indigo`. Never returns: it
/// keeps reconnecting to the INDIGO server for the life of the process.
pub async fn run(_hub: Hub, config: Arc<Config>) {
    info!(url = %config.indigo_url, "INDIGO backend selected (session not implemented yet)");
    std::future::pending::<()>().await;
}
