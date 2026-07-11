//! Shared state between the inbound KStars handlers and the outbound browser handler.
//!
//! In the Ekos Live protocol, KStars is the *client*: it dials our server on
//! `/message/ekos` (JSON) and `/media/ekos` (binary). Browsers connect to `/ws`
//! and we fan-out messages between them and the currently connected KStars session.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};

/// Broadcast capacity: number of in-flight messages buffered for slow browsers
/// before they start getting lagged errors.
const BROADCAST_CAP: usize = 512;

/// Per-session command queue capacity (browser → KStars).
pub const CMD_CAP: usize = 256;

#[derive(Clone)]
pub struct Hub {
    /// Text frames from KStars (message channel + decoded media frames)
    /// fanned out to all connected browsers.
    pub browser_tx: broadcast::Sender<String>,

    /// Current outbound pipe to the KStars message channel.
    /// `None` while no KStars session is connected. Set by the `/message/ekos`
    /// handler on connect and cleared on disconnect.
    pub kstars_msg_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,

    /// Last `new_connection_state` received from KStars, replayed to browsers
    /// on connect so a page refresh gets the full connected+online state.
    pub last_connection_state: Arc<Mutex<Option<String>>>,

    /// Last `astro_get_location` reply from KStars (the observer site), replayed
    /// to browsers on connect so a late-joining page gets KStars' real location
    /// immediately without having to re-query it. Primed once per KStars attach.
    pub last_site_location: Arc<Mutex<Option<String>>>,
}

impl Hub {
    pub fn new() -> Self {
        let (browser_tx, _) = broadcast::channel(BROADCAST_CAP);
        Self {
            browser_tx,
            kstars_msg_tx: Arc::new(Mutex::new(None)),
            last_connection_state: Arc::new(Mutex::new(None)),
            last_site_location: Arc::new(Mutex::new(None)),
        }
    }

    /// Subscribe a new browser to the broadcast of KStars events.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.browser_tx.subscribe()
    }

    /// Send a command string to KStars. Returns `false` if no KStars session
    /// is currently connected or the queue is closed.
    pub async fn send_to_kstars(&self, cmd: String) -> bool {
        let guard = self.kstars_msg_tx.lock().await;
        if let Some(tx) = guard.as_ref() {
            tx.send(cmd).await.is_ok()
        } else {
            false
        }
    }
}
