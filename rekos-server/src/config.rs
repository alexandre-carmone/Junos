use clap::Parser;

/// Rekos server configuration.
///
/// Architecture: KStars (Ekos Live client) connects *inbound* to us.
/// We do not dial out to KStars. Configure KStars to point its Ekos Live
/// "offline" or "online" server URL at this server (e.g. `http://rekos.local:8080`).
#[derive(Parser, Debug, Clone)]
#[command(name = "rekos-server", about = "Ekos Live server + WASM frontend host")]
pub struct Config {
    /// Address to bind the HTTP/WebSocket server on
    #[arg(long, default_value = "0.0.0.0:3000", env = "BIND_ADDR")]
    pub bind_addr: String,

    /// Path to the rekos-wasm dist directory to serve
    #[arg(long, default_value = "rekos-wasm/dist", env = "DIST_DIR")]
    pub dist_dir: String,
}
