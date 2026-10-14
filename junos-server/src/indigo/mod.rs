//! INDIGO backend: an Ekos Live emulation on top of an `indigo_server`.
//!
//! Where the Ekos backend relays KStars verbatim, this one dials *out* to an
//! INDIGO server (JSON over WebSocket), mirrors its properties
//! (`protocol.rs`), answers the browser's Ekos Live commands itself
//! (`commands.rs`) and turns property changes into Ekos Live events
//! (`events.rs`) — so the WASM client needs no second protocol.

mod alias;
mod astro;
mod commands;
mod events;
mod imaging;
mod indi_shape;
mod mount;
mod protocol;
mod roles;
mod scansexa;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// A fetched preview JPEG with its pixel size, from the BLOB task.
type Preview = (Vec<u8>, (u32, u32));

use crate::config::Config;
use crate::hub::{connection_state_msg, Hub, CMD_CAP};

use commands::{Ctx, Out};
use protocol::{Change, PropertyStore};
use roles::Roles;

/// Entry point spawned by `main` with `--backend indigo`. Never returns: it
/// keeps reconnecting to the INDIGO server for the life of the process, with
/// the same 500 ms → 10 s backoff the browser uses towards us.
pub async fn run(hub: Hub, config: Arc<Config>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match session(&hub, &config).await {
            Ok(()) => backoff = Duration::from_millis(500),
            Err(e) => {
                warn!(url = %config.indigo_url, "INDIGO: {e:#}; retrying in {backoff:?}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// One connection to the INDIGO server, from handshake to disconnect.
async fn session(hub: &Hub, config: &Config) -> anyhow::Result<()> {
    let url = url::Url::parse(&config.indigo_url)?;
    let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut sink, mut stream) = socket.split();

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(CMD_CAP);
    if !hub.attach_backend(cmd_tx).await {
        anyhow::bail!("another backend session is attached");
    }
    hub.reset_replay().await;
    info!(url = %url, "INDIGO session connected");

    let captures_dir = {
        let dir = config.resolved_captures_dir();
        dir.canonicalize().unwrap_or(dir).to_string_lossy().into_owned()
    };
    let indigo_host = url.host_str().unwrap_or("127.0.0.1").to_string();
    let indigo_port = url.port().unwrap_or(7624);

    let result = async {
        sink.send(Message::Text(protocol::get_properties_msg().into())).await?;

        // No Ekos profile to start: as soon as the server answers we are
        // "online". Cached so a browser opening later gets it on connect.
        let home = std::env::var("HOME").unwrap_or_default();
        let online = connection_state_msg(true, Some(true), Some(&home));
        *hub.last_connection_state.lock().await = Some(online.clone());
        let _ = hub.browser_tx.send(online);

        let mut store = PropertyStore::default();
        let mut roles = Roles::default();
        let mut mount_emitter = mount::MountEmitter::default();
        let mut imaging = imaging::Imaging::default();
        let (preview_tx, mut preview_rx) = mpsc::channel::<Preview>(4);
        let http = reqwest::Client::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                frame = stream.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            let Some(frame) = protocol::parse_frame(&text) else {
                                debug!(len = text.len(), "INDIGO frame not understood");
                                continue;
                            };
                            let Some(change) = store.apply(frame) else { continue };
                            // INDIGO devices come up disconnected and nothing
                            // else will connect them: do what Ekos' profile
                            // auto-connect does, as each one appears.
                            if let Change::Defined { device, name } = &change {
                                if name == "CONNECTION"
                                    && !store.connected(device)
                                    && store.interface(device) & protocol::interface::AGENT == 0
                                {
                                    let items = [("CONNECTED".to_string(), Value::Bool(true))];
                                    let w = protocol::new_vector_msg(protocol::Kind::Switch, device, "CONNECTION", &items);
                                    sink.send(Message::Text(w.into())).await?;
                                }
                            }
                            let mut msgs = events::for_change(&change, &store);
                            // A device appeared or vanished: the roles (and
                            // so the train the browser binds to) may change.
                            if matches!(change, Change::Defined { ref name, .. } if name == "INFO")
                                || matches!(change, Change::Deleted { .. })
                            {
                                let new_roles = Roles::derive(&store);
                                if new_roles != roles {
                                    roles = new_roles;
                                    msgs.push(commands::reply_msg("train_get_all", roles.train_payload()));
                                    msgs.push(commands::reply_msg("get_scopes", roles.scopes_payload(&store)));
                                }
                            }
                            if let Some(m) = roles.mount.as_deref() {
                                msgs.extend(mount_emitter.on_change(&change, &store, m));
                            }
                            if let Some(c) = roles.camera.as_deref() {
                                let r = imaging.on_change(&change, &store, c);
                                msgs.extend(r.to_browser);
                                for w in r.to_indigo {
                                    sink.send(Message::Text(w.into())).await?;
                                }
                                if let Some(link) = r.fetch_preview {
                                    spawn_preview_fetch(&http, &url, &link, preview_tx.clone());
                                }
                            }
                            for msg in msgs {
                                hub.capture(&msg).await;
                                let _ = hub.browser_tx.send(msg);
                            }
                            publish_site(hub, &change, &store).await;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            sink.send(Message::Pong(data)).await?;
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            info!("INDIGO closed the session");
                            return Ok(());
                        }
                        Some(Err(e)) => anyhow::bail!("socket error: {e}"),
                        _ => {}
                    }
                }

                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { return Ok(()) };
                    let Ok(cmd) = serde_json::from_str::<Value>(&cmd) else { continue };
                    let ctx = Ctx {
                        store: &store,
                        roles: &roles,
                        captures_dir: &captures_dir,
                        indigo_host: &indigo_host,
                        indigo_port,
                    };
                    for out in commands::handle(&cmd, &ctx, &mut imaging) {
                        match out {
                            Out::ToBrowser(msg) => {
                                remember_site(hub, &msg).await;
                                hub.capture(&msg).await;
                                let _ = hub.browser_tx.send(msg);
                            }
                            Out::ToIndigo(msg) => sink.send(Message::Text(msg.into())).await?,
                        }
                    }
                }

                // A preview JPEG arrived from the BLOB fetch.
                Some((jpeg, size)) = preview_rx.recv() => {
                    let msg = imaging.preview_msg(&jpeg, size);
                    hub.capture_preview(&msg).await;
                    let _ = hub.browser_tx.send(msg);
                }

                // Flush a throttled mount-coordinate update.
                _ = tick.tick() => {
                    if let Some(m) = roles.mount.as_deref() {
                        for msg in mount_emitter.tick(&store, m) {
                            hub.capture(&msg).await;
                            let _ = hub.browser_tx.send(msg);
                        }
                    }
                }
            }
        }
    }
    .await;

    hub.detach_backend().await;
    let _ = hub.browser_tx.send(connection_state_msg(false, None, None));
    info!("INDIGO session ended");
    result
}

