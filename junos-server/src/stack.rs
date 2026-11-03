//! The live stack relay.
//!
//! KStars never sends the stacked image over Ekos Live: it writes
//! `<outputDirectory>/stacked_frame_NNNN.fits` after each sub
//! (`fitsview.cpp:803-821`) for the cloud server to pick up. This module is
//! that server. It learns the folder and the running state from the KStars
//! traffic passing through (`livestacker_get_all_settings`, the settings
//! echo, and `new_livestacker_state`), polls the folder once a second while
//! a stack runs, publishes each new stacked frame as a `+S` preview — built
//! like a media frame (`kstars_ws::stack_preview`), so the per-browser
//! filter, the lite variant and the replay all apply — and prunes the folder
//! to the newest two files, since KStars writes a full-size float FITS per
//! sub and never deletes one.
//!
//! Polling, not events: `complete` is emitted before the file is written,
//! and after a `restarted` KStars starts over at `stacked_frame_0000.fits`,
//! overwriting in place. So a frame is new when its (path, mtime, size)
//! changed, and it is published once that triple has held for two polls
//! (cfitsio writes to the final name, no temp + rename).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::files::fits_to_jpeg;
use crate::hub::{FrameKind, Hub};
use crate::kstars_ws::stack_preview;

/// Stacked frames kept in the output folder: the newest and the one before
/// it, which may still be the one being rendered.
const KEEP: usize = 2;
const POLL: Duration = Duration::from_secs(1);
/// Longest side of the published JPEG — KStars' own preview size.
const MAX_SIDE: u32 = 1920;
const QUALITY: u8 = 90;

/// One sighting of a stacked frame.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

impl Seen {
    /// The NNNN of `stacked_frame_NNNN.fits`.
    fn index(&self) -> u32 {
        index_of(&self.path).unwrap_or(0)
    }
}

#[derive(Default)]
struct Inner {
    active: bool,
    out_dir: Option<PathBuf>,
    /// The newest frame the previous poll saw.
    seen: Option<Seen>,
    /// The frame last published (or given up on).
    last_sent: Option<Seen>,
    /// Published frames this session, for the uuid.
    seq: u32,
    warned_missing: bool,
}

#[derive(Clone, Default)]
pub struct StackWatch(Arc<Mutex<Inner>>);

impl StackWatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a KStars text message: the settings echo carries the output
    /// folder, the state says when to watch.
    pub async fn observe(&self, text: &str, hub: &Hub) {
        if !text.contains("livestacker") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        let payload = &v["payload"];
        match v["type"].as_str() {
            Some("livestacker_get_all_settings") => {
                if let Some(dir) = payload["outputDirectory"].as_str().filter(|d| !d.is_empty()) {
                    self.0.lock().await.out_dir = Some(PathBuf::from(dir));
                }
            }
            Some("new_livestacker_state") => match payload["state"].as_str() {
                Some("started") => {
                    let mut g = self.0.lock().await;
                    g.active = true;
                    g.seen = None;
                    g.last_sent = None;
                    g.warned_missing = false;
                    hub.clear_preview(FrameKind::Stack).await;
                    match &g.out_dir {
                        Some(d) => info!("live stack: watching {}", d.display()),
                        None => warn!("live stack started with no outputDirectory — nothing to relay"),
                    }
                }
                Some("stopped" | "closed" | "initialized") => {
                    let mut g = self.0.lock().await;
                    if g.active {
                        info!("live stack: stopped");
                    }
                    g.active = false;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// KStars went away: nothing to watch.
    pub async fn detach(&self) {
        self.0.lock().await.active = false;
    }

    /// The polling task, for the life of the server.
    pub fn spawn(self, hub: Hub) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(POLL);
            loop {
                tick.tick().await;
                self.poll(&hub).await;
            }
        });
    }

    async fn poll(&self, hub: &Hub) {
        let dir = {
            let g = self.0.lock().await;
            if !g.active {
                return;
            }
            let Some(d) = g.out_dir.clone() else { return };
            d
        };
        let scan_dir = dir.clone();
        let frames = match tokio::task::spawn_blocking(move || list_stacked(&scan_dir)).await {
            Ok(Ok(f)) => f,
            Ok(Err(e)) => {
                let mut g = self.0.lock().await;
                if !g.warned_missing {
                    warn!("live stack: cannot read {}: {e}", dir.display());
                    g.warned_missing = true;
                }
                return;
            }
            Err(e) => {
                warn!("live stack: scan task failed: {e}");
                return;
            }
        };
        let newest = pick_newest(&frames);
        let publish = {
            let mut g = self.0.lock().await;
            let ready = ready_to_publish(g.seen.as_ref(), g.last_sent.as_ref(), newest.as_ref());
            g.seen = newest.clone();
            if ready {
                g.last_sent = newest.clone();
                g.seq += 1;
            }
            ready.then(|| g.seq).zip(newest)
        };
        let Some((seq, frame)) = publish else { return };

        let want_lite = hub.lite_wanted();
        let path = frame.path.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            fits_to_jpeg(&bytes, MAX_SIDE, QUALITY)
        })
        .await;
        let name = frame.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match rendered {
            Ok(Ok((jpeg, w, h))) => {
                info!("live stack: {name} → {w}×{h}, {} KB", jpeg.len() / 1024);
                let out = Arc::new(stack_preview(&jpeg, w, h, seq, &name, want_lite));
                hub.capture_preview(out.clone()).await;
                let _ = hub.browser_tx.send(out);
            }
            Ok(Err(e)) => warn!("live stack: {name} not rendered: {e}"),
            Err(e) => warn!("live stack: render task failed: {e}"),
        }

        // The newest two stay; the rest only cost disk.
        for victim in prune_victims(&frames, KEEP) {
            match std::fs::remove_file(&victim) {
                Ok(()) => debug!("live stack: pruned {}", victim.display()),
                Err(e) => warn!("live stack: could not prune {}: {e}", victim.display()),
            }
        }
    }
}

