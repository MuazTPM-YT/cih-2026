//! Bundle encode pipeline (docs/ARCHITECTURE.md §4):
//! `clinical struct → CBOR (ciborium) → lz4_flex → XChaCha20-Poly1305 → sealed envelope`.
//!
//! The sealed envelope is what RaptorQ encodes and what sits in the redb queue at rest —
//! FEC symbols carry ciphertext only, so a partial symbol set teaches an eavesdropper
//! nothing, and the field device never stores PHI in the clear.
//!
//! Envelope layout: `nonce (24 B) || ciphertext+tag`. The AEAD associated data is
//! `[wire version | bundle UUID]`, binding the ciphertext to the bundle identity so a
//! captured envelope cannot be replayed under a different bundle id.

use chacha20poly1305::XNonce;
use chacha20poly1305::aead::{Aead, Payload};
use rand::RngCore;
use rand::rngs::OsRng;
use uuid::Uuid;

use crate::error::CoreError;
use crate::key::Key;
use crate::model::Bundle;
use crate::wire::WIRE_VERSION;

/// XChaCha20 nonce length (24 bytes; random per message is safe at this size).
pub(crate) const NONCE_LEN: usize = 24;
/// Poly1305 tag length.
pub(crate) const TAG_LEN: usize = 16;

/// AEAD associated data for a bundle: wire version + UUID.
fn associated_data(bundle_id: Uuid) -> [u8; 17] {
    let mut aad = [0u8; 17];
    aad[0] = WIRE_VERSION;
    aad[1..].copy_from_slice(bundle_id.as_bytes());
    aad
}

/// Seal a bundle into its encrypted envelope: CBOR → lz4 → AEAD.
pub fn seal_bundle(bundle: &Bundle, key: &Key) -> Result<Vec<u8>, CoreError> {
    let mut cbor = Vec::new();
    ciborium::into_writer(bundle, &mut cbor)
        .map_err(|e| CoreError::Encode(format!("CBOR: {e}")))?;

    let compressed = lz4_flex::compress_prepend_size(&cbor);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);

    let aad = associated_data(bundle.id);
    let ciphertext = key
        .cipher()
        .encrypt(
            &nonce,
            Payload {
                msg: &compressed,
                aad: &aad,
            },
        )
        .map_err(|_| CoreError::Crypto)?;

    let mut envelope = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);
    Ok(envelope)
}

/// Open a sealed envelope back into a [`Bundle`]: AEAD → lz4 → CBOR.
///
/// Any failure — truncation, tampering, wrong key, wrong `bundle_id` — yields
/// [`CoreError::Crypto`] or [`CoreError::Decode`] and **no partial data** (ARCHITECTURE.md
/// §4: "AEAD failure ⇒ drop bundle, log, never partial-accept").
pub fn open_envelope(bundle_id: Uuid, envelope: &[u8], key: &Key) -> Result<Bundle, CoreError> {
    if envelope.len() < NONCE_LEN + TAG_LEN {
        return Err(CoreError::Crypto);
    }
    let (nonce_slice, ciphertext) = envelope.split_at(NONCE_LEN);
    let nonce_bytes: [u8; NONCE_LEN] = nonce_slice.try_into().map_err(|_| CoreError::Crypto)?;
    let nonce = XNonce::from(nonce_bytes);

    let aad = associated_data(bundle_id);
    let compressed = key
        .cipher()
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CoreError::Crypto)?;

    let cbor = lz4_flex::decompress_size_prepended(&compressed)
        .map_err(|e| CoreError::Decode(format!("lz4: {e}")))?;

    let bundle: Bundle = ciborium::from_reader(cbor.as_slice())
        .map_err(|e| CoreError::Decode(format!("CBOR: {e}")))?;

    // The UUID inside the payload must agree with the identity the symbols claimed —
    // defense in depth on top of the AAD binding.
    if bundle.id != bundle_id {
        return Err(CoreError::Crypto);
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BundlePayload, Priority};

    fn test_bundle() -> Bundle {
        Bundle {
            id: Uuid::new_v4(),
            priority: Priority::Vitals,
            payload: BundlePayload::Image {
                mime: "application/octet-stream".into(),
                // Repetitive so the lz4 stage demonstrably shrinks it.
                data: vec![0xAB; 4096],
                patient_id: "P-TEST".into(),
            },
        }
    }

    #[test]
    fn seal_open_round_trip() {
        let key = Key::generate();
        let bundle = test_bundle();
        let envelope = match seal_bundle(&bundle, &key) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };
        let reopened = match open_envelope(bundle.id, &envelope, &key) {
            Ok(b) => b,
            Err(e) => panic!("open must succeed: {e}"),
        };
        assert_eq!(reopened, bundle);
    }

    #[test]
    fn compression_shrinks_repetitive_payloads() {
        let key = Key::generate();
        let bundle = test_bundle(); // 4 KiB of a repeated byte
        let envelope = match seal_bundle(&bundle, &key) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };
        assert!(
            envelope.len() < 1024,
            "4 KiB of repeated bytes should compress well below 1 KiB, got {}",
            envelope.len()
        );
    }

    #[test]
    fn tamper_is_rejected() {
        let key = Key::generate();
        let bundle = test_bundle();
        let mut envelope = match seal_bundle(&bundle, &key) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };
        let mid = envelope.len() / 2;
        envelope[mid] ^= 0x01;
        assert!(
            matches!(
                open_envelope(bundle.id, &envelope, &key),
                Err(CoreError::Crypto)
            ),
            "a single flipped bit must fail authentication"
        );
    }

    #[test]
    fn wrong_key_is_rejected() {
        let bundle = test_bundle();
        let envelope = match seal_bundle(&bundle, &Key::generate()) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };
        assert!(matches!(
            open_envelope(bundle.id, &envelope, &Key::generate()),
            Err(CoreError::Crypto)
        ));
    }

    #[test]
    fn wrong_bundle_id_is_rejected() {
        let key = Key::generate();
        let bundle = test_bundle();
        let envelope = match seal_bundle(&bundle, &key) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };
        assert!(
            matches!(
                open_envelope(Uuid::new_v4(), &envelope, &key),
                Err(CoreError::Crypto)
            ),
            "AAD binds the envelope to its bundle id; a different id must fail"
        );
    }

    #[test]
    fn truncated_envelope_is_rejected() {
        let key = Key::generate();
        let bundle = test_bundle();
        assert!(matches!(
            open_envelope(bundle.id, &[0u8; 10], &key),
            Err(CoreError::Crypto)
        ));
    }
}
