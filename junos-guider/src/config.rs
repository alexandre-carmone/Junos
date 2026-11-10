//! Command-line configuration (clap, env-aware like `junos-server`).

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "junos-guider", about = "PHD2-compatible autoguider for junos-web")]
pub struct Config {
    /// INDI server host.
    #[arg(long, env = "INDI_HOST", default_value = "localhost")]
    pub indi_host: String,

    /// INDI server port.
    #[arg(long, env = "INDI_PORT", default_value_t = 7624)]
    pub indi_port: u16,

    /// PHD2 instance number: the event server listens on 4400 + instance - 1
    /// and the config file is suffixed with the instance, like PHD2's `-i`.
    #[arg(long, env = "GUIDER_INSTANCE", default_value_t = 1)]
    pub instance: u32,

    /// Event-server port override (0 = an ephemeral port, for the tests).
    #[arg(long, env = "GUIDER_SERVER_PORT")]
    pub server_port: Option<u16>,

    /// Address of the local WebSocket the junos-server relay dials.
    #[arg(long, env = "GUIDER_WS_ADDR", default_value = "127.0.0.1:4410")]
    pub ws_addr: String,

    /// Where profiles, darks and defect maps live (default: XDG config/data dirs).
    #[arg(long, env = "GUIDER_DATA_DIR")]
    pub data_dir: Option<String>,

    /// Where the guide and debug logs go (default: ~/Documents/PHD2, like PHD2).
    #[arg(long, env = "GUIDER_LOG_DIR")]
    pub log_dir: Option<String>,

    /// Run against the built-in gear simulator instead of INDI (needs the
    /// `sim` feature).
    #[arg(long, env = "GUIDER_SIM", default_value_t = false)]
    pub sim: bool,

    /// Exit the process on the `shutdown` RPC (PHD2 behaviour). Off by
    /// default so a service install keeps serving after NINA's "shut down".
    #[arg(long, env = "GUIDER_SHUTDOWN_EXITS", default_value_t = false)]
    pub shutdown_exits: bool,
}

impl Config {
    /// The event-server port: `--server-port` or `4400 + instance - 1`.
    pub fn event_server_port(&self) -> u16 {
        self.server_port.unwrap_or(4400 + self.instance.saturating_sub(1) as u16)
    }
}
