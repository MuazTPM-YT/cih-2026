//! `tgw-field` — the lightweight field-client binary (docs/ARCHITECTURE.md).
//!
//! Subcommands: `keygen`, `send-vitals`, `send-image`, `status [--watch]`, `daemon`.
//! Everything a field worker sees flows through here; the certainty story
//! (`queued → sending → delivered ✓`, or a loud `STUCK`) is printed, never hidden.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tgw_core::{Bundle, BundleSender, Config, FecConfig, Key, RetryConfig};
use tokio::net::UdpSocket;
use tower_http::services::ServeDir;

use tgw_field::breaker::{BreakerEvent, LinkBreaker, probe_budget};
use tgw_field::discovery::{PeerTable, run_discovery};
use tgw_field::pacer::Pacer;
use tgw_field::queue::{BundleState, Queue, QueuedBundle};
use tgw_field::relay::{relay_failover, run_relay_service};
use tgw_field::sender::{Outcome, deliver};
use tgw_field::vitals::{VitalsInput, build_observations};

/// Instantaneous burst allowance for the pacer (matches the demo's `tbf burst 8kb`).
const PACER_BURST_BYTES: usize = 8 * 1024;

/// FEC overhead used when re-framing a bundle for the peer relay: higher than the direct
/// default so the relayed burst decodes at the gateway in one shot (the relay peer forwards
/// opaque bytes and cannot mint repair symbols).
const RELAY_OVERHEAD_FACTOR: f32 = 2.0;

