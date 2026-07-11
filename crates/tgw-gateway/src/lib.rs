//! `tgw-gateway` library — UDP receiver/decoder + axum HTTP API (Contract 3).
//!
//! OWNER: Twaha. Binaries are thin async shells (see `main.rs`); logic lives here so the
//! integration test can drive it in-process (Phase D).
//!
//! At scaffold stage the HTTP layer already serves the mock fixtures from
//! `static/mock/*.json`, so the dashboard works end-to-end immediately. The UDP decode path
//! and the redb-backed real API are `todo!()` — fill them in Phases B/E/F.

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tower_http::services::ServeDir;

/// Shared state handed to every HTTP handler.
#[derive(Clone)]
pub struct AppState {
    /// Directory served by axum (`ServeDir`) and source of the mock fixtures.
    pub static_dir: PathBuf,
    // TODO(PHASE-E/F): add the redb store handle here (e.g. `store: Arc<Store>`).
}

/// Build the axum router: the JSON API (Contract 3) plus static-file serving.
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

/// Run the HTTP server until shutdown.
pub async fn run_http_server(addr: SocketAddr, state: AppState) -> anyhow::Result<()> {
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "gateway HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Bind the UDP socket, decode incoming bundles, emit FHIR + receipts.
///
/// TODO(PHASE-B): `tokio::net::UdpSocket::bind` → `tgw_core::parse_frame` →
/// `tgw_core::BundleReceiver::absorb` per bundle → on `Absorb::Complete(bundle)` log the
/// decoded vitals and print `tgw_fhir::to_fhir_json`. TODO(PHASE-E): send receipts + dedup.
pub async fn run_udp_listener(addr: SocketAddr) -> anyhow::Result<()> {
    let _ = addr;
    todo!("PHASE-B: UDP receive + parse_frame + BundleReceiver::absorb")
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
