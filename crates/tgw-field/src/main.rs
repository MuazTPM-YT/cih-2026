//! `tgw-field` — the lightweight field-client binary (docs/ARCHITECTURE.md).
//!
//! Subcommands: `keygen`, `send-vitals`, `send-image`, `status [--watch]`, `daemon`.
//! Everything a field worker sees flows through here; the certainty story
//! (`queued → sending → delivered ✓`, or a loud `STUCK`) is printed, never hidden.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tgw_core::{Bundle, BundleSender, Config, Key};
use tokio::net::UdpSocket;

use tgw_field::pacer::Pacer;
use tgw_field::queue::{BundleState, Queue, QueuedBundle};
use tgw_field::sender::{Outcome, deliver};
use tgw_field::vitals::{VitalsInput, build_observations};

/// Instantaneous burst allowance for the pacer (matches the demo's `tbf burst 8kb`).
const PACER_BURST_BYTES: usize = 8 * 1024;

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
    /// Run continuously: drain the queue, resume interrupted transfers, serve NACKs.
    Daemon,
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
        Command::Status { watch } => status(&cli.config, watch).await,
        Command::Daemon => daemon(&cli.config).await,
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

fn load_config_and_key(config_path: &Path) -> Result<(Config, Key)> {
    let config = Config::load(config_path)
        .with_context(|| format!("loading config {}", config_path.display()))?;
    let key = Key::from_file(&config.crypto.key_file)
        .context("loading PSK (generate one with `tgw-field keygen`)")?;
    Ok((config, key))
}

fn image_bundle(
    config_path: &Path,
    image_path: &Path,
    patient: &str,
    mime: Option<String>,
) -> Result<Bundle> {
    let (config, _key) = load_config_and_key(config_path)?;
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
    let (config, key) = load_config_and_key(config_path)?;
    let queue = Queue::open(&queue_path())?;

    let record = QueuedBundle::from_bundle(&bundle, &key)?;
    queue.enqueue(&record)?;
    println!("bundle {}  [{}]  queued", short_id(bundle.id), record.kind);

    drain_queue(&config, &key, &queue).await?;

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

/// One pass over the queue: send everything sendable, vitals first.
async fn drain_queue(config: &Config, key: &Key, queue: &Queue) -> Result<()> {
    let socket = open_socket(config).await?;
    let mut pacer = Pacer::new(config.link.bandwidth_bps, PACER_BURST_BYTES);

    while let Some(record) = queue.next_sendable()? {
        deliver_one(config, key, queue, &socket, &mut pacer, &record).await?;
    }
    Ok(())
}

async fn open_socket(config: &Config) -> Result<UdpSocket> {
    let socket = UdpSocket::bind(&config.net.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.net.listen_addr))?;
    socket
        .connect(&config.net.gateway_addr)
        .await
        .with_context(|| format!("connecting to gateway {}", config.net.gateway_addr))?;
    Ok(socket)
}

async fn deliver_one(
    config: &Config,
    key: &Key,
    queue: &Queue,
    socket: &UdpSocket,
    pacer: &mut Pacer,
    record: &QueuedBundle,
) -> Result<()> {
    let mut fec_sender = BundleSender::from_envelope(record.id, &record.envelope, &config.fec())
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
    let outcome = deliver(
        socket,
        &mut fec_sender,
        pacer,
        key,
        &config.retry,
        preempt_probe,
    )
    .await?;

    match outcome {
        Outcome::Delivered => {
            queue.set_state(record.id, BundleState::Delivered)?;
            println!(
                "bundle {}  [{}]  delivered ✓  ({:.1}s)",
                short_id(record.id),
                record.kind,
                started.elapsed().as_secs_f64()
            );
        }
        Outcome::Stuck => {
            queue.bump_retries(record.id)?;
            queue.set_state(record.id, BundleState::Stuck)?;
            println!(
                "bundle {}  [{}]  STUCK after {} retries — kept in queue, will retry in daemon mode",
                short_id(record.id),
                record.kind,
                config.retry.max_retries
            );
        }
        Outcome::Preempted => {
            queue.set_state(record.id, BundleState::Queued)?;
            println!(
                "bundle {}  [{}]  paused — vitals take the link first",
                short_id(record.id),
                record.kind
            );
        }
    }
    Ok(())
}

async fn daemon(config_path: &Path) -> Result<()> {
    let (config, key) = load_config_and_key(config_path)?;
    let queue = Queue::open(&queue_path())?; // open() re-queues interrupted transfers
    let socket = open_socket(&config).await?;
    let mut pacer = Pacer::new(config.link.bandwidth_bps, PACER_BURST_BYTES);

    tracing::info!(
        gateway = %config.net.gateway_addr,
        bandwidth_bps = config.link.bandwidth_bps,
        "field daemon up — draining queue continuously"
    );
    loop {
        match queue.next_sendable()? {
            Some(record) => {
                // A stuck bundle re-enters via manual requeue; retries here are per-pass.
                deliver_one(&config, &key, &queue, &socket, &mut pacer, &record).await?;
            }
            None => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
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
