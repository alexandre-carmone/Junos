//! INDIGO backend: an Ekos Live emulation on top of an `indigo_server`.
//!
//! Where the Ekos backend relays KStars verbatim, this one dials *out* to an
//! INDIGO server (JSON over WebSocket), mirrors its properties
//! (`protocol.rs`), answers the browser's Ekos Live commands itself
//! (`commands.rs`) and turns property changes into Ekos Live events
//! (`events.rs`) — so the WASM client needs no second protocol.

mod agents;
mod alias;
mod align;
mod astro;
mod capture;
mod commands;
mod ephem;
mod events;
mod focus;
mod guide;
mod imaging;
mod indi_shape;
mod media;
mod mount;
mod process;
mod protocol;
mod roles;
mod scheduler;
mod scansexa;

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::hub::{connection_state_msg, Hub, CMD_CAP};

use agents::Agents;
use commands::{Ctx, Modules, Out};
use media::{Fetched, Source};
use protocol::{Change, PropertyStore};
use roles::Roles;
use scheduler::{queue::ScriptReq, ScriptDone};

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
        let mut modules = Modules::new(Instant::now());
        let mut media = media::Media::default();
        let (preview_tx, mut preview_rx) = mpsc::channel::<Fetched>(4);
        let (script_tx, mut script_rx) = mpsc::channel::<ScriptDone>(4);
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
                            let now = Instant::now();
                            let mut msgs = modules.agents.on_change(&change, &store, now);
                            msgs.extend(events::for_change(&change, &store));
                            // A device appeared or vanished, or an agent
                            // (de)selected one: the roles (and so the train
                            // the browser binds to) may change.
                            let roles_touched = match &change {
                                Change::Defined { name, .. } => name == "INFO" || name.starts_with("FILTER_"),
                                Change::Updated { name, .. } => name.starts_with("FILTER_"),
                                Change::Deleted { .. } => true,
                                Change::Message { .. } => false,
                            };
                            if roles_touched {
                                let new_roles = Roles::derive(&store, &modules.agents.set);
                                if new_roles != roles {
                                    roles = new_roles;
                                    msgs.push(commands::reply_msg("train_get_all", roles.train_payload()));
                                    msgs.push(commands::reply_msg("get_scopes", roles.scopes_payload(&store)));
                                }
                            }
                            if let Some(m) = roles.mount.as_deref() {
                                msgs.extend(mount_emitter.on_change(&change, &store, m));
                            }
                            // The modules: the camera for Imaging, the agents
                            // for Focus / Guide / Align.
                            let set = modules.agents.set.clone();
                            let mut reactions = Vec::new();
                            if let Some(c) = roles.camera.as_deref() {
                                reactions.push((Source::Imaging, modules.imaging.on_change(&change, &store, c)));
                            }
                            if let Some(a) = set.imager.as_deref() {
                                reactions.push((Source::Focus, modules.focus.on_change(&change, &store, a)));
                                reactions.push((Source::Imaging, modules.capture.on_change(&change, &store, a, now)));
                            }
                            if let Some(a) = set.guider.as_deref() {
                                reactions.push((Source::Guide, modules.guide.on_change(&change, &store, a)));
                            }
                            if let Some(a) = set.astrometry.as_deref() {
                                let rig = commands::rig(&modules.agents.set, &store, &roles);
                                reactions.push((Source::Align, modules.align.on_change(&change, &store, a, &rig, now)));
                            }
                            let mut module_msgs = Vec::new();
                            for (source, r) in reactions {
                                module_msgs.extend(r.to_browser.iter().cloned());
                                msgs.extend(r.to_browser);
                                for w in r.to_indigo {
                                    sink.send(Message::Text(w.into())).await?;
                                }
                                if let Some(link) = r.fetch_preview {
                                    spawn_preview_fetch(&http, &url, &link, source, preview_tx.clone());
                                }
                            }
                            // The Scheduler reads the modules' transitions.
                            let events = modules.capture.drain_events();
                            modules.scheduler.observe(&module_msgs, events);
                            if modules.scheduler.is_running() {
                                let ctx = Ctx { store: &store, roles: &roles, captures_dir: &captures_dir, home: &home, indigo_host: &indigo_host, indigo_port };
                                let (sctx, scheduler, mut drivers) = commands::sched(&ctx, &mut modules);
                                let r = scheduler.step(&sctx, &mut drivers);
                                for w in r.to_indigo {
                                    sink.send(Message::Text(w.into())).await?;
                                }
                                msgs.extend(r.to_browser);
                            }
                            // Keep the agents' device lists filled and the
                            // unclaimed devices connected.
                            if Agents::relevant(&change) {
                                let plan = modules.agents.plan(&store, &roles, now);
                                for w in plan.to_indigo {
                                    sink.send(Message::Text(w.into())).await?;
                                }
                                msgs.extend(plan.to_browser);
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
                        home: &home,
                        indigo_host: &indigo_host,
                        indigo_port,
                    };
                    for out in commands::handle(&cmd, &ctx, &mut modules) {
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
                Some(fetched) = preview_rx.recv() => {
                    let msg = media.preview_msg(&fetched);
                    hub.capture_preview(&msg).await;
                    let _ = hub.browser_tx.send(msg);
                    let after = match fetched.source {
                        Source::Align => modules.align.frame_delivered(fetched.size),
                        Source::Focus => {
                            modules.focus.frame_delivered(fetched.size);
                            Vec::new()
                        }
                        _ => Vec::new(),
                    };
                    for msg in after {
                        hub.capture(&msg).await;
                        let _ = hub.browser_tx.send(msg);
                    }
                }

                // Flush a throttled mount-coordinate update, and retry the
                // agent selections that are waiting on a backoff.
                _ = tick.tick() => {
                    if let Some(m) = roles.mount.as_deref() {
                        for msg in mount_emitter.tick(&store, m) {
                            hub.capture(&msg).await;
                            let _ = hub.browser_tx.send(msg);
                        }
                    }
                    let now = Instant::now();
                    let plan = modules.agents.plan(&store, &roles, now);
                    for w in plan.to_indigo {
                        sink.send(Message::Text(w.into())).await?;
                    }
                    let mut msgs: Vec<String> = plan.to_browser.into_iter().chain(modules.align.tick(now)).collect();
                    if let Some(a) = modules.agents.set.imager.clone() {
                        let r = modules.capture.tick(&store, &a, now);
                        for w in r.to_indigo {
                            sink.send(Message::Text(w.into())).await?;
                        }
                        msgs.extend(r.to_browser);
                    }
                    // The Scheduler: procedures, evaluation, stage timeouts.
                    let events = modules.capture.drain_events();
                    modules.scheduler.observe(&[], events);
                    if modules.scheduler.is_running() {
                        let ctx = Ctx { store: &store, roles: &roles, captures_dir: &captures_dir, home: &home, indigo_host: &indigo_host, indigo_port };
                        let (sctx, scheduler, mut drivers) = commands::sched(&ctx, &mut modules);
                        let mut r = scheduler.step(&sctx, &mut drivers);
                        let t = scheduler.tick(&sctx, &mut drivers);
                        r.to_indigo.extend(t.to_indigo);
                        r.to_browser.extend(t.to_browser);
                        r.scripts.extend(t.scripts);
                        for w in r.to_indigo {
                            sink.send(Message::Text(w.into())).await?;
                        }
                        msgs.extend(r.to_browser);
                        for req in r.scripts {
                            spawn_script(req, script_tx.clone());
                        }
                    }
                    for msg in msgs {
                        hub.capture(&msg).await;
                        let _ = hub.browser_tx.send(msg);
                    }
                }

                // A procedure's script ended.
                Some(done) = script_rx.recv() => {
                    modules.scheduler.script_finished(done);
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

/// Run a procedure's script — no arguments, nothing on stdin, like KStars'
/// `ScriptAction` — and report its end; killed after its timeout.
fn spawn_script(req: ScriptReq, tx: mpsc::Sender<ScriptDone>) {
    tokio::spawn(async move {
        let outcome = match tokio::process::Command::new(&req.path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Err(e) => Err(format!("cannot start {}: {e}", req.path.display())),
            Ok(child) => match tokio::time::timeout(req.timeout, child.wait_with_output()).await {
                Ok(Ok(out)) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.trim().is_empty() {
                        info!(script = %req.path.display(), "stderr: {}", err.trim());
                    }
                    out.status.code().ok_or_else(|| "killed by a signal".to_string())
                }
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("timed out".to_string()),
            },
        };
        let _ = tx.send(ScriptDone { id: req.id, outcome }).await;
    });
}

/// Fetch a BLOB link over HTTP, off the session loop. INDIGO gives either a
/// bare path (`/blob/0x….jpeg`) or an absolute URL; both resolve against the
/// server we're connected to, on its HTTP side.
fn spawn_preview_fetch(http: &reqwest::Client, indigo: &url::Url, link: &str, source: Source, tx: mpsc::Sender<Fetched>) {
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
        let _ = tx.send(Fetched { source, jpeg: bytes, size }).await;
    });
}