/// A device's site changed: push the observer location like the Ekos path
/// does after its poll, and cache it for late-joining browsers.
async fn publish_site(hub: &Hub, change: &protocol::Change, store: &PropertyStore) {
    let touched = match change {
        protocol::Change::Defined { name, .. } | protocol::Change::Updated { name, .. } => {
            name == "GEOGRAPHIC_COORDINATES"
        }
        _ => false,
    };
    if !touched {
        return;
    }
    if let Some(site) = commands::site_payload(store) {
        let msg = commands::reply_msg("astro_get_location", site);
        let mut cached = hub.last_site_location.lock().await;
        if cached.as_deref() == Some(msg.as_str()) {
            return;
        }
        *cached = Some(msg.clone());
        drop(cached);
        let _ = hub.browser_tx.send(msg);
    }
}

async fn remember_site(hub: &Hub, msg: &str) {
    if msg.starts_with(r#"{"type":"astro_get_location""#) {
        *hub.last_site_location.lock().await = Some(msg.to_string());
    }
}

/// Fetch a BLOB link over HTTP, off the session loop. INDIGO gives either a
/// bare path (`/blob/0x….jpeg`) or an absolute URL; both resolve against the
/// server we're connected to, on its HTTP side.
fn spawn_preview_fetch(http: &reqwest::Client, indigo: &url::Url, link: &str, tx: mpsc::Sender<Preview>) {
    let mut base = indigo.clone();
    let _ = base.set_scheme(if indigo.scheme() == "wss" { "https" } else { "http" });
    let Ok(target) = base.join(link) else {
        warn!(link, "INDIGO BLOB link does not resolve");
        return;
    };
    let http = http.clone();
    tokio::spawn(async move {
        let bytes = match http.get(target.clone()).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.bytes().await {
                Ok(b) => b.to_vec(),
                Err(e) => { warn!(%target, "INDIGO BLOB body: {e}"); return; }
            },
            Err(e) => { warn!(%target, "INDIGO BLOB fetch: {e}"); return; }
        };
        let size = image::ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok())
            .unwrap_or((0, 0));
        let _ = tx.send((bytes, size)).await;
    });
}
