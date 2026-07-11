//! `tgw-fhir` — maps `tgw_core::VitalsObservation` to FHIR **R5** Observation JSON.
//!
//! OWNER: Twaha. Reference: <https://hl7.org/fhir/observation.html> (R5, v5.0.0) — **not**
//! build.fhir.org (that serves the R6 ballot; see docs/DECISIONS.md). The gateway is the
//! FHIR boundary: it emits genuine R5 JSON while the wire format stays compact.
//!
//! [`to_fhir_json`] maps vitals to an R5 `Observation`; [`image_media_json`] maps a delivered
//! image to an R5 `Media` resource. The executable spec is in `tests/r5_contract.rs`.

use time::format_description::well_known::Rfc3339;

use serde_json::{Value, json};
use tgw_core::clinical::{
    PLAUSIBLE_DIASTOLIC_MAX, PLAUSIBLE_DIASTOLIC_MIN, PLAUSIBLE_HEART_RATE_MAX,
    PLAUSIBLE_HEART_RATE_MIN, PLAUSIBLE_SPO2_MAX, PLAUSIBLE_SPO2_MIN, PLAUSIBLE_SYSTOLIC_MAX,
    PLAUSIBLE_SYSTOLIC_MIN,
};
use tgw_core::{Component, Measure, VitalsObservation};

/// LOINC coding system URL (`code.coding[].system` for clinical codes).
const LOINC_SYSTEM: &str = "http://loinc.org";
/// UCUM (Unified Code for Units of Measure) coding system URL (`valueQuantity.system`).
const UCUM_SYSTEM: &str = "http://unitsofmeasure.org";

/// Convert a [`VitalsObservation`] into a FHIR R5 `Observation` resource as JSON.
///
/// Required R5 elements to emit: `resourceType: "Observation"`, `status: "final"`,
/// `code.coding` (LOINC), `subject`, `effectiveDateTime` (RFC 3339), and either
/// `valueQuantity` (UCUM) for a single-valued reading or `component[]` for a panel (BP).
/// R5: "all observations SHOULD have a performer" — emit `performer` too.
pub fn to_fhir_json(obs: &VitalsObservation) -> Value {
    // RFC 3339 formatting of a valid `OffsetDateTime` is effectively infallible; on the
    // impossible failure path we emit an empty string rather than panic, which would surface
    // as a clear test failure instead of a crash.
    let effective = obs.effective.format(&Rfc3339).unwrap_or_default();

    let mut root = json!({
        "resourceType": "Observation",
        "status": "final",
        "code": { "coding": [loinc_coding(&obs.loinc)] },
        "subject": { "reference": format!("Patient/{}", obs.patient_id) },
        "effectiveDateTime": effective,
        "performer": [{ "reference": format!("Practitioner/{}", obs.performer_id) }],
        "device": { "reference": format!("Device/{}", obs.device_id) },
    });

    // Single-valued reading with no sub-components ⇒ top-level `valueQuantity`.
    // A panel (`components` non-empty) ⇒ `component[]` and NO top-level `valueQuantity`,
    // even if `value` were `Some` (the BP fixture has `value: None`).
    if obs.components.is_empty() {
        if let Some(m) = &obs.value {
            root["valueQuantity"] = value_quantity(m);
        }
    } else {
        let components: Vec<Value> = obs
            .components
            .iter()
            .map(|c: &Component| {
                json!({
                    "code": { "coding": [loinc_coding(&c.loinc)] },
                    "valueQuantity": value_quantity(&c.value),
                })
            })
            .collect();
        root["component"] = Value::Array(components);
    }

    root
}

/// Convert a delivered image bundle into a FHIR R5 `Media` resource as JSON.
///
/// The wire model carries no image→observation link, so a true `Observation.derivedFrom`
/// has nothing to reference; a `Media` resource is the standards-honest representation of a
/// standalone clinical image. Emits `resourceType: "Media"`, `status: "completed"`, the
/// patient `subject`, and a `content` attachment pointing at the gateway's image URL.
pub fn image_media_json(patient_id: &str, mime: &str, image_url: &str) -> Value {
    json!({
        "resourceType": "Media",
        "status": "completed",
        "subject": { "reference": format!("Patient/{patient_id}") },
        "content": {
            "contentType": mime,
            "url": image_url,
        },
    })
}

/// Build a single `coding` object: `{ system: LOINC, code: <loinc> }`.
///
/// The free-text `display` string is intentionally omitted — it is not part of the coded
/// contract (the golden test normalises it away), and the spec forbids inventing values.
fn loinc_coding(loinc: &str) -> Value {
    json!({
        "system": LOINC_SYSTEM,
        "code": loinc,
        "display": loinc_display(loinc),
    })
}

/// Return the fixture-defined display text for the supported clinical LOINC codes.
fn loinc_display(loinc: &str) -> Option<&'static str> {
    match loinc {
        "85354-9" => Some("Blood pressure panel with all children optional"),
        "8480-6" => Some("Systolic blood pressure"),
        "8462-4" => Some("Diastolic blood pressure"),
        "59408-5" => Some("Oxygen saturation in Arterial blood by Pulse oximetry"),
        "8867-4" => Some("Heart rate"),
        _ => None,
    }
}

