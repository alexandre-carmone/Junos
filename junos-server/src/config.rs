//! Command-line and environment configuration (clap).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Which device-control stack sits behind the relay. The browser speaks Ekos
/// Live either way; see the `indigo` module for the translation.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// KStars/Ekos connects *inbound* to `/message/ekos` and `/media/ekos`;
    /// messages are relayed verbatim.
    Ekos,
    /// We connect *outbound* to an `indigo_server` and emulate Ekos Live on
    /// top of its properties. No KStars involved.
    Indigo,
}

impl Backend {
    /// Wire name, as reported by `/api/config`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ekos => "ekos",
            Self::Indigo => "indigo",
        }
    }
}

/// Junos server configuration.
///
/// Architecture: KStars (Ekos Live client) connects *inbound* to us over plain
/// HTTP from the same machine. Browsers connect over HTTPS — required by iOS
/// Safari (and modern best practice on every browser) to expose
/// `navigator.gpu` for the WebGPU planetarium.
///
/// Both listeners share the same Router, so any consumer can use either port.
#[derive(Parser, Debug, Clone)]
#[command(name = "junos-server", about = "Ekos Live server + WASM frontend host")]
pub struct Config {
    /// Device-control backend: `ekos` (KStars connects to us) or `indigo`
    /// (we connect to an INDIGO server, see --indigo-url).
    #[arg(long, value_enum, default_value_t = Backend::Ekos, env = "BACKEND")]
    pub backend: Backend,

    /// WebSocket URL of the INDIGO server (`--backend indigo` only). INDIGO
    /// speaks JSON over a WebSocket upgrade on its usual TCP port.
    #[arg(long, default_value = "ws://127.0.0.1:7624/", env = "INDIGO_URL")]
    pub indigo_url: String,

    /// HTTP listen address — used by KStars (Ekos Live offline server) with
    /// `--backend ekos`; otherwise just a plain-HTTP copy of the browser UI.
    #[arg(long, default_value = "0.0.0.0:8080", env = "HTTP_ADDR")]
    pub http_addr: String,

    /// HTTPS listen address — used by the browser UI.
    #[arg(long, default_value = "0.0.0.0:8443", env = "HTTPS_ADDR")]
    pub https_addr: String,

    /// Path to the junos-web dist directory to serve.
    #[arg(long, default_value = "junos-web/dist", env = "DIST_DIR")]
    pub dist_dir: String,

    /// Override the auto-managed PEM-encoded TLS certificate.
    /// When supplied together with --tls-key, used as-is; otherwise the
    /// server reuses or generates a self-signed cert under .certs/.
    #[arg(long, env = "TLS_CERT")]
    pub tls_cert: Option<PathBuf>,

    /// Override the auto-managed PEM-encoded TLS private key. See --tls-cert.
    #[arg(long, env = "TLS_KEY")]
    pub tls_key: Option<PathBuf>,

    /// Skip the HTTPS listener entirely (HTTP-only mode for CI / headless).
    #[arg(long, env = "NO_HTTPS")]
    pub no_https: bool,

    /// Root directory served by the `/api/files/*` browser. Browser requests
    /// are sandboxed inside this folder. If unset, falls back to $HOME/Pictures
    /// and finally to the current working directory.
    #[arg(long, env = "CAPTURES_DIR")]
    pub captures_dir: Option<PathBuf>,

    /// Directory holding the offline DSO survey tiles served at
    /// `/api/dso_tiles/*`, as written by `scripts/prefetch_dso_tiles.py`.
    /// Defaults to `.cache/dso_tiles` next to the working directory. The
    /// directory is optional — without it the Framing Assistant previews
    /// uncovered sky as black.
    #[arg(long, env = "DSO_TILE_DIR")]
    pub dso_tile_dir: Option<PathBuf>,

    /// KStars task-queue root written by the Scheduler's startup/shutdown
    /// queue editor (`/api/taskqueue/*`): collections go in `collections/`,
    /// their shell scripts in `scripts/`. Defaults to KStars' own
    /// `$XDG_DATA_HOME/kstars/taskqueue` (else `~/.local/share/kstars/taskqueue`),
    /// so KStars' Collections dialog lists the same files. Point it at
    /// `~/.var/app/org.kde.kstars/data/kstars/taskqueue` for Flatpak KStars.
    #[arg(long, env = "TASKQUEUE_DIR")]
    pub taskqueue_dir: Option<PathBuf>,
}

impl Config {
    /// Resolve the effective captures root: flag → $HOME/Pictures → cwd.
    pub fn resolved_captures_dir(&self) -> PathBuf {
        if let Some(p) = &self.captures_dir {
            return p.clone();
        }
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(home).join("Pictures");
            if p.is_dir() {
                return p;
            }
        }
        PathBuf::from(".")
    }

    /// Resolve the offline DSO tile cache: flag → `.cache/dso_tiles`.
    pub fn resolved_dso_tile_dir(&self) -> PathBuf {
        self.dso_tile_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(".cache").join("dso_tiles"))
    }

    /// Resolve the KStars task-queue root:
    /// flag → `$XDG_DATA_HOME/kstars/taskqueue` → `$HOME/.local/share/kstars/taskqueue`
    /// → cwd. Always absolute — KStars loads the collection paths we hand it
    /// through `QUrl::fromUserInput`, which turns a relative path into a URL.
    pub fn resolved_taskqueue_dir(&self) -> PathBuf {
        let dir = if let Some(p) = &self.taskqueue_dir {
            p.clone()
        } else if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            PathBuf::from(xdg).join("kstars").join("taskqueue")
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".local/share/kstars/taskqueue")
        } else {
            PathBuf::from(".local/share/kstars/taskqueue")
        };
        if dir.is_absolute() {
            dir
        } else {
            std::env::current_dir().map(|cwd| cwd.join(&dir)).unwrap_or(dir)
        }
    }
}
