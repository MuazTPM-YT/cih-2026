//! `tgw-gateway` library — UDP receiver/decoder + axum HTTP API (Contract 3).
//!
//! OWNER: Twaha. Binaries are thin async shells (see `main.rs`); logic lives here so the
//! integration test can drive it in-process.
//!
//! Two routers ship: [`router`] serves the `static/mock/*.json` fixtures (used by the
//! `api_contract` tests and quick dashboard bring-up), and [`router_with_store`] backs the
//! same Contract-3 shapes with the live redb [`Store`]. The UDP decode path
//! ([`run_udp_listener`]) reassembles bundles, emits FHIR + AEAD receipts, and drives the
//! gateway-side NACK/repair loop. All are implemented and tested.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tgw_core::{
    Absorb, Bundle, BundlePayload, BundleReceiver, CoreError, Frame, Key, build_receipt,
    encode_nack, parse_frame,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::net::UdpSocket;
use tokio::time::timeout;
use tower_http::services::ServeDir;
use uuid::Uuid;

mod store;
/// Redb-backed persistence for delivered bundles and gateway queue state.
pub use store::Store;

pub mod pairing;

/// Persisted pairing-derived key for the gateway (hex, `0600`). Absent ⇒ fall back to key_file.
pub mod session {
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result};
    use tgw_core::Key;

    /// Default gateway session path; `TGW_GW_SESSION_PATH` overrides.
    #[must_use]
    pub fn default_path() -> PathBuf {
        std::env::var("TGW_GW_SESSION_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("gateway-session.key"))
    }

    /// Save `key` as hex, `0600`.
    pub fn save(path: &Path, key: &Key) -> Result<()> {
        std::fs::write(path, key.to_hex()).with_context(|| format!("write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("chmod 600 {}", path.display()))?;
        }
        Ok(())
    }

    /// Load the key, or `Ok(None)` if absent.
    pub fn load(path: &Path) -> Result<Option<Key>> {
        match std::fs::read_to_string(path) {
            Ok(hex) => Ok(Some(Key::from_hex(hex.trim()).context("gateway session key")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
}

/// Maximum UDP datagram the gateway buffers (fits a 65535-byte IP payload).
const MAX_DATAGRAM: usize = 65_535;

/// Cap on concurrently-decoding bundles; the oldest partial is evicted past this. Defence in
/// depth on the public port: even authenticated-but-abandoned bundles (a buggy or compromised
/// paired client, or massive reordering) cannot grow decoder memory without bound.
const MAX_INFLIGHT_BUNDLES: usize = 256;

/// Choose the oldest in-flight bundle to evict when the receiver map is at capacity (LRU by
/// first-seen). Returns `None` when under capacity.
fn bundle_to_evict(
    first_seen: &HashMap<Uuid, std::time::Instant>,
    cap: usize,
) -> Option<Uuid> {
    if first_seen.len() < cap {
        return None;
    }
    first_seen
        .iter()
        .min_by_key(|(_, seen)| **seen)
        .map(|(id, _)| *id)
}

/// Shared state handed to every HTTP handler.
#[derive(Clone)]
pub struct AppState {
    /// Directory served by axum (`ServeDir`) and source of the mock fixtures.
    pub static_dir: PathBuf,
}

/// Shared state for the redb-backed (Phase F) API: the mock state plus a store handle.
#[derive(Clone)]
pub struct StoreState {
    /// Inherited mock state (static dir for `ServeDir` fallback).
    pub base: AppState,
    /// The redb store backing the real API.
    pub store: Arc<Store>,
}

/// Build the axum router in **mock mode** (serves `static/mock/*.json` fixtures).
/// Used by the `api_contract` test suite and the scaffold binary before Phase F.
pub fn router(state: AppState) -> Router {
    let static_dir = state.static_dir.clone();
    Router::new()
        .route("/api/observations", get(get_observations))
        .route("/api/queue", get(get_queue))
        .route("/api/images/{bundle_id}", get(get_image))
        .route("/naive-upload", post(naive_upload))
        // Anything not matched above is served from `static/` (index.html, app.js, mock/…).
        .fallback_service(ServeDir::new(static_dir))
        .with_state(state)
}

/// Build the axum router in **redb-backed mode** (Phase F): the JSON API reads from the
/// [`Store`], producing Contract-3-shaped responses identical to the mock fixtures. The
/// static-file fallback (dashboard) is preserved for Jiya's dashboard assets.
pub fn router_with_store(state: StoreState) -> Router {
    let static_dir = state.base.static_dir.clone();
    Router::new()
        .route("/api/observations", get(get_observations_store))
        .route("/api/queue", get(get_queue_store))
        .route("/api/images/{bundle_id}", get(get_image_store))
        .route("/naive-upload", post(naive_upload))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(state)
}

/// Run the HTTP server in mock mode until shutdown.
pub async fn run_http_server(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "gateway HTTP listening (mock)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Run the HTTP server in redb-backed mode (Phase F) until shutdown.
pub async fn run_http_server_store(addr: SocketAddr, state: StoreState) -> anyhow::Result<()> {
    let app = router_with_store(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "gateway HTTP listening (redb)");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Bind the UDP socket, decode incoming bundles, emit FHIR + receipts, and drive the
/// gateway-side NACK/repair loop.
///
/// Loop: `recv_from` (bounded by `nack_timeout`) → [`tgw_core::parse_frame`] → for `DATA`
/// frames, drive the matching [`tgw_core::BundleReceiver`] with
/// [`tgw_core::BundleReceiver::absorb`]. On [`tgw_core::Absorb::Complete`], dedup against the
/// [`Store`]: a brand-new bundle is persisted (FHIR JSON for vitals, raw bytes for images)
/// and marked delivered; a re-burst of an already-delivered bundle is logged but not
/// re-stored (idempotent). In both cases a `DELIVERED` receipt is sent back to the field.
///
/// Decode-stall recovery: `recv_from` is wrapped in a `nack_timeout` window. When it
/// elapses with in-flight bundles still incomplete, the gateway mints a
/// [`tgw_core::BundleReceiver::build_nack`] for each and sends it to that bundle's remembered
/// source address — the gateway-initiated repair loop from the architecture. The field's own
/// silence re-burst remains a backstop; both are idempotent under the fountain code.
pub async fn run_udp_listener(
    addr: SocketAddr,
    store: Arc<Store>,
    key: Key,
    nack_timeout: Duration,
) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(addr)
        .await
        .context("gateway: bind UDP listener")?;
    tracing::info!(%addr, "gateway UDP listening");

    let mut receivers: HashMap<Uuid, BundleReceiver> = HashMap::new();
    // Where to send NACKs/receipts for each in-flight bundle (its last-seen field source).
    let mut sources: HashMap<Uuid, SocketAddr> = HashMap::new();
    // First-seen instant per in-flight bundle, for the LRU eviction cap (defence in depth).
    let mut first_seen: HashMap<Uuid, std::time::Instant> = HashMap::new();
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        let (n, src) = match timeout(nack_timeout, sock.recv_from(&mut buf)).await {
            Ok(result) => result.context("gateway: recv_from")?,
            Err(_elapsed) => {
                // Stall timer: nudge every incomplete in-flight bundle with a fresh NACK.
                for (bundle_id, receiver) in &receivers {
                    let Some(nack) = receiver.build_nack() else {
                        continue;
                    };
                    if nack.needed.iter().sum::<u32>() == 0 {
                        continue;
                    }
                    if let Some(dst) = sources.get(bundle_id) {
                        sock.send_to(&encode_nack(&nack), *dst)
                            .await
                            .context("gateway: send stall NACK")?;
                        tracing::info!(%bundle_id, "gateway: decode stalled, NACK sent");
                    }
                }
                continue;
            }
        };
        let dgram = &buf[..n];

        let frame = match parse_frame(dgram) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, from = %src, "gateway: malformed frame, ignoring");
                continue;
            }
        };

        match frame {
            Frame::Data { bundle_id } => {
                // PUBLIC-PORT HARDENING: authenticate the datagram under the session key BEFORE
                // touching any per-bundle map. An off-key flood (random UUIDs, forged tags) is
                // dropped here, so it can never create BundleReceiver/sources state — closing the
                // unauthenticated memory-exhaustion vector on an internet-facing port. `absorb`
                // re-checks the tag as defence in depth; the cost here is one HMAC over ~1 KB,
                // far cheaper than a decoder slot, and a corrupt packet leaves any legitimate
                // in-flight bundle's accumulated symbols untouched.
                if !tgw_core::authenticate_data(dgram, &key) {
                    tracing::debug!(%bundle_id, from = %src, "dropping unauthenticated DATA (no state created)");
                    continue;
                }
                // Drive the per-bundle receiver. The borrow of `receivers` ends once `absorb`
                // returns, so we can mutate `receivers` again in the outcome arms below.
                sources.insert(bundle_id, src);
                // Cap authenticated in-flight bundles; evict the oldest partial on overflow.
                // Delivery of any bundle clears its entry, so this only bites pathological cases.
                if !receivers.contains_key(&bundle_id) {
                    if let Some(victim) = bundle_to_evict(&first_seen, MAX_INFLIGHT_BUNDLES) {
                        receivers.remove(&victim);
                        sources.remove(&victim);
                        first_seen.remove(&victim);
                        tracing::warn!(evicted = %victim, "in-flight cap reached — evicted oldest partial bundle");
                    }
                    first_seen.insert(bundle_id, std::time::Instant::now());
                }
                let (outcome, symbols_received, symbols_needed) = {
                    let receiver = receivers
                        .entry(bundle_id)
                        .or_insert_with(|| BundleReceiver::new(key.clone()));
                    let outcome = receiver.absorb(dgram);
                    (
                        outcome,
                        receiver.symbols_received(),
                        receiver.symbols_needed().unwrap_or_default(),
                    )
                };
                match outcome {
                    Ok(Absorb::NeedMore) => {
                        store.record_receiving(
                            bundle_id,
                            symbols_received,
                            symbols_needed,
                            &timestamp()?,
                        )?;
                        tracing::debug!(%bundle_id, "gateway: need more symbols");
                    }
                    Ok(Absorb::Complete(bundle)) => {
                        receivers.remove(&bundle_id);
                        sources.remove(&bundle_id);
                        first_seen.remove(&bundle_id);
                        if handle_complete(&store, &bundle)? {
                            let receipt = build_receipt(bundle.id, &key);
                            sock.send_to(&receipt, src)
                                .await
                                .context("gateway: send receipt")?;
                            store.mark_receipt_sent(bundle.id)?;
                        }
                    }
                    Ok(Absorb::Nack(nack)) => {
                        tracing::info!(
                            %bundle_id,
                            needed_blocks = nack.needed.len(),
                            "gateway: decode stalled, NACK queued"
                        );
                        sock.send_to(&encode_nack(&nack), src)
                            .await
                            .context("gateway: send NACK")?;
                    }
                    Err(CoreError::Crypto) => {
                        // AEAD verification failed on a fully reconstructed envelope (Fix 1b):
                        // the bundle is poisoned. Persist nothing, send no receipt, and drop
                        // the receiver. The field, receiving no receipt within its timeout,
                        // re-bursts (its existing backoff) and a fresh receiver retries the
                        // whole bundle — so this stays retryable end-to-end. Never log key
                        // material or plaintext; only ids and counts.
                        tracing::warn!(
                            %bundle_id,
                            symbols_received,
                            symbols_needed,
                            "gateway: AEAD verification failed after reconstruction — \
                             dropping bundle (no persist, no receipt); bundle remains retryable"
                        );
                        receivers.remove(&bundle_id);
                        sources.remove(&bundle_id);
                        first_seen.remove(&bundle_id);
                    }
                    Err(e) => {
                        // A single corrupt/malformed datagram (failed integrity tag or bad
                        // framing) from radio interference or a forged packet (Fix 1a). Drop
                        // just this datagram and KEEP the receiver's accumulated clean symbols
                        // — one bad packet must never discard an in-progress bundle.
                        tracing::debug!(
                            %bundle_id,
                            error = %e,
                            "gateway: dropping corrupt datagram, keeping bundle progress"
                        );
                    }
                }
            }
            Frame::Nack(nack) => {
                // A NACK is gateway→field; an inbound one is unexpected — log and ignore.
                tracing::debug!(bundle = %nack.bundle_id, "gateway: inbound NACK frame ignored");
            }
            Frame::Receipt {
                bundle_id,
                delivered,
            } => {
                // A RECEIPT is gateway→field; an inbound one is unexpected — log and ignore.
                tracing::debug!(%bundle_id, delivered, "gateway: inbound RECEIPT frame ignored");
            }
        }
    }
}

/// Dedup + persist a fully decoded bundle, then log its identity (no PHI) and emit FHIR JSON.
///
/// A re-burst of an already-delivered bundle is logged but not re-stored (idempotent); a fresh
/// bundle is persisted — FHIR R5 JSON for vitals, raw bytes + MIME for images — and marked
/// delivered in the [`Store`].
fn handle_complete(store: &Store, bundle: &Bundle) -> anyhow::Result<bool> {
    let id = bundle.id;
    let already_delivered = store.is_delivered(id)?;
    if already_delivered {
        tracing::info!(%id, "gateway: duplicate re-burst of an already-delivered bundle");
        // The bundle is owed a fresh receipt (idempotent), but no second record is written.
        return Ok(true);
    }

    on_complete(bundle);
    persist_bundle(store, bundle)?;
    Ok(true)
}

/// Persist a fresh bundle into the [`Store`]: FHIR JSON for vitals, bytes+MIME for images,
/// then mark the bundle ID as delivered with an RFC-3339 `received_at` timestamp.
fn persist_bundle(store: &Store, bundle: &Bundle) -> anyhow::Result<()> {
    let received_at = timestamp()?;
    match &bundle.payload {
        BundlePayload::Vitals(observations) => {
            let fhir: Vec<_> = observations.iter().map(tgw_fhir::to_fhir_json).collect();
            // Fix 1c: compute advisory plausibility flags per observation at ingest. This is
            // additive metadata — it never changes whether the observation is stored.
            let flags: Vec<Vec<String>> = observations
                .iter()
                .map(tgw_fhir::plausibility_flags)
                .collect();
            store.complete_vitals(bundle.id, &fhir, &flags, &received_at)?;
        }
        BundlePayload::Image {
            mime,
            data,
            patient_id,
        } => {
            store.complete_image(bundle.id, mime, data, patient_id, &received_at)?;
        }
    }
    Ok(())
}

/// Format a UTC timestamp for durable Contract-3 fields.
fn timestamp() -> anyhow::Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format RFC-3339 timestamp")
}

/// Log a decoded bundle's identity (no PHI) and pretty-print its FHIR JSON for vitals.
fn on_complete(bundle: &Bundle) {
    match &bundle.payload {
        BundlePayload::Vitals(observations) => {
            for obs in observations {
                tracing::info!(
                    bundle_id = %bundle.id,
                    loinc = %obs.loinc,
                    "gateway: decoded vitals observation (no PHI values logged)"
                );
                // PHI: the decoded Observation carries patient values. Never print it by
                // default (stdout is captured by journald/containers — that would leak PHI,
                // contradicting the no-PHI-in-logs posture). Opt in only for local demos.
                if std::env::var_os("TGW_PRINT_FHIR").is_some() {
                    let fhir = tgw_fhir::to_fhir_json(obs);
                    let pretty = serde_json::to_string_pretty(&fhir).unwrap_or_default();
                    println!("{pretty}");
                }
            }
        }
        BundlePayload::Image { mime, data, .. } => {
            tracing::info!(
                bundle_id = %bundle.id,
                %mime,
                bytes = data.len(),
                "gateway: decoded image bundle (no PHI values logged)"
            );
        }
    }
}

// --- HTTP handlers ---------------------------------------------------------------------

async fn get_observations(State(state): State<AppState>) -> Response {
    // Mock router: serve the static fixture. The live path is `get_observations_store`.
    serve_mock(&state, "observations.json").await
}

async fn get_queue(State(state): State<AppState>) -> Response {
    // Mock router: serve the static fixture. The live path is `get_queue_store`.
    serve_mock(&state, "queue.json").await
}

async fn get_image(Path(bundle_id): Path<String>, State(_state): State<AppState>) -> Response {
    // Mock router has no image bytes; the live path is `get_image_store` (redb-backed).
    tracing::debug!(%bundle_id, "image requested on the mock router (no store)");
    (StatusCode::NOT_FOUND, "image not available in mock mode").into_response()
}

/// Demo-only sink: reads the body and returns 200, for the failing-`curl` comparison.
async fn naive_upload(body: Bytes) -> StatusCode {
    tracing::info!(bytes = body.len(), "naive-upload received");
    StatusCode::OK
}

/// Read `static/mock/<file>` and return it as a JSON response (scaffold behaviour).
async fn serve_mock(state: &AppState, file: &str) -> Response {
    let path = state.static_dir.join("mock").join(file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "mock fixture unreadable");
            (StatusCode::INTERNAL_SERVER_ERROR, "mock fixture unreadable").into_response()
        }
    }
}

