//! `tgw-core` — bundle model, wire protocol, FEC framing, crypto envelope, config.
//!
//! The transport core of the low-bandwidth telemedicine gateway (docs/ARCHITECTURE.md):
//! clinical bundles are CBOR-encoded, lz4-compressed, sealed with XChaCha20-Poly1305,
//! fountain-coded with RaptorQ (RFC 6330), and shipped as single-UDP-datagram symbols
//! that survive >20% packet loss inside a <64 kbps budget.
//!
//! The public surface at the crate root is **Contract 1** (tasks/CONTRACTS.md) — frozen;
//! changing signatures requires a team sync. All logic is I/O-free and unit-tested here;
//! the binaries (`tgw-field`, `tgw-gateway`) are thin async shells.
//!
//! # Pipeline
//!
//! ```text
//! sender:   Bundle → CBOR → lz4 → AEAD(seal) → RaptorQ symbols → DATA datagrams
//! receiver: DATA datagrams → RaptorQ decode → AEAD(open) → lz4 → CBOR → Bundle
//! backstop: receiver stalls ⇒ NACK ⇒ sender mints fresh repair symbols
//! receipt:  gateway → authenticated DELIVERED ⇒ field clears the bundle
//! ```
//!
//! Store-and-forward resume: `seal_bundle` once at capture, persist the sealed envelope
//! (redb), rebuild the sender later with [`BundleSender::from_envelope`] — plaintext
//! never sits at rest and re-bursts never re-encrypt.
//!
//! OWNER: Muaz (`muaz/core`). Teammates: consume, never edit.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod config;
mod envelope;
mod error;
mod fec;
mod key;
mod model;
mod wire;

pub use config::{
    Config, CryptoConfig, FecConfig, LinkConfig, MediaConfig, NetConfig, RetryConfig,
};
pub use envelope::{open_envelope, seal_bundle};
pub use error::CoreError;
pub use fec::{Absorb, BundleReceiver, BundleSender, encode_bundle};
pub use key::{KEY_LEN, Key};
pub use model::{Bundle, BundlePayload, Component, Datagram, Measure, Priority, VitalsObservation};
pub use wire::{
    FRAME_DATA, FRAME_NACK, FRAME_RECEIPT, Frame, NackFrame, WIRE_VERSION, build_receipt,
    encode_nack, parse_frame, verify_receipt,
};
