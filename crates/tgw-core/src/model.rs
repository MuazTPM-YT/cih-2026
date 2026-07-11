//! Clinical bundle model — Contract 1 types (tasks/CONTRACTS.md).
//!
//! Shapes are frozen: changing anything here requires a team sync and a PR that edits
//! CONTRACTS.md. All types are serde-capable; the wire form is CBOR (ciborium).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A single UCUM-coded measurement (e.g. `{ value: 91.0, ucum_unit: "%" }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    /// Numeric reading.
    pub value: f64,
    /// UCUM unit code, e.g. `"mm[Hg]"`, `"%"`, `"/min"`.
    pub ucum_unit: String,
}

/// One component of a multi-valued observation (e.g. systolic within a BP panel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    /// LOINC code of the component, e.g. `"8480-6"` (systolic).
    pub loinc: String,
    /// The component's measurement.
    pub value: Measure,
}

/// A clinical vitals reading, constrained to map losslessly to a FHIR R5 Observation
/// (docs/ARCHITECTURE.md §5). The gateway is the FHIR boundary; this struct is the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VitalsObservation {
    /// Patient reference/identifier, e.g. `"P-1023"`.
    pub patient_id: String,
    /// LOINC code for the observation, e.g. `"85354-9"` (blood pressure panel).
    pub loinc: String,
    /// Effective instant (RFC 3339 on the wire and in JSON).
    #[serde(with = "time::serde::rfc3339")]
    pub effective: OffsetDateTime,
    /// Single-valued reading (`None` when the reading is expressed via `components`).
    pub value: Option<Measure>,
    /// Sub-components (e.g. systolic `8480-6` / diastolic `8462-4` for a BP panel).
    pub components: Vec<Component>,
    /// Field device identifier (FHIR `device`).
    pub device_id: String,
    /// Field worker identifier (FHIR `performer`; R5: observations SHOULD have one).
    pub performer_id: String,
}

/// The payload carried by a bundle: a batch of vitals, or a single image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BundlePayload {
    /// One or more vitals observations captured together.
    Vitals(Vec<VitalsObservation>),
    /// A single image (already recompressed to fit `image_max_bytes`).
    Image {
        /// MIME type, e.g. `"image/jpeg"`.
        mime: String,
        /// Raw image bytes.
        data: Vec<u8>,
        /// Patient the image belongs to — Contract 3's image entries surface
        /// `patient_id`, so it must ride in the bundle. (Contract 1 delta, flagged:
        /// added 2026-07-11 with the H2–6 implementation.)
        patient_id: String,
    },
}

/// Scheduling priority — vitals always preempt images (docs/ARCHITECTURE.md §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    /// Sub-KB critical readings; always sent first.
    Vitals,
    /// Larger, delay-tolerant media.
    Image,
}

impl Priority {
    /// Scheduling rank: lower sends first. Keeps ordering logic in one place.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Priority::Vitals => 0,
            Priority::Image => 1,
        }
    }
}

/// A unit of store-and-forward delivery, keyed by `id`.
///
/// A bundle is only ever cleared by an authenticated `DELIVERED` receipt carrying this
/// `id`; retransmits are harmless because the gateway dedups on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    /// Delivery/dedup key. Also the AEAD associated data, so a ciphertext cannot be
    /// replayed under a different bundle identity.
    pub id: Uuid,
    /// Scheduling class.
    pub priority: Priority,
    /// The clinical payload.
    pub payload: BundlePayload,
}

impl Bundle {
    /// New vitals bundle with a fresh v4 UUID.
    #[must_use]
    pub fn new_vitals(observations: Vec<VitalsObservation>) -> Self {
        Bundle {
            id: Uuid::new_v4(),
            priority: Priority::Vitals,
            payload: BundlePayload::Vitals(observations),
        }
    }

    /// New image bundle with a fresh v4 UUID.
    #[must_use]
    pub fn new_image(mime: String, data: Vec<u8>, patient_id: String) -> Self {
        Bundle {
            id: Uuid::new_v4(),
            priority: Priority::Image,
            payload: BundlePayload::Image {
                mime,
                data,
                patient_id,
            },
        }
    }
}

/// A single UDP datagram, ready to send.
pub type Datagram = Vec<u8>;
