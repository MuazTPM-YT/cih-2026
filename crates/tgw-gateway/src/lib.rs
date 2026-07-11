//! `tgw-gateway` library — UDP receiver/decoder + axum HTTP API (Contract 3).
//!
//! OWNER: Twaha. Binaries are thin async shells (see `main.rs`); logic lives here so the
//! integration test can drive it in-process (Phase D).
//!
//! At scaffold stage the HTTP layer already serves the mock fixtures from
//! `static/mock/*.json`, so the dashboard works end-to-end immediately. The UDP decode path
//! and the redb-backed real API are `todo!()` — fill them in Phases B/E/F.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tgw_core::{
    Absorb, Bundle, BundlePayload, BundleReceiver, Frame, Key, build_receipt, encode_nack,
    parse_frame,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::net::UdpSocket;
use tower_http::services::ServeDir;
use uuid::Uuid;

mod store;
pub use store::Store;

/// Maximum UDP datagram the gateway buffers (fits a 65535-byte IP payload).
const MAX_DATAGRAM: usize = 65_535;

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

/// Bind the UDP socket, decode incoming bundles, emit FHIR + receipts.
///
/// Loop: `recv_from` → [`tgw_core::parse_frame`] → for `DATA` frames, drive the matching
/// [`tgw_core::BundleReceiver`] with [`tgw_core::BundleReceiver::absorb`]. On
/// [`tgw_core::Absorb::Complete`], dedup against the [`Store`]: a brand-new bundle is persisted
/// (FHIR JSON for vitals, raw bytes for images) and marked delivered; a re-burst of an
/// already-delivered bundle is logged but not re-stored (idempotent). In both cases a
/// `DELIVERED` receipt is owed back to the field (Phase E).
///
/// NOTE: [`tgw_core::BundleReceiver::absorb`] and [`tgw_core::parse_frame`] are `todo!()` on
/// Muaz's branch; a live decode panics until his core lands — expected at the H6 sync. The
/// store/dedup path is real and unit-tested; the receipt send is `BLOCKED(PHASE-E)` until Muaz's
/// `build_receipt` + `Key::from_file` exist. This path is correct-by-construction and
/// compiles + clippy-clean today.
pub async fn run_udp_listener(addr: SocketAddr, store: Arc<Store>, key: Key) -> anyhow::Result<()> {
    let sock = UdpSocket::bind(addr)
        .await
        .context("gateway: bind UDP listener")?;
    tracing::info!(%addr, "gateway UDP listening");

    let mut receivers: HashMap<Uuid, BundleReceiver> = HashMap::new();
    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        let (n, src) = sock
            .recv_from(&mut buf)
            .await
            .context("gateway: recv_from")?;
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
                // Drive the per-bundle receiver. The borrow of `receivers` ends once `absorb`
                // returns, so we can mutate `receivers` again in the outcome arms below.
                let outcome = receivers
                    .entry(bundle_id)
                    .or_insert_with(|| BundleReceiver::new(key.clone()))
                    .absorb(dgram);
                match outcome {
                    Ok(Absorb::NeedMore) => {
                        tracing::debug!(%bundle_id, "gateway: need more symbols");
                    }
                    Ok(Absorb::Complete(bundle)) => {
                        receivers.remove(&bundle_id);
                        handle_complete(&store, &bundle);
                        let receipt = build_receipt(bundle.id, &key);
                        sock.send_to(&receipt, src).await.context("gateway: send receipt")?;
                    }
                    Ok(Absorb::Nack(nack)) => {
                        tracing::info!(
                            %bundle_id,
                            needed_blocks = nack.needed.len(),
                            "gateway: decode stalled, NACK queued"
                        );
                        sock.send_to(&encode_nack(&nack), src).await.context("gateway: send NACK")?;
                    }
                    Err(e) => {
                        tracing::warn!(%bundle_id, error = %e, "gateway: absorb failed, dropping bundle");
                        receivers.remove(&bundle_id);
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
fn handle_complete(store: &Store, bundle: &Bundle) {
    let id = bundle.id;
    let already_delivered = match store.is_delivered(id) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, %id, "gateway: dedup check failed; storing fresh");
            false
        }
    };
    if already_delivered {
        tracing::info!(%id, "gateway: duplicate re-burst of an already-delivered bundle");
        // The bundle is owed a fresh receipt (idempotent), but no second record is written.
        return;
    }

    on_complete(bundle);
    if let Err(e) = persist_bundle(store, bundle) {
        tracing::warn!(error = %e, %id, "gateway: persisting bundle failed");
    }
}

/// Persist a fresh bundle into the [`Store`]: FHIR JSON for vitals, bytes+MIME for images,
/// then mark the bundle ID as delivered with an RFC-3339 `received_at` timestamp.
fn persist_bundle(store: &Store, bundle: &Bundle) -> anyhow::Result<()> {
    let received_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    match &bundle.payload {
        BundlePayload::Vitals(observations) => {
            // Contract 3 has one `fhir` per bundle; use the first (panel) observation.
            if let Some(obs) = observations.first() {
                let fhir = tgw_fhir::to_fhir_json(obs);
                let json = serde_json::to_string(&fhir).unwrap_or_default();
                store.store_observation(bundle.id, &json)?;
            }
        }
        BundlePayload::Image { mime, data, .. } => {
            store.store_image(bundle.id, mime, data)?;
        }
    }
    store.mark_delivered(bundle.id, &received_at)?;
    Ok(())
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
                let fhir = tgw_fhir::to_fhir_json(obs);
                let pretty = serde_json::to_string_pretty(&fhir).unwrap_or_default();
                println!("{pretty}");
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
    // PHASE-B: serve the mock fixture. PHASE-F: back this with the redb store.
    serve_mock(&state, "observations.json").await
}

async fn get_queue(State(state): State<AppState>) -> Response {
    // PHASE-B: serve the mock fixture. PHASE-F: back this with live queue state.
    serve_mock(&state, "queue.json").await
}

async fn get_image(Path(bundle_id): Path<String>, State(_state): State<AppState>) -> Response {
    // TODO(PHASE-F): read image bytes from the redb store; set the correct Content-Type.
    tracing::debug!(%bundle_id, "image requested (not available in scaffold)");
    (StatusCode::NOT_FOUND, "image not available in scaffold").into_response()
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
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .filter_map(|(id, received_at, kind)| match kind {
            "vitals" => {
                let fhir_json: serde_json::Value = store.get_observation(id).ok()??.parse().ok()?;
                Some(json!({
                    "bundle_id": id.to_string(),
                    "received_at": received_at,
                    "patient_id": fhir_json
                        .get("subject")
                        .and_then(|s| s.get("reference"))
                        .and_then(|r| r.as_str())
                        .and_then(|r| r.strip_prefix("Patient/"))
                        .unwrap_or(""),
                    "kind": "vitals",
                    "summary": format_summary(&fhir_json),
                    "fhir": fhir_json,
                }))
            }
            "image" => Some(json!({
                "bundle_id": id.to_string(),
                "received_at": received_at,
                "patient_id": "",
                "kind": "image",
                "image_url": format!("/api/images/{}", id),
            })),
            _ => None,
        })
        .collect();
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
    let rows = store.list_delivered()?;
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(id, received_at, _kind)| {
            json!({
                "bundle_id": id.to_string(),
                "state": "receipt_sent",
                "symbols_received": 0,
                "symbols_needed": 0,
                "first_seen": received_at,
                "completed_at": received_at,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(items))
}
