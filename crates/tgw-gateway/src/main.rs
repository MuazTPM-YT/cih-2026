//! `tgw-gateway` binary — thin async shell. OWNER: Twaha.
//!
//! Loads the Contract-4 TOML config, opens the redb store, then runs the UDP decode
//! listener and the store-backed HTTP API (dashboard + live JSON) concurrently.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};

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
    /// Subcommand; absent ⇒ serve (receive bundles + dashboard).
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a pairing window: print a code + pairing string, run SPAKE2 over the public UDP
    /// port, and store the derived session key. Field runs `tgw-field pair "…"` against it.
    Pair,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let config = tgw_core::Config::load(&cli.config).context("load gateway config")?;

    match cli.command {
        Some(Cmd::Pair) => run_pairing(&config).await,
        None => run_serve(&cli, &config).await,
    }
}

/// Receive bundles + serve the dashboard (the default, pre-existing behaviour).
async fn run_serve(cli: &Cli, config: &tgw_core::Config) -> anyhow::Result<()> {
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

    // A paired session key (from `tgw-gateway pair`) wins over a configured key file.
    let key = match tgw_gateway::session::load(&tgw_gateway::session::default_path())? {
        Some(k) => {
            tracing::info!("using paired session key");
            k
        }
        None => {
            let key_path = config.crypto.key_file.clone().context(
                "no paired session and no [crypto].key_file — run `tgw-gateway pair` first",
            )?;
            tgw_core::Key::from_file(&key_path).context("load gateway key")?
        }
    };

    let store = Arc::new(tgw_gateway::Store::open(&db).context("open gateway store")?);
    let state = tgw_gateway::StoreState {
        base: tgw_gateway::AppState {
            static_dir: cli.static_dir.clone(),
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

/// Open a pairing window on the public UDP port, then persist the derived session key.
async fn run_pairing(config: &tgw_core::Config) -> anyhow::Result<()> {
    let bind: std::net::SocketAddr = config
        .net
        .listen_addr
        .parse()
        .context("parse gateway UDP listen address")?;
    let code = gen_code();
    // Advertise the port-forwarded public address if configured. Without it, the bind
    // address is often 0.0.0.0 — undialable — so substitute this machine's LAN IP and
    // say plainly that the string only works from the same network.
    let (public, lan_only) = match config.net.public_addr.clone() {
        Some(addr) => (addr, false),
        None => {
            let ip = if bind.ip().is_unspecified() {
                local_lan_ip().context(
                    "cannot determine this machine's address: [net].listen_addr is 0.0.0.0 and \
                     no [net].public_addr is set — set public_addr in the gateway config",
                )?
            } else {
                bind.ip()
            };
            (format!("{ip}:{}", bind.port()), true)
        }
    };
    println!("\n  Pairing code: {code}");
    println!("  Field runs:   tgw-field pair \"tgw1:{public}:{code}\"\n");
    if lan_only {
        println!(
            "  NOTE: {public} is this machine's local address — that pairing string works\n\
             from the SAME network only. For a field device on a DIFFERENT network (WAN):\n\
             port-forward UDP {} on your router to this machine, set\n\
             [net].public_addr = \"<your-public-ip>:{}\" in config/gateway.toml,\n\
             and re-run `tgw-gateway pair`.\n",
            bind.port(),
            bind.port()
        );
    }
    let key = tgw_gateway::pairing::run_pair_responder(bind, &code, Default::default()).await?;
    tgw_gateway::session::save(&tgw_gateway::session::default_path(), &key)?;
    println!("paired ✓  session key stored — now start the gateway (no subcommand) to receive");
    Ok(())
}

/// Best-effort local LAN address: UDP-connect to a public IP (no packet is sent for a
/// UDP connect) and read back the source address the kernel would route from.
fn local_lan_ip() -> Option<std::net::IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_unspecified() { None } else { Some(ip) }
}

/// Human pairing code: a digit + three short words from a small wordlist (~44 bits of entropy).
fn gen_code() -> String {
    use rand::Rng;
    use rand::seq::SliceRandom;
    const WORDS: &[&str] = &[
        "otter", "cobalt", "maple", "harbor", "ember", "quartz", "willow", "pilot", "raven",
        "cedar", "onyx", "delta", "lotus", "falcon", "amber", "slate",
    ];
    let mut rng = rand::thread_rng();
    let pick = |rng: &mut rand::rngs::ThreadRng| *WORDS.choose(rng).unwrap_or(&"otter");
    format!(
        "{}-{}-{}-{}",
        rng.gen_range(1..=9),
        pick(&mut rng),
        pick(&mut rng),
        pick(&mut rng)
    )
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
