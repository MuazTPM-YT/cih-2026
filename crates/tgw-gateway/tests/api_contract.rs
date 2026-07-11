//! Executable spec for the gateway HTTP API against **Contract 3** (tasks/CONTRACTS.md).
//! Socket-free via `tower::ServiceExt::oneshot`. These pass now (router serves the mock
//! fixtures) and MUST STAY GREEN after Phase F swaps in the redb-backed store — the shapes
//! are the contract Jiya's dashboard depends on. Never weaken a test to pass.

use std::path::PathBuf;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tgw_gateway::{AppState, router};
use tower::ServiceExt;

/// Build the app with the real fixture directory (independent of the test's CWD).
fn app() -> axum::Router {
    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    router(AppState { static_dir })
}

async fn get_json(uri: &str) -> (StatusCode, Option<String>, Value) {
    let resp = app()
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
        .expect("body collects");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, ctype, json)
}

/// Every `/api/observations` element must carry the exact Contract-3 keys.
fn assert_observation_shape(item: &Value) {
    for k in ["bundle_id", "received_at", "patient_id", "kind"] {
        assert!(item.get(k).is_some(), "observation missing `{k}`: {item}");
    }
    match item["kind"].as_str() {
        Some("vitals") => {
            assert!(item.get("summary").is_some(), "vitals item needs `summary`");
            let fhir = &item["fhir"];
            assert_eq!(
                fhir["resourceType"], "Observation",
                "vitals.fhir must be an Observation"
            );
            assert_eq!(fhir["status"], "final");
            assert_eq!(fhir["code"]["coding"][0]["system"], "http://loinc.org");
            assert!(
                fhir["subject"]["reference"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("Patient/"),
                "subject.reference must be Patient/<id>"
            );
        }
        Some("image") => {
            let url = item["image_url"].as_str().unwrap_or_default();
            assert!(
                url.starts_with("/api/images/"),
                "image_url must be /api/images/<id>"
            );
        }
        other => panic!("unknown observation kind: {other:?}"),
    }
}

/// Every `/api/queue` element must carry the exact Contract-3 keys and a valid state.
fn assert_queue_shape(item: &Value) {
    let state = item["state"].as_str().expect("state is a string");
    assert!(
        matches!(state, "receiving" | "complete" | "receipt_sent"),
        "unexpected queue state `{state}`"
    );
    assert!(
        item["symbols_received"].is_u64() || item["symbols_received"].is_i64(),
        "symbols_received must be an integer"
    );
    assert!(
        item["symbols_needed"].is_u64() || item["symbols_needed"].is_i64(),
        "symbols_needed must be an integer"
    );
    assert!(item.get("first_seen").is_some(), "first_seen required");
    let completed = &item["completed_at"];
    assert!(
        completed.is_null() || completed.is_string(),
        "completed_at must be a string or null"
    );
}

#[tokio::test]
async fn observations_endpoint_matches_contract() {
    let (status, ctype, json) = get_json("/api/observations").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let arr = json.as_array().expect("observations is a JSON array");
    assert!(!arr.is_empty(), "observations should not be empty");
    for item in arr {
        assert_observation_shape(item);
    }
    assert!(
        arr.iter().any(|i| i["kind"] == "vitals"),
        "fixture has a vitals entry"
    );
    assert!(
        arr.iter().any(|i| i["kind"] == "image"),
        "fixture has an image entry"
    );
}

#[tokio::test]
async fn queue_endpoint_matches_contract() {
    let (status, ctype, json) = get_json("/api/queue").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype.as_deref(), Some("application/json"));
    let arr = json.as_array().expect("queue is a JSON array");
    assert!(!arr.is_empty(), "queue should not be empty");
    for item in arr {
        assert_queue_shape(item);
    }
}

#[tokio::test]
async fn naive_upload_returns_200() {
    let resp = app()
        .oneshot(
            Request::post("/naive-upload")
                .body(Body::from("vitals blob"))
                .expect("request builds"),
        )
        .await
        .expect("router is infallible");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "naive-upload is the demo sink"
    );
}

#[tokio::test]
async fn image_endpoint_is_reachable_and_never_500s() {
    // Phase-gated behaviour: 404 in the scaffold (no store yet), 200 + Content-Type after
    // Phase F. Either is acceptable; a 5xx is not.
    let resp = app()
        .oneshot(
            Request::get("/api/images/does-not-exist")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router is infallible");
    assert!(
        matches!(resp.status(), StatusCode::NOT_FOUND | StatusCode::OK),
        "image endpoint returned {}",
        resp.status()
    );
}
