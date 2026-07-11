//! Executable spec for `tgw_fhir::to_fhir_json` (+ `image_media_json`). These tests ARE the
//! contract: make the code satisfy them — never weaken a test. FHIR R5 reference:
//! <https://hl7.org/fhir/observation.html> (R5, NOT build.fhir.org — see docs/DECISIONS.md).

use serde_json::Value;
use tgw_core::{Component, Measure, VitalsObservation};
use tgw_fhir::{image_media_json, to_fhir_json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::datetime;

// --- fixtures (match crates/tgw-gateway/static/mock/observations.json entry 0) ------------

fn effective() -> OffsetDateTime {
    datetime!(2026-07-11 14:03:22 UTC)
}

/// Blood-pressure panel (components path) — mirrors the golden fixture's first entry.
fn bp_observation() -> VitalsObservation {
    VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: "85354-9".into(),
        effective: effective(),
        value: None,
        components: vec![
            Component {
                loinc: "8480-6".into(),
                value: Measure {
                    value: 142.0,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
            Component {
                loinc: "8462-4".into(),
                value: Measure {
                    value: 95.0,
                    ucum_unit: "mm[Hg]".into(),
                },
            },
        ],
        device_id: "field-bp-01".into(),
        performer_id: "fieldworker-7".into(),
    }
}

/// SpO2 (single-value path).
fn spo2_observation() -> VitalsObservation {
    VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: "59408-5".into(),
        effective: effective(),
        value: Some(Measure {
            value: 91.0,
            ucum_unit: "%".into(),
        }),
        components: vec![],
        device_id: "field-spo2-01".into(),
        performer_id: "fieldworker-7".into(),
    }
}

/// Heart rate / pulse (single-value path, `/min`).
fn pulse_observation() -> VitalsObservation {
    VitalsObservation {
        patient_id: "P-1023".into(),
        loinc: "8867-4".into(),
        effective: effective(),
        value: Some(Measure {
            value: 108.0,
            ucum_unit: "/min".into(),
        }),
        components: vec![],
        device_id: "field-ecg-01".into(),
        performer_id: "fieldworker-7".into(),
    }
}

// --- helpers -----------------------------------------------------------------------------

/// Recursively normalise a `Value` for semantic comparison: coerce every number to f64 (so
/// `142` and `142.0` match) and drop human-readable `display` strings (free text, not part of
/// the coded contract).
fn normalize(v: &Value) -> Value {
    match v {
        Value::Number(n) => Value::from(n.as_f64().unwrap_or(f64::NAN)),
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| k.as_str() != "display")
                .map(|(k, val)| (k.clone(), normalize(val)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn coding0(fhir: &Value) -> &Value {
    &fhir["code"]["coding"][0]
}

// --- structural requirements (FHIR R5) ---------------------------------------------------

#[test]
fn resource_type_and_status() {
    let f = to_fhir_json(&spo2_observation());
    assert_eq!(f["resourceType"], "Observation");
    assert_eq!(f["status"], "final");
}

#[test]
fn code_is_loinc_matching_observation() {
    let obs = spo2_observation();
    let f = to_fhir_json(&obs);
    assert_eq!(coding0(&f)["system"], "http://loinc.org");
    assert_eq!(coding0(&f)["code"], obs.loinc);
}

#[test]
fn subject_and_performer_references() {
    let obs = spo2_observation();
    let f = to_fhir_json(&obs);
    assert_eq!(
        f["subject"]["reference"],
        format!("Patient/{}", obs.patient_id)
    );
    // R5: observations SHOULD have a performer.
    assert_eq!(
        f["performer"][0]["reference"],
        format!("Practitioner/{}", obs.performer_id)
    );
}

#[test]
fn effective_datetime_is_rfc3339_input_instant() {
    let obs = spo2_observation();
    let f = to_fhir_json(&obs);
    let expected = obs.effective.format(&Rfc3339).expect("rfc3339 formats");
    assert_eq!(f["effectiveDateTime"], Value::String(expected));
    // Sanity: the canonical fixture instant.
    assert_eq!(f["effectiveDateTime"], "2026-07-11T14:03:22Z");
}

#[test]
fn single_value_uses_value_quantity_not_component() {
    let obs = spo2_observation();
    let f = to_fhir_json(&obs);
    let vq = &f["valueQuantity"];
    assert_eq!(vq["value"].as_f64(), Some(91.0));
    assert_eq!(vq["unit"], "%");
    assert_eq!(vq["system"], "http://unitsofmeasure.org");
    assert_eq!(vq["code"], "%");
    assert!(
        f.get("component").is_none(),
        "single-value obs must not emit component[]"
    );
}

#[test]
fn pulse_value_quantity_ucum_per_minute() {
    let f = to_fhir_json(&pulse_observation());
    assert_eq!(coding0(&f)["code"], "8867-4");
    assert_eq!(f["valueQuantity"]["value"].as_f64(), Some(108.0));
    assert_eq!(f["valueQuantity"]["code"], "/min");
}

#[test]
fn bp_panel_emits_two_components_with_correct_codes_and_units() {
    let f = to_fhir_json(&bp_observation());
    let comps = f["component"]
        .as_array()
        .expect("component[] present for BP");
    assert_eq!(comps.len(), 2, "BP panel has systolic + diastolic");

    let systolic = &comps[0];
    assert_eq!(systolic["code"]["coding"][0]["code"], "8480-6");
    assert_eq!(systolic["valueQuantity"]["value"].as_f64(), Some(142.0));
    assert_eq!(systolic["valueQuantity"]["unit"], "mmHg");
    assert_eq!(
        systolic["valueQuantity"]["system"],
        "http://unitsofmeasure.org"
    );
    assert_eq!(systolic["valueQuantity"]["code"], "mm[Hg]");

    let diastolic = &comps[1];
    assert_eq!(diastolic["code"]["coding"][0]["code"], "8462-4");
    assert_eq!(diastolic["valueQuantity"]["value"].as_f64(), Some(95.0));
    assert_eq!(diastolic["valueQuantity"]["code"], "mm[Hg]");

    assert!(
        f.get("valueQuantity").is_none(),
        "BP panel must not emit top-level valueQuantity"
    );
}

// --- golden test: impl output == the Contract-3 fixture (single source of truth) ---------

#[test]
fn bp_matches_golden_contract_fixture() {
    // The gateway fixture Jiya renders is the canonical expected FHIR output. Bind to it so
    // the mapper and the dashboard can never silently diverge.
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tgw-gateway/static/mock/observations.json"
    ))
    .expect("fixture is valid JSON");
    let golden = &fixture[0]["fhir"];

    let produced = to_fhir_json(&bp_observation());

    assert_eq!(
        normalize(&produced),
        normalize(golden),
        "to_fhir_json(bp) must match observations.json[0].fhir (numbers coerced, display ignored)"
    );
}

// --- round-trip: coded values survive struct -> JSON -> read-back -------------------------

#[test]
fn round_trip_values_stable() {
    let obs = bp_observation();
    let f = to_fhir_json(&obs);

    // Read the two component values back out and confirm they equal the source.
    let comps = f["component"].as_array().expect("component[]");
    assert_eq!(
        comps[0]["valueQuantity"]["value"].as_f64(),
        Some(obs.components[0].value.value)
    );
    assert_eq!(
        comps[1]["valueQuantity"]["value"].as_f64(),
        Some(obs.components[1].value.value)
    );
    assert_eq!(coding0(&f)["code"], obs.loinc);
    assert_eq!(
        f["subject"]["reference"],
        format!("Patient/{}", obs.patient_id)
    );
}

// --- edge: no value and no components is a client bug; the mapper must not panic ----------

#[test]
fn valueless_observation_does_not_panic_and_stays_valid() {
    let obs = VitalsObservation {
        patient_id: "P-9".into(),
        loinc: "8867-4".into(),
        effective: effective(),
        value: None,
        components: vec![],
        device_id: "d".into(),
        performer_id: "w".into(),
    };
    let f = to_fhir_json(&obs); // must return, not panic
    // Still a structurally valid Observation core.
    assert_eq!(f["resourceType"], "Observation");
    assert_eq!(f["status"], "final");
    // With no measurement, it must NOT fabricate a value.
    assert!(f.get("valueQuantity").is_none());
    assert!(f.get("component").is_none());
}

// --- images map to a FHIR Media resource (the standards-honest linkage) --------------------

#[test]
fn image_maps_to_fhir_media_resource() {
    let media = image_media_json("P-1023", "image/jpeg", "/api/images/abc-123");
    assert_eq!(media["resourceType"], "Media");
    assert_eq!(media["status"], "completed");
    assert_eq!(media["subject"]["reference"], "Patient/P-1023");
    assert_eq!(media["content"]["contentType"], "image/jpeg");
    assert_eq!(media["content"]["url"], "/api/images/abc-123");
}
