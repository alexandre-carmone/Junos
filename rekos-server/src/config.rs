use std::path::PathBuf;

use clap::Parser;

/// Rekos server configuration.
///
/// Architecture: KStars (Ekos Live client) connects *inbound* to us over plain
/// HTTP from the same machine. Browsers connect over HTTPS — required by iOS
/// Safari (and modern best practice on every browser) to expose
/// `navigator.gpu` for the WebGPU planetarium.
///
/// Both listeners share the same Router, so any consumer can use either port.
#[derive(Parser, Debug, Clone)]
#[command(name = "rekos-server", about = "Ekos Live server + WASM frontend host")]
pub struct Config {
    /// HTTP listen address — used by KStars (Ekos Live offline server).
    #[arg(long, default_value = "0.0.0.0:8080", env = "HTTP_ADDR")]
    pub http_addr: String,

    /// HTTPS listen address — used by the browser UI.
    #[arg(long, default_value = "0.0.0.0:8443", env = "HTTPS_ADDR")]
    pub https_addr: String,

    /// Path to the rekos-wasm dist directory to serve.
    #[arg(long, default_value = "rekos-wasm/dist", env = "DIST_DIR")]
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
}
