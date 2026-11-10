use clap::Parser;
use junos_guider::config::Config;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();
    let cfg = Config::parse();
    tracing::info!(port = cfg.event_server_port(), ws = %cfg.ws_addr, "junos-guider starting");
}
