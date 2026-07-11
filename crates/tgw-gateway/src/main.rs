//! `tgw-gateway` binary — thin async shell. OWNER: Twaha.
//!
//! Scaffold: starts the HTTP server (serving the dashboard + mock fixtures). Wire up config
//! parsing (Phase F) and the UDP listener (Phase B) at the marked TODOs.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "tgw-gateway",
    about = "Low-bandwidth telemedicine gateway (receiver)"
)]
struct Cli {
    /// Path to the gateway TOML config (Contract 4).
    #[arg(long, default_value = "config/gateway.toml")]
    config: PathBuf,
    /// Directory of static dashboard files (Jiya's `static/`).
    #[arg(long, default_value = "crates/tgw-gateway/static")]
    static_dir: PathBuf,
    /// HTTP bind address for the dashboard/API.
    #[arg(long, default_value = "0.0.0.0:8080")]
    http_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    // TODO(PHASE-F): load `cli.config` into a typed GatewayConfig (TOML + env overrides).
    let _ = &cli.config;

    let state = tgw_gateway::AppState {
        static_dir: cli.static_dir,
    };

    // TODO(PHASE-B): also spawn the UDP listener, e.g.
    //   tokio::spawn(tgw_gateway::run_udp_listener(listen_addr));
    tgw_gateway::run_http_server(cli.http_addr, state).await
}