// --- redb-backed handlers (Phase F) — Contract-3-shaped responses -----------------------

async fn get_observations_store(State(state): State<StoreState>) -> Response {
    match build_observations_json(&state.store) {
        Ok(json) => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&json).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "store: listing observations failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "observations listing failed",
            )
                .into_response()
        }
    }
}

async fn get_queue_store(State(state): State<StoreState>) -> Response {
    match build_queue_json(&state.store) {
        Ok(json) => (
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_vec(&json).unwrap_or_default(),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "store: listing queue failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "queue listing failed").into_response()
        }
    }
}

async fn get_image_store(
    Path(bundle_id): Path<String>,
    State(state): State<StoreState>,
) -> Response {
    let id = match bundle_id.parse::<Uuid>() {
        Ok(u) => u,
        Err(_) => return (StatusCode::NOT_FOUND, "invalid bundle id").into_response(),
    };
    match state.store.get_image(id) {
        Ok(Some((mime, data))) => ([(header::CONTENT_TYPE, mime.as_str())], data).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "image not found").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, %bundle_id, "store: image fetch failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "image fetch failed").into_response()
        }
    }
}

/// Build the Contract-3 `/api/observations` JSON array from the store, newest-first.
fn build_observations_json(store: &Store) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    let rows = store.list_delivered()?;
    let mut items = Vec::new();
    for (id, received_at, kind) in rows {
        match kind {
            "vitals" => {
                let observations = store.get_observations(id)?.ok_or_else(|| {
                    anyhow::anyhow!("delivered vitals bundle {id} is missing FHIR data")
                })?;
                // Fix 1c: plausibility flags, index-aligned with the observations. Absent for
                // bundles stored before flags existed → treated as "no flags".
                let flags = store.get_flags(id)?.unwrap_or_default();
                for (i, fhir_json) in observations.into_iter().enumerate() {
                    let patient_id = fhir_json
                        .get("subject")
                        .and_then(|subject| subject.get("reference"))
                        .and_then(|reference| reference.as_str())
                        .and_then(|reference| reference.strip_prefix("Patient/"))
                        .unwrap_or_default();
                    let obs_flags = flags.get(i).cloned().unwrap_or_default();
                    items.push(json!({
                        "bundle_id": id.to_string(),
                        "received_at": received_at,
                        "patient_id": patient_id,
                        "kind": "vitals",
                        "summary": format_summary(&fhir_json),
                        "flags": obs_flags,
                        "fhir": fhir_json,
                    }));
                }
            }
            "image" => {
                let patient_id = store.get_image_patient(id)?.ok_or_else(|| {
                    anyhow::anyhow!("delivered image bundle {id} is missing patient data")
                })?;
                let image_url = format!("/api/images/{id}");
                let mime = store.get_image_mime(id)?.unwrap_or_default();
                items.push(json!({
                    "bundle_id": id.to_string(),
                    "received_at": received_at,
                    "patient_id": patient_id,
                    "kind": "image",
                    "image_url": image_url,
                    "fhir": tgw_fhir::image_media_json(&patient_id, &mime, &image_url),
                }));
            }
            _ => {}
        }
    }
    Ok(serde_json::Value::Array(items))
}

