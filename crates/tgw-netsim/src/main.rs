//! `tgw-netsim` binary — CLI shell over [`tgw_netsim::run_proxy`]. OWNER: Twaha.
//!
//! With `--control-http` it also serves a tiny JSON control API so a dashboard slider can raise
//! or lower packet loss / corruption / bandwidth **while traffic is flowing** — the loss the
//! field client actually experiences changes in real time.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tgw_netsim::{LinkControls, NetsimConfig, run_proxy_controlled};
use tower_http::cors::{Any, CorsLayer};

#[derive(Parser)]
#[command(
    name = "tgw-netsim",
    about = "Deterministic lossy UDP proxy (test instrument) with live control"
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
    /// Optional HTTP address for the live control API (`GET`/`POST /api/link`). When set, the
    /// dashboard can adjust loss/corruption/bandwidth at runtime.
    #[arg(long)]
    control_http: Option<SocketAddr>,
}

/// Current link state returned by `GET /api/link` and echoed by `POST /api/link`.
#[derive(Serialize)]
struct LinkState {
    loss: f64,
    corrupt: f64,
    rate_bps: u64,
}

/// Partial update for `POST /api/link` — any omitted field is left unchanged.
#[derive(Deserialize)]
struct LinkUpdate {
    loss: Option<f64>,
    corrupt: Option<f64>,
    rate_bps: Option<u64>,
}

fn snapshot(controls: &LinkControls) -> LinkState {
    LinkState {
        loss: controls.loss(),
        corrupt: controls.corrupt(),
        rate_bps: controls.rate_bps(),
    }
}

async fn get_link(State(controls): State<LinkControls>) -> Json<LinkState> {
    Json(snapshot(&controls))
}

async fn post_link(
    State(controls): State<LinkControls>,
    Json(update): Json<LinkUpdate>,
) -> Result<Json<LinkState>, StatusCode> {
    if let Some(v) = update.loss {
        if !v.is_finite() {
            return Err(StatusCode::BAD_REQUEST);
        }
        controls.set_loss(v);
    }
    if let Some(v) = update.corrupt {
        if !v.is_finite() {
            return Err(StatusCode::BAD_REQUEST);
        }
        controls.set_corrupt(v);
    }
    if let Some(v) = update.rate_bps {
        controls.set_rate_bps(v);
    }
    tracing::info!(
        loss = controls.loss(),
        corrupt = controls.corrupt(),
        rate_bps = controls.rate_bps(),
        "netsim: link adjusted via control API"
    );
    Ok(Json(snapshot(&controls)))
}

async fn run_control_server(addr: SocketAddr, controls: LinkControls) -> Result<()> {
    // The dashboard posts from another origin, so allow cross-origin GET + POST on the control API.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers(Any);
    let app = Router::new()
        .route("/api/link", get(get_link).post(post_link))
        .layer(cors)
        .with_state(controls);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("netsim: bind control API {addr}"))?;
    tracing::info!(%addr, "netsim: live control API up (GET/POST /api/link)");
    axum::serve(listener, app)
        .await
        .context("netsim: control API server")?;
    Ok(())
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
    let controls = LinkControls::from_config(&cfg);

    match cli.control_http {
        Some(addr) => {
            // Proxy and control API share the same `controls`, so a POST changes live traffic.
            let proxy = run_proxy_controlled(cfg, cli.listen, cli.forward, controls.clone());
            let server = run_control_server(addr, controls);
            tokio::try_join!(proxy, server)?;
            Ok(())
        }
        None => run_proxy_controlled(cfg, cli.listen, cli.forward, controls).await,
    }
}
