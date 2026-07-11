//! `tgw-gateway` binary — thin async shell. OWNER: Twaha.
//!
//! Scaffold: starts the HTTP server (serving the dashboard + mock fixtures). Wire up config
//! parsing (Phase F) and the UDP listener (Phase B) at the marked TODOs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
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
    /// UDP bind address for incoming `DATA` frames (Phase-B shortcut; Phase F loads it from
    /// the TOML config's `[net] listen_addr`).
    #[arg(long, default_value = "0.0.0.0:47000")]
    listen: SocketAddr,
    /// Path to the redb database file (Phase-E shortcut; Phase F loads it from config).
    #[arg(long, default_value = "gateway.redb")]
    db: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    // TODO(PHASE-F): load `cli.config` into a typed GatewayConfig (TOML + env overrides) and
    // derive `listen`/`http_addr`/`key_file`/db path from it instead of the CLI shortcuts.
    let _ = &cli.config;

    let store = Arc::new(tgw_gateway::Store::open(&cli.db).context("open gateway store")?);

    let state = tgw_gateway::StoreState {
        base: tgw_gateway::AppState {
            static_dir: cli.static_dir,
        },
        store: Arc::clone(&store),
    };

    // Run the UDP decode path and the HTTP API concurrently; neither blocks the other. If
    // either errors (e.g. UDP bind fails), `try_join!` cancels the other and propagates.
    let udp = tgw_gateway::run_udp_listener(cli.listen, Arc::clone(&store));
    let http = tgw_gateway::run_http_server_store(cli.http_addr, state);
    let ((), ()) = tokio::try_join!(udp, http)?;
    Ok(())
}