#[derive(Parser)]
#[command(
    name = "tgw-field",
    about = "Low-bandwidth telemedicine field client: store-and-forward over RaptorQ/UDP",
    version
)]
struct Cli {
    /// Path to the Contract 4 TOML config.
    #[arg(long, global = true, default_value = "config/field.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a fresh 256-bit pre-shared key (hex). Write it to the path in
    /// `[crypto].key_file` on BOTH devices; never commit it.
    Keygen {
        /// Write to this file (0600) instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Capture vitals and deliver them (vitals preempt any queued image).
    SendVitals {
        /// Blood pressure as `SYS/DIA` in mmHg, e.g. `142/95`.
        #[arg(long)]
        bp: Option<String>,
        /// Oxygen saturation in percent, e.g. `91`.
        #[arg(long)]
        spo2: Option<f64>,
        /// Pulse in beats per minute, e.g. `108`.
        #[arg(long)]
        pulse: Option<f64>,
        /// Patient identifier, e.g. `P-1023`.
        #[arg(long)]
        patient: String,
        /// Capturing device id.
        #[arg(long, default_value = "field-device")]
        device: String,
        /// Field worker id (FHIR performer).
        #[arg(long, default_value = "field-worker")]
        performer: String,
    },
    /// Queue and deliver an image (pre-sized JPEG/PNG ≤ media.image_max_bytes).
    SendImage {
        /// Image file to send.
        path: PathBuf,
        /// Patient identifier the image belongs to.
        #[arg(long)]
        patient: String,
        /// MIME type; guessed from the extension when omitted.
        #[arg(long)]
        mime: Option<String>,
    },
    /// Show the queue: per-bundle state, retries, age. The certainty view.
    Status {
        /// Refresh every second until interrupted.
        #[arg(long)]
        watch: bool,
    },
    /// Move STUCK bundle(s) back to queued so the daemon retries them (Fix F1). Pass a bundle
    /// id (full or 8-char short prefix) or `--all`.
    Requeue {
        /// The bundle id to requeue (full UUID, or its 8-char `status` short id).
        id: Option<String>,
        /// Requeue every stuck bundle.
        #[arg(long)]
        all: bool,
    },
    /// Pair with a hospital across the internet using a code it displays (no key files).
    /// Runs SPAKE2 over UDP and stores the derived session key + hospital address locally.
    Pair {
        /// The pairing string the hospital shows: `tgw1:<host:port>:<code>`.
        pairing_string: String,
    },
    /// Run continuously: drain the queue, resume interrupted transfers, serve NACKs.
    Daemon,
    /// Serve the browser field-capture UI and bridge its captures onto the REAL send path.
    /// A `POST /api/capture` seals + RaptorQ-encodes + sends over UDP to the gateway exactly
    /// like `send-vitals`, so the frontend drives the true store-and-forward path (not a mock).
    Serve {
        /// HTTP bind for the UI + bridge API.
        #[arg(long, default_value = "127.0.0.1:8091")]
        http: String,
        /// Directory of the field-capture UI to serve.
        #[arg(long, default_value = "field-ui")]
        ui_dir: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Keygen { out } => keygen(out.as_deref()),
        Command::SendVitals {
            bp,
            spo2,
            pulse,
            patient,
            device,
            performer,
        } => {
            let input = VitalsInput {
                bp,
                spo2,
                pulse,
                patient,
                device,
                performer,
            };
            let observations = build_observations(&input)?;
            let bundle = Bundle::new_vitals(observations);
            enqueue_and_drain(&cli.config, bundle).await
        }
        Command::SendImage {
            path,
            patient,
            mime,
        } => {
            let bundle = image_bundle(&cli.config, &path, &patient, mime)?;
            enqueue_and_drain(&cli.config, bundle).await
        }
        Command::Pair { pairing_string } => pair_cmd(&pairing_string).await,
        Command::Status { watch } => status(&cli.config, watch).await,
        Command::Requeue { id, all } => requeue_cmd(id, all),
        Command::Daemon => daemon(&cli.config).await,
        Command::Serve { http, ui_dir } => serve(&cli.config, &http, ui_dir).await,
    }
}

/// Fix F1 — move stuck bundle(s) back to queued so the daemon retries them. Local queue edit
/// only; needs no config or key.
fn requeue_cmd(id: Option<String>, all: bool) -> Result<()> {
    let queue = Queue::open(&queue_path())?;
    if all {
        let n = queue.requeue_all_stuck()?;
        println!("requeued {n} stuck bundle(s) — start `tgw-field daemon` to deliver them");
        return Ok(());
    }
    let Some(needle) = id else {
        bail!("provide a bundle id (full UUID or 8-char short id) or `--all`");
    };
    let target = resolve_bundle_id(&queue, &needle)?;
    if queue.requeue(target)? {
        println!(
            "bundle {} requeued — start `tgw-field daemon` to deliver it",
            short_id(target)
        );
        Ok(())
    } else {
        bail!(
            "bundle {} is not STUCK — nothing to requeue",
            short_id(target)
        )
    }
}

/// Resolve a full UUID or an 8-char `status` short id against the queue to a single bundle id.
fn resolve_bundle_id(queue: &Queue, needle: &str) -> Result<uuid::Uuid> {
    if let Ok(uuid) = uuid::Uuid::parse_str(needle) {
        return Ok(uuid);
    }
    let matches: Vec<uuid::Uuid> = queue
        .list()?
        .into_iter()
        .map(|r| r.id)
        .filter(|id| short_id(*id) == needle || id.to_string().starts_with(needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => bail!("no bundle matches id {needle:?} (see `tgw-field status`)"),
        _ => bail!(
            "id {needle:?} is ambiguous — {} bundles match",
            matches.len()
        ),
    }
}

fn keygen(out: Option<&Path>) -> Result<()> {
    let key = Key::generate();
    let hex = key.to_hex();
    match out {
        None => {
            println!("{hex}");
            eprintln!("(store this in [crypto].key_file on both devices; never commit it)");
        }
        Some(path) => {
            std::fs::write(path, format!("{hex}\n"))
                .with_context(|| format!("writing key file {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .with_context(|| format!("chmod 600 {}", path.display()))?;
            }
            eprintln!(
                "wrote key to {} (mode 0600); never commit it",
                path.display()
            );
        }
    }
    Ok(())
}

/// Where the persistent queue lives. `TGW_QUEUE_PATH` overrides for tests/multi-device
/// setups; the default sits next to the process and is `.gitignore`d.
fn queue_path() -> PathBuf {
    std::env::var("TGW_QUEUE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("field-queue.redb"))
}

/// Resolve the effective (key, gateway target) for the send path: a paired session (from
/// `tgw-field pair`) wins over the config's `key_file` + `gateway_addr`, so the cross-LAN mode
/// needs no key file. Falls back to the LAN path when no session exists.
fn resolve_key_and_target(config: &Config) -> Result<(Key, String)> {
    if let Some(session) = tgw_field::session::load(&tgw_field::session::default_path())? {
        return Ok((session.key, session.hospital_addr));
    }
    let key_path = config.crypto.key_file.clone().context(
        "no paired session and no [crypto].key_file — run `tgw-field pair \"tgw1:…\"` first, \
         or generate a key with `tgw-field keygen`",
    )?;
    let key = Key::from_file(&key_path).context("loading PSK")?;
    Ok((key, config.net.gateway_addr.clone()))
}

/// Parse a pairing string `tgw1:<host:port>:<code>` into (addr, code).
fn parse_pairing_string(s: &str) -> Result<(String, String)> {
    let rest = s
        .strip_prefix("tgw1:")
        .context("pairing string must start with `tgw1:`")?;
    // Format is exactly `tgw1:HOST:PORT:CODE`; the code follows the last ':' after the port.
    let (addr, code) = rest.rsplit_once(':').context("pairing string missing code")?;
    if addr.parse::<SocketAddr>().is_err() {
        bail!("pairing string address {addr:?} is not host:port");
    }
    if code.is_empty() {
        bail!("pairing string has an empty code");
    }
    Ok((addr.to_string(), code.to_string()))
}

/// Pair with a hospital: run SPAKE2 over UDP, then persist the derived session key + address.
async fn pair_cmd(pairing_string: &str) -> Result<()> {
    let (addr, code) = parse_pairing_string(pairing_string)?;
    println!("pairing with hospital at {addr} …");
    let key = tgw_field::pairing::pair_with_hospital(&addr, &code, Duration::from_secs(60)).await?;
    let path = tgw_field::session::default_path();
    tgw_field::session::save(
        &path,
        &tgw_field::session::Session {
            hospital_addr: addr,
            key,
        },
    )?;
    println!(
        "paired ✓  session saved to {} — now run `tgw-field daemon`",
        path.display()
    );
    Ok(())
}

fn image_bundle(
    config_path: &Path,
    image_path: &Path,
    patient: &str,
    mime: Option<String>,
) -> Result<Bundle> {
    let config = Config::load(config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let data = std::fs::read(image_path)
        .with_context(|| format!("reading image {}", image_path.display()))?;
    if data.len() > config.media.image_max_bytes {
        // JPEG recompression is deliberately out of scope: the `image` crate is
        // unverified in docs/RESEARCH_LOG.md, so we accept pre-sized files and say so.
        bail!(
            "image is {} bytes but media.image_max_bytes is {} — pre-size it, e.g.:\n  \
             magick {} -resize 800x800 -quality 60 smaller.jpg",
            data.len(),
            config.media.image_max_bytes,
            image_path.display()
        );
    }
    let mime = mime.unwrap_or_else(|| {
        match image_path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("jpg" | "jpeg") => "image/jpeg".to_string(),
            Some("png") => "image/png".to_string(),
            _ => "application/octet-stream".to_string(),
        }
    });
    Ok(Bundle::new_image(mime, data, patient.to_string()))
}

/// Enqueue `bundle`, then drain the queue (highest priority first) until empty or stuck.
/// Exits non-zero if THIS bundle did not reach `delivered` — the field worker must know.
async fn enqueue_and_drain(config_path: &Path, bundle: Bundle) -> Result<()> {
    let config = Config::load(config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let (key, target) = resolve_key_and_target(&config)?;
    let queue = Queue::open(&queue_path())?;

    let record = QueuedBundle::from_bundle(&bundle, &key)?;
    queue.enqueue(&record)?;
    println!("bundle {}  [{}]  queued", short_id(bundle.id), record.kind);

    // One-shot send uses the direct link only (no time to discover peers); the daemon runs
    // discovery and enables relay failover.
    drain_queue(&config, &key, &target, &queue, None).await?;

    match queue.get(bundle.id)?.map(|r| r.state) {
        Some(BundleState::Delivered) => Ok(()),
        other => bail!(
            "bundle {} did not reach delivered (state: {}); it is KEPT in the queue — \
             retry with `tgw-field daemon` or check the link",
            short_id(bundle.id),
            other.map_or("missing", BundleState::label)
        ),
    }
}

/// One pass over the queue: send everything sendable, vitals first. `peers`, when present,
/// enables peer-relay failover for bundles the direct link cannot deliver.
async fn drain_queue(
    config: &Config,
    key: &Key,
    target: &str,
    queue: &Queue,
    peers: Option<&PeerTable>,
) -> Result<()> {
    let socket = open_socket(&config.net.listen_addr, target).await?;
    let mut pacer = Pacer::new(config.link.bandwidth_bps, PACER_BURST_BYTES);

    while let Some(record) = queue.next_sendable()? {
        // One-shot drain uses the full retry budget; the F4 breaker is a daemon-loop concern.
        deliver_one(
            config,
            key,
            queue,
            &socket,
            &mut pacer,
            &record,
            peers,
            &config.retry,
        )
        .await?;
    }
    Ok(())
}

async fn open_socket(listen_addr: &str, target_addr: &str) -> Result<UdpSocket> {
    let socket = UdpSocket::bind(listen_addr)
        .await
        .with_context(|| format!("binding {listen_addr}"))?;
    socket
        .connect(target_addr)
        .await
        .with_context(|| format!("connecting to gateway {target_addr}"))?;
    Ok(socket)
}

// The delivery path legitimately threads config, key, queue, socket, pacer, the record, the
// discovered peers, and the (F4-adjustable) retry budget; grouping them into a struct would only
// obscure a straight-line function.
#[allow(clippy::too_many_arguments)]
async fn deliver_one(
    config: &Config,
    key: &Key,
    queue: &Queue,
    socket: &UdpSocket,
    pacer: &mut Pacer,
    record: &QueuedBundle,
    peers: Option<&PeerTable>,
    retry: &RetryConfig,
) -> Result<Outcome> {
    let mut fec_sender =
        BundleSender::from_envelope(record.id, &record.envelope, key, &config.fec())
            .context("rebuilding FEC sender from stored envelope")?;

    queue.set_state(record.id, BundleState::Sending)?;
    println!(
        "bundle {}  [{}]  sending…",
        short_id(record.id),
        record.kind
    );

    // Vitals never yield; images step aside when vitals arrive.
    let is_image = record.kind == "image";
    let preempt_probe = || is_image && queue.vitals_waiting().unwrap_or(false);

    let started = std::time::Instant::now();
    // `retry` is the effective per-bundle budget for this pass: the full config schedule
    // normally, or a shrunk 1-retry probe when the F4 breaker has tripped on a dead link.
    let outcome = deliver(socket, &mut fec_sender, pacer, key, retry, preempt_probe).await?;

    match outcome {
        Outcome::Delivered => {
            queue.set_state(record.id, BundleState::Delivered)?;
            println!(
                "bundle {}  [{}]  delivered ✓  ({:.1}s)",
                short_id(record.id),
                record.kind,
                started.elapsed().as_secs_f64()
            );
            Ok(Outcome::Delivered)
        }
        Outcome::Stuck => {
            // Fix 2: before flagging stuck, try to reach the gateway through a discovered peer.
            if try_relay_failover(config, key, queue, record, peers).await? {
                return Ok(Outcome::Delivered);
            }
            queue.bump_retries(record.id)?;
            queue.mark_stuck(record.id, time::OffsetDateTime::now_utc())?;
            println!(
                "bundle {}  [{}]  STUCK after {} retries — kept in queue, will retry in daemon mode",
                short_id(record.id),
                record.kind,
                retry.max_retries
            );
            Ok(Outcome::Stuck)
        }
        Outcome::Preempted => {
            queue.set_state(record.id, BundleState::Queued)?;
            println!(
                "bundle {}  [{}]  paused — vitals take the link first",
                short_id(record.id),
                record.kind
            );
            Ok(Outcome::Preempted)
        }
    }
}

/// Attempt peer-relay failover for a bundle the direct link could not deliver. Returns `true`
/// if a discovered peer relayed it to a verified receipt (queue marked delivered), `false` if
/// relay is disabled, no peers are known, or none succeeded (the caller then flags it stuck).
async fn try_relay_failover(
    config: &Config,
    key: &Key,
    queue: &Queue,
    record: &QueuedBundle,
    peers: Option<&PeerTable>,
) -> Result<bool> {
    let Some(table) = peers else {
        return Ok(false);
    };
    let candidates: Vec<SocketAddr> = table
        .active(Instant::now())
        .into_iter()
        .filter_map(|addr| addr.parse().ok())
        .collect();
    if candidates.is_empty() {
        return Ok(false);
    }

    // Re-frame the still-sealed envelope into an over-provisioned burst the relay can forward
    // opaquely (it never decrypts; it only holds ciphertext).
    let relay_cfg = FecConfig {
        symbol_size: config.link.symbol_size,
        overhead_factor: RELAY_OVERHEAD_FACTOR,
    };
    let datagrams = BundleSender::from_envelope(record.id, &record.envelope, key, &relay_cfg)
        .context("re-framing bundle for relay")?
        .initial_burst();
    let budget = Duration::from_millis(config.retry.retry_backoff_ms.saturating_mul(4).max(1000));

    println!(
        "bundle {}  [{}]  direct link stuck — trying {} peer relay(s)…",
        short_id(record.id),
        record.kind,
        candidates.len()
    );
    if relay_failover(&candidates, record.id, &datagrams, key, budget).await? == Outcome::Delivered
    {
        queue.set_state(record.id, BundleState::Delivered)?;
        println!(
            "bundle {}  [{}]  delivered ✓  via peer relay",
            short_id(record.id),
            record.kind
        );
        return Ok(true);
    }
    Ok(false)
}

async fn daemon(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let (key, target) = resolve_key_and_target(&config)?;
    let queue = Queue::open(&queue_path())?; // open() re-queues interrupted transfers
    let socket = open_socket(&config.net.listen_addr, &target).await?;
    let mut pacer = Pacer::new(config.link.bandwidth_bps, PACER_BURST_BYTES);

    // Fix 2: when the relay fallback is enabled, announce presence, learn peers, and accept
    // relay requests from other devices — all in the background alongside the drain loop.
    let peers = start_relay_services(&config);

    tracing::info!(
        gateway = %target,
        bandwidth_bps = config.link.bandwidth_bps,
        relay = config.relay.enabled,
        circuit_breaker_threshold = config.retry.circuit_breaker_threshold,
        "field daemon up — draining queue continuously"
    );

    // Fix F4: fast-fail on a dead link. The full budget is used until `circuit_breaker_threshold`
    // consecutive bundles go stuck; then subsequent bundles are probed with a 1-retry budget and
    // the idle wait stretches to `circuit_cooldown_ms`, so a blackout reaches STUCK fast instead
    // of flogging every bundle at full budget. Any delivery restores the full budget.
    let full_retry = config.retry.clone();
    let probe_retry = probe_budget(&full_retry);
    let mut breaker = LinkBreaker::new(config.retry.circuit_breaker_threshold);

    let stuck_backoff = time::Duration::milliseconds(
        i64::try_from(config.retry.stuck_retry_backoff_ms).unwrap_or(i64::MAX),
    );
    loop {
        // Fix F1: revive stuck bundles that have backed off long enough (bounded by the retry
        // cap) so the daemon re-attempts them — including via peer-relay failover — instead of
        // leaving them terminal after the single pass in which they first went stuck.
        let rearmed = queue.rearm_stuck(
            time::OffsetDateTime::now_utc(),
            stuck_backoff,
            config.retry.max_stuck_retries,
        )?;
        if rearmed > 0 {
            tracing::info!(
                rearmed,
                "re-armed stuck bundle(s) for another delivery pass"
            );
        }
        match queue.next_sendable()? {
            Some(record) => {
                let retry = breaker.budget(&full_retry, &probe_retry);
                let outcome = deliver_one(
                    &config,
                    &key,
                    &queue,
                    &socket,
                    &mut pacer,
                    &record,
                    peers.as_ref(),
                    retry,
                )
                .await?;
                match breaker.record(outcome) {
                    BreakerEvent::Tripped => tracing::warn!(
                        threshold = config.retry.circuit_breaker_threshold,
                        cooldown_ms = config.retry.circuit_cooldown_ms,
                        "F4 circuit breaker tripped — direct link treated as down; probing with a \
                         reduced retry budget (bundles are still kept, never dropped)"
                    ),
                    BreakerEvent::Recovered => {
                        tracing::info!("direct link recovered — restoring full retry budget")
                    }
                    BreakerEvent::Unchanged => {}
                }
            }
            None => {
                // Idle wait: a long cool-down while the link is down (nothing to send and the
                // breaker is tripped), otherwise the normal snappy poll.
                let idle_ms = if breaker.is_tripped() {
                    config.retry.circuit_cooldown_ms.max(1)
                } else {
                    500
                };
                tokio::time::sleep(Duration::from_millis(idle_ms)).await;
            }
        }
    }
}

/// Launch peer discovery and the relay service if `[relay].enabled`, returning the shared
/// [`PeerTable`] the drain loop consults for failover. Returns `None` when relay is disabled.
///
/// Deployment note: `relay_listen_addr` is announced verbatim, so in production it must be the
/// device's reachable LAN address (not `0.0.0.0`); binding stays on the configured address.
fn start_relay_services(config: &Config) -> Option<PeerTable> {
    if !config.relay.enabled {
        return None;
    }
    let table = PeerTable::new(Duration::from_millis(config.relay.peer_ttl_ms));

    // Per-run instance id: self-filters our own announces without depending on the (often shared,
    // `0.0.0.0:…`) relay-listen address string. See discovery.rs (Fix F3).
    let instance_id = uuid::Uuid::new_v4();
    let discovery_addr = config.relay.discovery_addr.clone();
    let own_relay_addr = config.relay.relay_listen_addr.clone();
    let interval = Duration::from_millis(config.relay.announce_interval_ms.max(1));
    let discovery_table = table.clone();
    tracing::info!(%instance_id, discovery = %discovery_addr, "peer discovery starting");
    tokio::spawn(async move {
        if let Err(e) = run_discovery(
            &discovery_addr,
            instance_id,
            &own_relay_addr,
            interval,
            discovery_table,
        )
        .await
        {
            tracing::warn!(error = %e, "peer discovery stopped");
        }
    });

    if let Ok(gateway_addr) = config.net.gateway_addr.parse::<SocketAddr>() {
        let relay_listen = config.relay.relay_listen_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = run_relay_service(&relay_listen, gateway_addr).await {
                tracing::warn!(error = %e, "relay service stopped");
            }
        });
    } else {
        tracing::warn!(
            gateway = %config.net.gateway_addr,
            "relay service not started: gateway_addr is not a socket address"
        );
    }

    Some(table)
}

/// Shared state for the field bridge server (Fix: connect the browser UI to the real path).
struct BridgeState {
    config: Config,
    key: Key,
    /// Resolved gateway target (paired session address, or the config's `gateway_addr`).
    target: String,
    queue: Queue,
    /// Serializes captures so concurrent POSTs don't interleave drains of the shared queue.
    send_lock: tokio::sync::Mutex<()>,
}

/// One capture POSTed by the browser field UI. All vitals are optional; at least one must be
/// present (mirrors `send-vitals`). BP is sent as separate systolic/diastolic components.
#[derive(Debug, Deserialize)]
struct CapturePayload {
    patient: String,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    performer: Option<String>,
    #[serde(default)]
    bp_sys: Option<f64>,
    #[serde(default)]
    bp_dia: Option<f64>,
    #[serde(default)]
    spo2: Option<f64>,
    #[serde(default)]
    pulse: Option<f64>,
}

/// The result of a bridged capture: the real delivery outcome, surfaced back to the UI.
#[derive(Debug, Serialize)]
struct CaptureResult {
    bundle_id: String,
    short_id: String,
    state: String,
    delivered: bool,
}

/// Serve the browser field UI and bridge its captures onto the real UDP send path.
async fn serve(config_path: &Path, http: &str, ui_dir: PathBuf) -> Result<()> {
    let config = Config::load(config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let (key, target) = resolve_key_and_target(&config)?;
    let queue = Queue::open(&queue_path())?;
    let state = Arc::new(BridgeState {
        config,
        key,
        target,
        queue,
        send_lock: tokio::sync::Mutex::new(()),
    });

    let app = Router::new()
        .route("/api/capture", post(capture_handler))
        .route("/api/status", get(status_handler))
        // Anything else is served from the field UI directory (index.html, app.js, …).
        .fallback_service(ServeDir::new(&ui_dir))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(http)
        .await
        .with_context(|| format!("binding field bridge {http}"))?;
    tracing::info!(
        http = %http,
        gateway = %state.target,
        ui = %ui_dir.display(),
        "field bridge up — POST /api/capture runs the REAL seal→RaptorQ→UDP send"
    );
    axum::serve(listener, app)
        .await
        .context("field bridge server")?;
    Ok(())
}

/// Bridge a browser capture onto the real send path: build observations under the same capture
/// guards as the CLI, enqueue, and drain over UDP to the gateway (direct link, like `send-vitals`).
async fn capture_handler(
    State(state): State<Arc<BridgeState>>,
    Json(payload): Json<CapturePayload>,
) -> Result<Json<CaptureResult>, (StatusCode, String)> {
    let bp = match (payload.bp_sys, payload.bp_dia) {
        (Some(sys), Some(dia)) => Some(format!("{sys}/{dia}")),
        (None, None) => None,
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "blood pressure needs both systolic and diastolic".to_string(),
            ));
        }
    };
    let input = VitalsInput {
        bp,
        spo2: payload.spo2,
        pulse: payload.pulse,
        patient: payload.patient,
        device: payload.device.unwrap_or_else(|| "field-ui".to_string()),
        performer: payload
            .performer
            .unwrap_or_else(|| "field-worker".to_string()),
    };
    // Same capture guards as the CLI: INPUT_* bounds, diastolic < systolic, ≥1 measurement.
    let observations =
        build_observations(&input).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    let bundle = Bundle::new_vitals(observations);

    let record = QueuedBundle::from_bundle(&bundle, &state.key)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    state
        .queue
        .enqueue(&record)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;

    // Serialize the drain so concurrent captures don't interleave on the shared queue/socket.
    {
        let _guard = state.send_lock.lock().await;
        drain_queue(&state.config, &state.key, &state.target, &state.queue, None)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    }

    let final_state = state
        .queue
        .get(bundle.id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .map(|r| r.state);
    Ok(Json(CaptureResult {
        bundle_id: bundle.id.to_string(),
        short_id: short_id(bundle.id),
        state: final_state
            .map_or("missing", BundleState::label)
            .to_string(),
        delivered: final_state == Some(BundleState::Delivered),
    }))
}

/// Return the field queue as JSON so the UI can show queued/sending/delivered/stuck states.
async fn status_handler(
    State(state): State<Arc<BridgeState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let records = state
        .queue
        .list()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let items: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "short_id": short_id(r.id),
                "kind": r.kind,
                "state": r.state.label(),
                "retries": r.retries,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "queue": items })))
}

