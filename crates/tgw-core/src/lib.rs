//! `tgw-core` — bundle model, wire protocol, FEC framing, crypto envelope.
//!
//! # PLACEHOLDER (owned by Muaz — `muaz/core`)
//!
//! This file exists ONLY so Twaha's crates (`tgw-fhir`, `tgw-gateway`, `tgw-netsim`, the
//! integration test) can compile against the frozen **Contract 1** API from
//! `tasks/CONTRACTS.md`. The type shapes and function signatures below are the contract and
//! must not drift without a team sync. The bodies are `todo!()` — Muaz fills the real
//! transport/crypto logic on his branch, which is authoritative.
//!
//! Twaha (and the coding agent): **consume these, never edit this crate.**

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A single UCUM-coded measurement (e.g. `{ value: 91.0, ucum_unit: "%" }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measure {
    pub value: f64,
    pub ucum_unit: String,
}

/// One component of a multi-valued observation (e.g. systolic within a BP panel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    pub loinc: String,
    pub value: Measure,
}

/// A clinical vitals reading, constrained to map losslessly to a FHIR R5 Observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VitalsObservation {
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
    pub device_id: String,
    pub performer_id: String,
}

/// The payload carried by a bundle: a batch of vitals, or a single image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BundlePayload {
    Vitals(Vec<VitalsObservation>),
    Image { mime: String, data: Vec<u8> },
}

/// Scheduling priority — vitals always preempt images.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Vitals,
    Image,
}

/// A unit of store-and-forward delivery, keyed by `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub id: Uuid,
    pub priority: Priority,
    pub payload: BundlePayload,
}

/// A single UDP datagram, ready to send.
pub type Datagram = Vec<u8>;

/// 256-bit pre-shared key for XChaCha20-Poly1305. Loaded from a file; never logged.
#[derive(Clone)]
pub struct Key {
    #[allow(dead_code)] // read by the real crypto impl on Muaz's branch
    bytes: [u8; 32],
}

impl Key {
    /// Load a 32-byte PSK from `path`. Never hard-code a key.
    pub fn from_file(path: &std::path::Path) -> Result<Self, CoreError> {
        let _ = path;
        todo!("Muaz/Contract-1: load 32-byte PSK from file")
    }
}

/// FEC tuning knobs (subset relevant to the receiver; full set in the config).
#[derive(Debug, Clone)]
pub struct FecConfig {
    pub symbol_size: u16,
    pub overhead_factor: f32,
}

/// A repair request from the gateway back to the field client.
#[derive(Debug, Clone)]
pub struct NackFrame {
    pub bundle_id: Uuid,
    /// Additional symbols needed, per source block.
    pub needed: Vec<u32>,
}

/// A parsed wire frame (Contract 2: `0x01 DATA`, `0x02 NACK`, `0x03 RECEIPT`).
#[derive(Debug, Clone)]
pub enum Frame {
    Data { bundle_id: Uuid },
    Nack(NackFrame),
    Receipt { bundle_id: Uuid, delivered: bool },
}

/// Result of feeding one datagram to a [`BundleReceiver`].
#[derive(Debug)]
pub enum Absorb {
    /// Not enough symbols yet; keep absorbing.
    NeedMore,
    /// Bundle fully decoded and authenticated.
    Complete(Bundle),
    /// Decode has stalled; the gateway should send this NACK to request repair symbols.
    Nack(NackFrame),
}

/// Reassembles one in-flight bundle from incoming symbols. One instance per `Uuid`.
pub struct BundleReceiver;

impl Default for BundleReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl BundleReceiver {
    /// Create a receiver for a new in-flight bundle.
    pub fn new() -> Self {
        BundleReceiver
    }

    /// Feed one datagram's worth of symbol data; drive the decode state machine.
    pub fn absorb(&mut self, dgram: &[u8]) -> Result<Absorb, CoreError> {
        let _ = dgram;
        todo!("Muaz/Contract-1: RaptorQ decode + AEAD open state machine")
    }
}

/// Encode a bundle into paced UDP `DATA` datagrams (field → gateway).
pub fn encode_bundle(b: &Bundle, key: &Key, cfg: &FecConfig) -> Result<Vec<Datagram>, CoreError> {
    let _ = (b, key, cfg);
    todo!("Muaz/Contract-1: CBOR -> lz4 -> AEAD -> RaptorQ -> DATA frames")
}

/// Build an AEAD-authenticated `DELIVERED` receipt (gateway → field).
pub fn build_receipt(bundle_id: Uuid, key: &Key) -> Vec<u8> {
    let _ = (bundle_id, key);
    todo!("Muaz/Contract-1: RECEIPT frame, AEAD tag over UUID+status")
}

/// Dispatch a raw datagram to its [`Frame`] type.
pub fn parse_frame(dgram: &[u8]) -> Result<Frame, CoreError> {
    let _ = dgram;
    todo!("Muaz/Contract-1: read [version|type|body], return Frame")
}

/// Errors surfaced by `tgw-core`. Libraries use `thiserror`; binaries use `anyhow`.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("AEAD authentication failure")]
    Crypto,
    #[error("malformed frame")]
    MalformedFrame,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
