//! `tgw-gateway` binary — thin async shell. OWNER: Twaha.
//!
//! Loads the Contract-4 TOML config, opens the redb store, then runs the UDP decode
//! listener and the store-backed HTTP API (dashboard + live JSON) concurrently.

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = tgw_core::Config::load(&cli.config).context("load gateway config")?;
    let listen = config
        .net
        .listen_addr
        .parse()
        .context("parse gateway UDP listen address")?;
    let http_addr = config
        .net
        .http_addr
        .parse()
        .context("parse gateway HTTP address")?;
    let db = database_path(&cli.config)?;
    let key = tgw_core::Key::from_file(&config.crypto.key_file).context("load gateway key")?;

    let store = Arc::new(tgw_gateway::Store::open(&db).context("open gateway store")?);

    let state = tgw_gateway::StoreState {
        base: tgw_gateway::AppState {
            static_dir: cli.static_dir,
        },
        store: Arc::clone(&store),
    };

    // Run the UDP decode path and the HTTP API concurrently; neither blocks the other. If
    // either errors (e.g. UDP bind fails), `try_join!` cancels the other and propagates.
    let nack_timeout = std::time::Duration::from_millis(config.retry.nack_timeout_ms);
    let udp = tgw_gateway::run_udp_listener(listen, Arc::clone(&store), key, nack_timeout);
    let http = tgw_gateway::run_http_server_store(http_addr, state);
    let ((), ()) = tokio::try_join!(udp, http)?;
    Ok(())
}

/// Load the gateway-only storage extension and apply its environment override.
fn database_path(config_path: &std::path::Path) -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("TGW_DB_PATH") {
        return Ok(PathBuf::from(path));
    }
    let text = std::fs::read_to_string(config_path).context("read gateway storage config")?;
    let value: toml::Value = toml::from_str(&text).context("parse gateway storage config")?;
    let path = value
        .get("storage")
        .and_then(|storage| storage.get("db_path"))
        .and_then(toml::Value::as_str)
        .context("config [storage].db_path is required")?;
    Ok(PathBuf::from(path))
}
