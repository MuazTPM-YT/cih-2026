//! Store-backed API spec: drives `router_with_store` over a real redb [`Store`] (not the
//! mock fixtures) and asserts the Contract-3 shapes surface genuine `to_fhir_json` output,
//! real queue lifecycle state, and image bytes. Complements `api_contract.rs` (mock router).
//! Never weaken a test to pass — these shapes are what Jiya's dashboard depends on.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tgw_core::{Measure, VitalsObservation};
use tgw_gateway::{AppState, Store, StoreState, router_with_store};
use time::macros::datetime;
use tower::ServiceExt;
use uuid::Uuid;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh, isolated redb store on a unique temp path (removed if a stale one lingers).
fn temp_store() -> Store {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("tgw-store-api-{}-{n}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Store::open(&path).expect("open store")
}

/// Seed one vitals bundle (real FHIR JSON) and one image bundle; return their ids + state.
fn seeded_state() -> (StoreState, Uuid, Uuid) {
    let store = temp_store();

    let obs = VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: "8867-4".into(),
        effective: datetime!(2026-07-11 14:03:22 UTC),
        value: Some(Measure {
            value: 72.0,
            ucum_unit: "/min".into(),
        }),
        components: vec![],
        device_id: "field-ecg-01".into(),
        performer_id: "fieldworker-7".into(),
    };
    let vitals_id = Uuid::new_v4();
    store
        .complete_vitals(
            vitals_id,
            &[tgw_fhir::to_fhir_json(&obs)],
            &[vec![]],
            "2026-07-11T14:03:22Z",
        )
        .expect("persist vitals");

    let image_id = Uuid::new_v4();
    store
        .complete_image(
            image_id,
            "image/jpeg",
            &[0xABu8; 2048],
            "P-1023",
            "2026-07-11T14:04:00Z",
        )
        .expect("persist image");

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    let state = StoreState {
        base: AppState { static_dir },
        store: Arc::new(store),
    };
    (state, vitals_id, image_id)
}

async fn get(state: &StoreState, uri: &str) -> (StatusCode, Option<String>, Vec<u8>) {
    let resp = router_with_store(state.clone())
        .oneshot(
            Request::get(uri)
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router is infallible");
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body collects")
        .to_vec();
    (status, ctype, bytes)
}

#[tokio::test]
async fn observations_endpoint_serves_real_fhir_from_store() {
    let (state, _v, _i) = seeded_state();
    let (status, ctype, body) = get(&state, "/api/observations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let arr: Vec<Value> = serde_json::from_slice(&body).expect("json array");
    assert_eq!(arr.len(), 2, "one vitals + one image");

    let vitals = arr
        .iter()
        .find(|i| i["kind"] == "vitals")
        .expect("vitals item");
    assert_eq!(vitals["fhir"]["resourceType"], "Observation");
    assert_eq!(vitals["fhir"]["status"], "final");
    assert_eq!(
        vitals["fhir"]["code"]["coding"][0]["system"],
        "http://loinc.org"
    );
    assert_eq!(vitals["fhir"]["code"]["coding"][0]["code"], "8867-4");
    assert_eq!(vitals["patient_id"], "P-1023");

    let image = arr
        .iter()
        .find(|i| i["kind"] == "image")
        .expect("image item");
    assert!(
        image["image_url"]
            .as_str()
            .unwrap_or_default()
            .starts_with("/api/images/"),
        "image_url must be /api/images/<id>"
    );
    assert_eq!(image["fhir"]["resourceType"], "Media");
    assert_eq!(image["fhir"]["content"]["contentType"], "image/jpeg");
}

#[tokio::test]
async fn queue_endpoint_reflects_real_lifecycle_state() {
    let (state, _v, _i) = seeded_state();
    let (status, ctype, body) = get(&state, "/api/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let arr: Vec<Value> = serde_json::from_slice(&body).expect("json array");
    assert_eq!(arr.len(), 2, "both bundles appear in the queue");
    for item in &arr {
        // Both were persisted via `complete_*`, so they sit at the `complete` state with a
        // real `completed_at` timestamp — not a fabricated value.
        assert_eq!(item["state"], "complete");
        assert!(
            item["completed_at"].is_string(),
            "completed_at set on complete"
        );
        assert!(item["symbols_needed"].is_u64() || item["symbols_needed"].is_i64());
    }
}

#[tokio::test]
async fn image_endpoint_returns_stored_bytes_with_content_type() {
    let (state, _v, image_id) = seeded_state();
    let (status, ctype, body) = get(&state, &format!("/api/images/{image_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("image/jpeg"));
    assert_eq!(body, vec![0xABu8; 2048], "image bytes round-trip intact");
}

#[tokio::test]
async fn unknown_image_is_404_never_500() {
    let (state, _v, _i) = seeded_state();
    let (status, _ctype, _body) = get(&state, &format!("/api/images/{}", Uuid::new_v4())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_sets_cors_header_for_cross_origin_dashboard() {
    // The metrics dashboard polls this API from another origin; a GET carrying an Origin must
    // come back with an Access-Control-Allow-Origin header, or the browser blocks the read.
    let (state, _v, _i) = seeded_state();
    let res = router_with_store(state)
        .oneshot(
            Request::builder()
                .uri("/api/queue")
                .header(header::ORIGIN, "http://localhost:9999")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("*"),
        "cross-origin GET must be allowed for the dashboard"
    );
}
