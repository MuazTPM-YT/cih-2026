//! Fix 1c end-to-end: a flagged observation is persisted and retrievable via the real
//! `/api/observations` endpoint WITH its flag, an in-range one carries no flags, and flagging
//! is never a rejection path — the endpoint returns 200, never a 4xx/5xx, for flagged data.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tgw_core::{Measure, VitalsObservation};
use tgw_fhir::{plausibility_flags, to_fhir_json};
use tgw_gateway::{AppState, Store, StoreState, router_with_store};
use time::macros::datetime;
use tower::ServiceExt;
use uuid::Uuid;

fn single(loinc: &str, value: f64, unit: &str) -> VitalsObservation {
    VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: loinc.into(),
        effective: datetime!(2026-07-11 14:03:22 UTC),
        value: Some(Measure {
            value,
            ucum_unit: unit.into(),
        }),
        components: vec![],
        device_id: "field-device".into(),
        performer_id: "field-worker".into(),
    }
}

/// Persist a vitals observation exactly as the gateway ingest path does (FHIR + flags).
fn ingest(store: &Store, id: Uuid, obs: &VitalsObservation, received_at: &str) {
    store
        .complete_vitals(
            id,
            &[to_fhir_json(obs)],
            &[plausibility_flags(obs)],
            received_at,
        )
        .expect("ingest vitals");
}

async fn get_observations(state: StoreState) -> (StatusCode, Value) {
    let resp = router_with_store(state)
        .oneshot(
            Request::get("/api/observations")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router is infallible");
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body collects");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn fresh_store(tag: &str) -> Arc<Store> {
    let path =
        std::env::temp_dir().join(format!("tgw-flags-api-{}-{tag}.redb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Arc::new(Store::open(&path).expect("open store"))
}

#[tokio::test]
async fn flagged_and_clean_observations_are_both_retrievable_with_correct_flags() {
    let store = fresh_store("flag-surfacing");

    // SpO2 of 105% is physically impossible → must be flagged, but still stored.
    let flagged_id = Uuid::new_v4();
    ingest(
        &store,
        flagged_id,
        &single("59408-5", 105.0, "%"),
        "2026-07-11T14:03:22Z",
    );
    // A normal pulse → no flags.
    let clean_id = Uuid::new_v4();
    ingest(
        &store,
        clean_id,
        &single("8867-4", 78.0, "/min"),
        "2026-07-11T14:03:20Z",
    );

    let state = StoreState {
        base: AppState {
            static_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static"),
        },
        store,
    };
    let (status, json) = get_observations(state).await;

    // Flagging is additive metadata, never a rejection: the endpoint must succeed.
    assert_eq!(
        status,
        StatusCode::OK,
        "flagged data must not cause a 4xx/5xx"
    );

    let arr = json.as_array().expect("observations is an array");
    let find = |id: Uuid| {
        arr.iter()
            .find(|i| i["bundle_id"] == id.to_string())
            .unwrap_or_else(|| panic!("bundle {id} missing from response"))
    };

    let flagged = find(flagged_id);
    let flags = flagged["flags"]
        .as_array()
        .expect("flagged item exposes a flags array");
    assert!(
        flags.iter().any(|f| f == "spo2-out-of-range"),
        "the out-of-range SpO2 must be surfaced with a flag, got {flags:?}"
    );

    let clean = find(clean_id);
    assert!(
        clean["flags"]
            .as_array()
            .expect("clean item exposes a flags array")
            .is_empty(),
        "an in-range reading must carry no flags"
    );
}