/// Build a short human-readable summary string from a FHIR R5 Observation (for the dashboard).
fn format_summary(fhir: &serde_json::Value) -> String {
    let loinc = fhir
        .get("code")
        .and_then(|c| c.get("coding"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let value = fhir
        .get("valueQuantity")
        .and_then(|vq| vq.get("value"))
        .and_then(|v| v.as_f64())
        .map(|v| v.to_string())
        .unwrap_or_default();
    let unit = fhir
        .get("valueQuantity")
        .and_then(|vq| vq.get("unit"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    match (loinc, value.is_empty()) {
        ("8867-4", false) => format!("Pulse {value} {unit}"),
        ("59408-5", false) => format!("SpO2 {value}{unit}"),
        _ if value.is_empty() => "Panel".to_string(),
        _ => format!("{value} {unit}"),
    }
}

/// Build the Contract-3 `/api/queue` JSON array from the store, newest-first.
fn build_queue_json(store: &Store) -> anyhow::Result<serde_json::Value> {
    use serde_json::json;
    let items: Vec<serde_json::Value> = store
        .list_queue()?
        .into_iter()
        .map(|entry| {
            json!({
                "bundle_id": entry.bundle_id.to_string(),
                "state": entry.state,
                "symbols_received": entry.symbols_received,
                "symbols_needed": entry.symbols_needed,
                "first_seen": entry.first_seen,
                "completed_at": entry.completed_at,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(items))
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    #[test]
    fn evicts_oldest_only_at_capacity() {
        let mut m = HashMap::new();
        let t0 = std::time::Instant::now();
        let oldest = Uuid::new_v4();
        m.insert(oldest, t0);
        m.insert(Uuid::new_v4(), t0 + std::time::Duration::from_millis(5));
        assert_eq!(bundle_to_evict(&m, 3), None, "under cap: nothing evicted");
        assert_eq!(bundle_to_evict(&m, 2), Some(oldest), "at cap: oldest chosen");
    }
}