/// Build a FHIR `valueQuantity`: `{ value, unit, system: UCUM, code: <UCUM> }`.
///
/// `code` is the raw UCUM code from [`Measure::ucum_unit`]; `unit` is a human-readable label
/// derived by stripping UCUM annotation brackets (e.g. `mm[Hg]` → `mmHg`). Codes without
/// brackets (`%`, `/min`) pass through unchanged, so `unit == code` for those.
fn value_quantity(m: &Measure) -> Value {
    json!({
        "value": m.value,
        "unit": display_unit(&m.ucum_unit),
        "system": UCUM_SYSTEM,
        "code": m.ucum_unit.clone(),
    })
}

/// Derive a human-readable unit label from a UCUM code by removing annotation brackets.
///
/// UCUM denotes annotations like `[Hg]` (millimetres of mercury) in brackets; the conventional
/// clinical display spelling drops them (`mm[Hg]` → `mmHg`). Bracket-free codes are unchanged.
fn display_unit(ucum: &str) -> String {
    ucum.chars().filter(|ch| *ch != '[' && *ch != ']').collect()
}

// ---------------------------------------------------------------------------------------
// Clinical plausibility (Fix 1c) — additive flagging, never rejection.
// ---------------------------------------------------------------------------------------
//
// AEAD proves the bytes arrived intact; it says nothing about whether the sensor reading is
// physiologically sane. `plausibility_flags` runs after decrypt + FHIR mapping and returns a
// list of advisory flags. An out-of-range or inconsistent value is NEVER dropped or refused —
// it is stored and surfaced with its flags so the dashboard can mark it "verify" without ever
// hiding a possibly-real emergency reading.
//
// !!! EVERY numeric bound used below is a PLACEHOLDER marked `NEEDS CLINICIAN REVIEW`. The
// values are now single-sourced from `tgw_core::clinical::PLAUSIBLE_*` (Fix F5) so the field
// hard-reject tier and this advisory tier cannot silently diverge; clinician sign-off is a
// one-file edit there.

/// Compute advisory plausibility flags for one observation.
///
/// Returns an empty vector for a clean, in-range, internally-consistent reading, and for any
/// LOINC code without a defined range (no opinion is offered rather than a false alarm). The
/// caller stores these flags alongside the observation; they are additive metadata and must
/// never gate persistence or produce an API error.
#[must_use]
pub fn plausibility_flags(obs: &VitalsObservation) -> Vec<String> {
    let mut flags = Vec::new();
    match obs.loinc.as_str() {
        // Blood-pressure panel: validate each component and their mutual consistency.
        "85354-9" => {
            let systolic = component_value(obs, "8480-6");
            let diastolic = component_value(obs, "8462-4");
            match systolic {
                Some(s) if !in_range(s, PLAUSIBLE_SYSTOLIC_MIN, PLAUSIBLE_SYSTOLIC_MAX) => {
                    flags.push("systolic-out-of-range".to_string());
                }
                None => flags.push("missing-systolic".to_string()),
                _ => {}
            }
            match diastolic {
                Some(d) if !in_range(d, PLAUSIBLE_DIASTOLIC_MIN, PLAUSIBLE_DIASTOLIC_MAX) => {
                    flags.push("diastolic-out-of-range".to_string());
                }
                None => flags.push("missing-diastolic".to_string()),
                _ => {}
            }
            if let (Some(s), Some(d)) = (systolic, diastolic)
                && s <= d
            {
                flags.push("systolic-not-greater-than-diastolic".to_string());
            }
        }
        "8867-4" => flag_single(
            obs,
            PLAUSIBLE_HEART_RATE_MIN,
            PLAUSIBLE_HEART_RATE_MAX,
            "heart-rate",
            &mut flags,
        ),
        "59408-5" => flag_single(
            obs,
            PLAUSIBLE_SPO2_MIN,
            PLAUSIBLE_SPO2_MAX,
            "spo2",
            &mut flags,
        ),
        "8480-6" => flag_single(
            obs,
            PLAUSIBLE_SYSTOLIC_MIN,
            PLAUSIBLE_SYSTOLIC_MAX,
            "systolic",
            &mut flags,
        ),
        "8462-4" => flag_single(
            obs,
            PLAUSIBLE_DIASTOLIC_MIN,
            PLAUSIBLE_DIASTOLIC_MAX,
            "diastolic",
            &mut flags,
        ),
        // No defined plausible range for this code: offer no opinion, never reject.
        _ => {}
    }
    flags
}

/// Flag a single-valued observation: missing value, or value outside `[min, max]`.
fn flag_single(obs: &VitalsObservation, min: f64, max: f64, name: &str, flags: &mut Vec<String>) {
    match obs.value.as_ref().map(|m| m.value) {
        Some(v) if !in_range(v, min, max) => flags.push(format!("{name}-out-of-range")),
        None => flags.push(format!("{name}-missing-value")),
        _ => {}
    }
}

/// The numeric value of the panel component with LOINC `loinc`, if present.
fn component_value(obs: &VitalsObservation, loinc: &str) -> Option<f64> {
    obs.components
        .iter()
        .find(|c| c.loinc == loinc)
        .map(|c| c.value.value)
}

/// Inclusive range check that also rejects NaN (`NaN` fails both comparisons).
fn in_range(value: f64, min: f64, max: f64) -> bool {
    value >= min && value <= max
}

// The FHIR-mapping verification suite lives in `tests/r5_contract.rs`; the plausibility spec
// lives in `tests/plausibility.rs`.
