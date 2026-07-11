//! `tgw-netsim` binary — thin CLI shell over [`tgw_netsim::run_proxy`]. OWNER: Twaha.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tgw_netsim::NetsimConfig;

#[derive(Parser)]
#[command(
    name = "tgw-netsim",
    about = "Deterministic lossy UDP proxy (test instrument)"
)]
struct Cli {
    /// Per-packet drop probability, 0.0..=1.0.
    #[arg(long, default_value_t = 0.25)]
    loss: f64,
    /// Per-packet corruption probability for survivors, 0.0..=1.0 (single-bit flip).
    #[arg(long, default_value_t = 0.0)]
    corrupt: f64,
    /// How often a burst-loss episode begins, in milliseconds.
    #[arg(long, default_value_t = 5000)]
    burst_every_ms: u64,
    /// Burst-loss episode length, in milliseconds.
    #[arg(long, default_value_t = 800)]
    burst_len_ms: u64,
    /// Token-bucket rate cap in bits per second.
    #[arg(long, default_value_t = 64_000)]
    rate: u64,
    /// Max added latency jitter, in milliseconds.
    #[arg(long, default_value_t = 40)]
    jitter_ms: u64,
    /// RNG seed for a reproducible drop pattern.
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Address to receive datagrams on (field → here).
    #[arg(long)]
    listen: SocketAddr,
    /// Address to forward survivors to (here → gateway).
    #[arg(long)]
    forward: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let cfg = NetsimConfig {
        loss: cli.loss,
        corrupt: cli.corrupt,
        burst_every: Duration::from_millis(cli.burst_every_ms),
        burst_len: Duration::from_millis(cli.burst_len_ms),
        rate_bps: cli.rate,
        jitter: Duration::from_millis(cli.jitter_ms),
        seed: cli.seed,
    };

    tgw_netsim::run_proxy(cfg, cli.listen, cli.forward).await
}
