//! Command-line configuration (clap, env-aware like `junos-server`).

use std::path::PathBuf;

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
    /// Up to 61126, the last that gives the relay's socket a port too.
    #[arg(long, env = "GUIDER_INSTANCE", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(0..=61126))]
    pub instance: u32,

    /// Event-server port override (0 = an ephemeral port, for the tests).
    #[arg(long, env = "GUIDER_SERVER_PORT")]
    pub server_port: Option<u16>,

    /// Address of the local WebSocket the junos-server relay dials
    /// (default: 127.0.0.1, port 4410 + instance - 1).
    #[arg(long, env = "GUIDER_WS_ADDR")]
    pub ws_addr: Option<String>,

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

    /// Replace every setting with a file's (written by --save or PHD2's
    /// `-s`), then exit: PHD2's `-l`.
    #[arg(long, value_name = "FILE")]
    pub load: Option<PathBuf>,

    /// Write every setting to a file, then exit: PHD2's `-s`. Wins over
    /// --load.
    #[arg(long, value_name = "FILE")]
    pub save: Option<PathBuf>,

    /// Reset every setting to its default before starting, deleting the
    /// profiles' dark libraries and defect maps: PHD2's `-R`.
    #[arg(long, default_value_t = false)]
    pub reset: bool,
}

impl Config {
    /// The event-server port: `--server-port` or `4400 + instance - 1`
    /// (event_server.cpp:2588), so instance 0 listens on 4399 as PHD2's does.
    pub fn event_server_port(&self) -> u16 {
        self.server_port.unwrap_or((4399 + self.instance) as u16)
    }

    /// The relay's socket: `--ws-addr`, or `127.0.0.1:(4410 + instance -
    /// 1)`, so that two instances do not collide.
    pub fn ws_addr(&self) -> String {
        self.ws_addr.clone().unwrap_or_else(|| format!("127.0.0.1:{}", 4409 + self.instance))
    }

    /// PHD2's default file folder (`GetDefaultFileDir`, where `save_image`
    /// writes): `--data-dir`, else `$XDG_DATA_HOME/junos-guider`, else
    /// `~/.local/share/junos-guider`.
    pub fn data_path(&self) -> PathBuf {
        if let Some(dir) = &self.data_dir {
            return PathBuf::from(dir);
        }
        let xdg = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
        let base = xdg
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("junos-guider")
    }

    /// Where the dark libraries and defect maps live: the data folder's
    /// `darks`.
    pub fn darks_dir(&self) -> PathBuf {
        self.data_path().join("darks")
    }

    /// Where the profile store lives: `--data-dir`, else
    /// `$XDG_CONFIG_HOME/junos-guider`, else `~/.config/junos-guider`.
    pub fn config_dir(&self) -> PathBuf {
        if let Some(dir) = &self.data_dir {
            return PathBuf::from(dir);
        }
        let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute());
        let base = xdg
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("junos-guider")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ports() {
        let port = |args: &[&str]| Config::parse_from([&["junos-guider"][..], args].concat()).event_server_port();
        assert_eq!(port(&[]), 4400);
        assert_eq!(port(&["--instance", "2"]), 4401);
        assert_eq!(port(&["--instance", "0"]), 4399);
        assert_eq!(port(&["--instance", "61126"]), 65525);
        assert_eq!(port(&["--instance", "3", "--server-port", "0"]), 0);
        assert_eq!(port(&["--server-port", "4500"]), 4500);
        assert!(Config::try_parse_from(["junos-guider", "--instance", "61127"]).is_err());
        let ws = |args: &[&str]| Config::parse_from([&["junos-guider"][..], args].concat()).ws_addr();
        assert_eq!(ws(&[]), "127.0.0.1:4410");
        assert_eq!(ws(&["--instance", "2"]), "127.0.0.1:4411");
        assert_eq!(ws(&["--instance", "61126"]), "127.0.0.1:65535");
        assert_eq!(ws(&["--instance", "2", "--ws-addr", "0.0.0.0:5000"]), "0.0.0.0:5000");
    }
}