/// `stacked_frame_NNNN.fits` → NNNN.
fn index_of(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    name.strip_prefix("stacked_frame_")?.strip_suffix(".fits")?.parse().ok()
}

/// The stacked frames in `dir`, by name pattern only.
fn list_stacked(dir: &Path) -> std::io::Result<Vec<Seen>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if index_of(&path).is_none() {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        out.push(Seen { path, mtime: meta.modified()?, size: meta.len() });
    }
    Ok(out)
}

/// Newest by mtime, then by index — a `0000` rewritten after a restart beats
/// the older `0001` next to it.
fn pick_newest(frames: &[Seen]) -> Option<Seen> {
    frames.iter().max_by_key(|f| (f.mtime, f.index())).cloned()
}

/// Publish the candidate once it is the same as last poll's (the file is
/// no longer being written) and not what was already sent.
fn ready_to_publish(seen: Option<&Seen>, last_sent: Option<&Seen>, newest: Option<&Seen>) -> bool {
    match newest {
        Some(n) => seen == Some(n) && last_sent != Some(n),
        None => false,
    }
}

/// The frames beyond the `keep` newest, oldest last.
fn prune_victims(frames: &[Seen], keep: usize) -> Vec<PathBuf> {
    let mut sorted: Vec<&Seen> = frames.iter().collect();
    sorted.sort_by_key(|f| std::cmp::Reverse((f.mtime, f.index())));
    sorted.into_iter().skip(keep).map(|f| f.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(name: &str, secs: u64, size: u64) -> Seen {
        Seen {
            path: PathBuf::from(format!("/out/{name}")),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
            size,
        }
    }

    #[test]
    fn pick_newest_uses_mtime_then_index() {
        let a = seen("stacked_frame_0000.fits", 20, 10);
        let b = seen("stacked_frame_0001.fits", 10, 10);
        assert_eq!(pick_newest(&[b.clone(), a.clone()]), Some(a.clone()));
        let c = seen("stacked_frame_0002.fits", 20, 10);
        assert_eq!(pick_newest(&[a, c.clone()]), Some(c));
        assert_eq!(pick_newest(&[]), None);
        assert_eq!(index_of(Path::new("/out/frame_123.fits")), None);
        assert_eq!(index_of(Path::new("/out/stacked_frame_0042.fits")), Some(42));
    }

    #[test]
    fn publishes_once_when_stable() {
        let f = seen("stacked_frame_0000.fits", 10, 100);
        // First sighting: not yet stable.
        assert!(!ready_to_publish(None, None, Some(&f)));
        // Same again: publish.
        assert!(ready_to_publish(Some(&f), None, Some(&f)));
        // Already sent: no.
        assert!(!ready_to_publish(Some(&f), Some(&f), Some(&f)));
        // Rewritten in place (new size): wait one poll, then publish.
        let g = seen("stacked_frame_0000.fits", 12, 120);
        assert!(!ready_to_publish(Some(&f), Some(&f), Some(&g)));
        assert!(ready_to_publish(Some(&g), Some(&f), Some(&g)));
        assert!(!ready_to_publish(Some(&g), Some(&g), None));
    }

    #[test]
    fn prune_keeps_the_newest() {
        let frames = [
            seen("stacked_frame_0000.fits", 10, 1),
            seen("stacked_frame_0001.fits", 20, 1),
            seen("stacked_frame_0002.fits", 30, 1),
            seen("stacked_frame_0003.fits", 40, 1),
        ];
        let victims = prune_victims(&frames, 2);
        assert_eq!(victims, vec![PathBuf::from("/out/stacked_frame_0001.fits"), PathBuf::from("/out/stacked_frame_0000.fits")]);
        assert!(prune_victims(&frames[..2], 2).is_empty());
    }
}