async fn status(config_path: &Path, watch: bool) -> Result<()> {
    // Status must work even if the config/key are absent — reading the queue is local.
    let _ = config_path;
    let queue = Queue::open(&queue_path())?;
    loop {
        let records = queue.list()?;
        if watch {
            print!("\x1b[2J\x1b[H"); // clear screen, home cursor
        }
        println!(
            "{bundle:<10} {kind:<8} {state:<14} {size:>9} {retries:>8}  AGE",
            bundle = "BUNDLE",
            kind = "KIND",
            state = "STATE",
            size = "SIZE",
            retries = "RETRIES",
        );
        if records.is_empty() {
            println!("(queue empty)");
        }
        for record in &records {
            let age = time::OffsetDateTime::now_utc() - record.created_at;
            println!(
                "{:<10} {:<8} {:<14} {:>7} B {:>8}  {}s",
                short_id(record.id),
                record.kind,
                record.state.label(),
                record.envelope.len(),
                record.retries,
                age.whole_seconds().max(0)
            );
        }
        if !watch {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn short_id(id: uuid::Uuid) -> String {
    let hyphenated = id.to_string();
    hyphenated.chars().take(8).collect()
}

#[cfg(test)]
mod pairing_string_tests {
    use super::*;

    #[test]
    fn parses_well_formed_and_rejects_bad() {
        let (a, c) = parse_pairing_string("tgw1:203.0.113.5:47000:4-otter-cobalt").expect("ok");
        assert_eq!(a, "203.0.113.5:47000");
        assert_eq!(c, "4-otter-cobalt");
        assert!(parse_pairing_string("nope").is_err());
        assert!(
            parse_pairing_string("tgw1:203.0.113.5:47000:").is_err(),
            "empty code"
        );
        assert!(parse_pairing_string("tgw1:not-an-addr:code").is_err());
    }
}
