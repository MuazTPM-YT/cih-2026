//! Wire protocol v1 (Contract 2, tasks/CONTRACTS.md).
//!
//! Every datagram: `[1 B version | 1 B frame type | body]`.
//! `0x01 DATA` (field → gw), `0x02 NACK` (gw → field), `0x03 RECEIPT` (gw → field).
//! Frame-type byte layout is the frozen contract; body internals are core-private.
//!
//! DATA body: `uuid (16) | OTI (12) | RaptorQ EncodingPacket (4 B PayloadId + symbol) |
//! integrity tag (8)`. The 12-byte OTI rides in **every** DATA frame (≈1% overhead at symbol
//! size 1100) so the receiver can initialize its decoder from whichever symbol arrives first —
//! with 25% loss there is no "reliable first packet" to pin it to.
//!
//! The trailing 8-byte tag is a truncated HMAC-SHA256 over the whole datagram-minus-tag,
//! keyed by an HKDF-derived subkey ([`Key::derive_subkey`]) with domain separation from the
//! envelope/receipt AEAD. Radio corruption that survives the UDP checksum is caught here and
//! the datagram is dropped **before** it reaches the RaptorQ decoder, so a bit-flipped symbol
//! can never poison an otherwise-recoverable bundle or inflate the receiver's symbol count.
//! Wire-format note: this changes the on-wire DATA layout from wire v1's original framing —
//! both endpoints run this build, so there is no interop break, but a v1-original peer cannot
//! talk to a tagged peer.
//!
//! NACK body: `uuid (16) | block count (1) | needed-per-block (4 each, BE)`.
//!
//! RECEIPT body: `uuid (16) | status (1) | nonce (24) | AEAD tag (16)`. The tag
//! authenticates `version|uuid|status` under the shared key, so only the true gateway
//! can clear a bundle from the field queue.

use chacha20poly1305::XNonce;
use chacha20poly1305::aead::{Aead, Payload};
use rand::RngCore;
use rand::rngs::OsRng;
use uuid::Uuid;

use crate::envelope::{NONCE_LEN, TAG_LEN};
use crate::error::CoreError;
use crate::key::Key;
use crate::mac::{ct_eq, hmac_sha256};
use crate::model::Datagram;

/// Protocol version byte (Contract 2).
pub const WIRE_VERSION: u8 = 0x01;
/// Frame type: FEC symbol carrying encrypted bundle data.
pub const FRAME_DATA: u8 = 0x01;
/// Frame type: repair request.
pub const FRAME_NACK: u8 = 0x02;
/// Frame type: authenticated delivery receipt.
pub const FRAME_RECEIPT: u8 = 0x03;

/// `DELIVERED` status byte inside a RECEIPT frame.
const RECEIPT_DELIVERED: u8 = 0x01;

const HEADER_LEN: usize = 2;
const UUID_LEN: usize = 16;
/// Serialized `ObjectTransmissionInformation` length (RFC 6330 OTI).
pub(crate) const OTI_LEN: usize = 12;
const RECEIPT_LEN: usize = HEADER_LEN + UUID_LEN + 1 + NONCE_LEN + TAG_LEN;

/// Truncated-HMAC integrity tag length on every DATA frame (8 bytes ≈ 0.7% overhead at
/// symbol size 1100; a 64-bit forgery bound is ample for a per-datagram drop decision).
const DATA_TAG_LEN: usize = 8;

/// HKDF `info` label for the DATA-frame integrity subkey. Distinct from the AEAD key space,
/// so the integrity MAC and the envelope cipher never share key material.
pub(crate) const DATA_INTEGRITY_LABEL: &[u8] = b"tgw/data-integrity/v1";

/// Derive the DATA-frame integrity subkey from the shared PSK. Sender and receiver both call
/// this so their tags agree; a peer with the wrong PSK derives a different subkey and its
/// datagrams are rejected at the integrity gate, before decode.
pub(crate) fn data_subkey(key: &Key) -> [u8; 32] {
    key.derive_subkey(DATA_INTEGRITY_LABEL)
}

/// Verify a DATA datagram's trailing integrity tag under `subkey`.
///
/// Recomputes the truncated HMAC over the datagram minus its tag and compares in constant
/// time. A corrupt, truncated, or wrong-key datagram yields [`CoreError::MalformedFrame`] —
/// the "drop this datagram and carry on" error, never [`CoreError::Crypto`] (which is
/// reserved for a fully-reconstructed envelope failing AEAD).
pub(crate) fn verify_data_tag(dgram: &[u8], subkey: &[u8; 32]) -> Result<(), CoreError> {
    if dgram.len() <= DATA_TAG_LEN {
        return Err(CoreError::MalformedFrame);
    }
    let (covered, tag) = dgram.split_at(dgram.len() - DATA_TAG_LEN);
    let expected = hmac_sha256(subkey, covered);
    if ct_eq(&expected[..DATA_TAG_LEN], tag) {
        Ok(())
    } else {
        Err(CoreError::MalformedFrame)
    }
}

/// A repair request from the gateway back to the field client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackFrame {
    /// Bundle the gateway is still trying to decode.
    pub bundle_id: Uuid,
    /// Additional symbols needed, indexed by source block number.
    pub needed: Vec<u32>,
}

/// A parsed wire frame (Contract 2 dispatch: `Data | Nack | Receipt`).
///
/// `parse_frame` classifies without a key; RECEIPT authenticity must then be checked
/// with [`verify_receipt`] before acting on it.
#[derive(Debug, Clone)]
pub enum Frame {
    /// FEC symbol for `bundle_id` — route to that bundle's `BundleReceiver`.
    Data {
        /// Bundle the symbol belongs to.
        bundle_id: Uuid,
    },
    /// Repair request — route to the sending side's repair loop.
    Nack(NackFrame),
    /// Delivery receipt — **unverified**; call [`verify_receipt`] on the raw datagram.
    Receipt {
        /// Bundle the receipt claims to acknowledge.
        bundle_id: Uuid,
        /// Whether the status byte reads `DELIVERED`.
        delivered: bool,
    },
}

/// Dispatch a raw datagram to its [`Frame`] type (gateway and field RX loops).
pub fn parse_frame(dgram: &[u8]) -> Result<Frame, CoreError> {
    let (version, frame_type, body) = split_header(dgram)?;
    if version != WIRE_VERSION {
        return Err(CoreError::MalformedFrame);
    }
    match frame_type {
        FRAME_DATA => {
            let parts = parse_data_body(body)?;
            Ok(Frame::Data {
                bundle_id: parts.bundle_id,
            })
        }
        FRAME_NACK => Ok(Frame::Nack(parse_nack_body(body)?)),
        FRAME_RECEIPT => {
            if dgram.len() != RECEIPT_LEN {
                return Err(CoreError::MalformedFrame);
            }
            let bundle_id = read_uuid(body, 0)?;
            let delivered = body[UUID_LEN] == RECEIPT_DELIVERED;
            Ok(Frame::Receipt {
                bundle_id,
                delivered,
            })
        }
        _ => Err(CoreError::MalformedFrame),
    }
}

/// Build an AEAD-authenticated `DELIVERED` receipt datagram (gateway → field).
///
/// Infallible by construction (encrypting an empty message under a valid key cannot
/// fail), hence the `Vec<u8>` return per Contract 1.
#[must_use]
pub fn build_receipt(bundle_id: Uuid, key: &Key) -> Vec<u8> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = XNonce::from(nonce_bytes);
    let aad = receipt_aad(bundle_id, RECEIPT_DELIVERED);

    // Empty plaintext ⇒ ciphertext is exactly the 16-byte tag.
    let tag = key
        .cipher()
        .encrypt(
            &nonce,
            Payload {
                msg: &[],
                aad: &aad,
            },
        )
        .unwrap_or_default();
    debug_assert_eq!(tag.len(), TAG_LEN);

    let mut dgram = Vec::with_capacity(RECEIPT_LEN);
    dgram.push(WIRE_VERSION);
    dgram.push(FRAME_RECEIPT);
    dgram.extend_from_slice(bundle_id.as_bytes());
    dgram.push(RECEIPT_DELIVERED);
    dgram.extend_from_slice(&nonce_bytes);
    dgram.extend_from_slice(&tag);
    dgram
}

/// Verify a RECEIPT datagram's AEAD tag; returns the acknowledged bundle id.
///
/// The field client must clear a bundle **only** after this succeeds — an unauthenticated
/// receipt would let an attacker silently discard clinical data (ARCHITECTURE.md §6).
pub fn verify_receipt(dgram: &[u8], key: &Key) -> Result<Uuid, CoreError> {
    let (version, frame_type, body) = split_header(dgram)?;
    if version != WIRE_VERSION || frame_type != FRAME_RECEIPT || dgram.len() != RECEIPT_LEN {
        return Err(CoreError::MalformedFrame);
    }
    let bundle_id = read_uuid(body, 0)?;
    let status = body[UUID_LEN];
    if status != RECEIPT_DELIVERED {
        return Err(CoreError::MalformedFrame);
    }
    let nonce_start = UUID_LEN + 1;
    let nonce_bytes: [u8; NONCE_LEN] = body
        .get(nonce_start..nonce_start + NONCE_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(CoreError::MalformedFrame)?;
    let nonce = XNonce::from(nonce_bytes);
    let tag = &body[nonce_start + NONCE_LEN..];
    let aad = receipt_aad(bundle_id, status);

    key.cipher()
        .decrypt(
            &nonce,
            Payload {
                msg: tag,
                aad: &aad,
            },
        )
        .map_err(|_| CoreError::Crypto)?;
    Ok(bundle_id)
}

/// Serialize a NACK for transmission (gateway → field).
#[must_use]
pub fn encode_nack(nack: &NackFrame) -> Datagram {
    let mut dgram = Vec::with_capacity(HEADER_LEN + UUID_LEN + 1 + nack.needed.len() * 4);
    dgram.push(WIRE_VERSION);
    dgram.push(FRAME_NACK);
    dgram.extend_from_slice(nack.bundle_id.as_bytes());
    // Wire caps at 255 source blocks — RFC 6330's Z is a u8, so this is exact, and a
    // saturating cast keeps the no-panic guarantee even on a malformed request.
    dgram.push(u8::try_from(nack.needed.len()).unwrap_or(u8::MAX));
    for needed in nack.needed.iter().take(usize::from(u8::MAX)) {
        dgram.extend_from_slice(&needed.to_be_bytes());
    }
    dgram
}

fn parse_nack_body(body: &[u8]) -> Result<NackFrame, CoreError> {
    if body.len() < UUID_LEN + 1 {
        return Err(CoreError::MalformedFrame);
    }
    let bundle_id = read_uuid(body, 0)?;
    let count = usize::from(body[UUID_LEN]);
    let counts = &body[UUID_LEN + 1..];
    if counts.len() != count * 4 {
        return Err(CoreError::MalformedFrame);
    }
    let needed = counts
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    Ok(NackFrame { bundle_id, needed })
}

fn receipt_aad(bundle_id: Uuid, status: u8) -> [u8; 18] {
    let mut aad = [0u8; 18];
    aad[0] = WIRE_VERSION;
    aad[1..17].copy_from_slice(bundle_id.as_bytes());
    aad[17] = status;
    aad
}

fn split_header(dgram: &[u8]) -> Result<(u8, u8, &[u8]), CoreError> {
    if dgram.len() < HEADER_LEN {
        return Err(CoreError::MalformedFrame);
    }
    Ok((dgram[0], dgram[1], &dgram[HEADER_LEN..]))
}

fn read_uuid(body: &[u8], offset: usize) -> Result<Uuid, CoreError> {
    let bytes: [u8; UUID_LEN] = body
        .get(offset..offset + UUID_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(CoreError::MalformedFrame)?;
    Ok(Uuid::from_bytes(bytes))
}

/// Parsed pieces of a DATA frame body (core-internal; the FEC layer consumes this).
pub(crate) struct DataParts<'a> {
    pub bundle_id: Uuid,
    pub oti: [u8; OTI_LEN],
    pub packet: &'a [u8],
}

/// Build one DATA datagram: `header | uuid | OTI | serialized EncodingPacket | integrity tag`.
///
/// The tag is a truncated HMAC-SHA256 over everything preceding it, keyed by `subkey` (the
/// DATA-integrity subkey from [`data_subkey`]).
pub(crate) fn build_data_frame(
    bundle_id: Uuid,
    oti: &[u8; OTI_LEN],
    packet: &[u8],
    subkey: &[u8; 32],
) -> Datagram {
    let mut dgram =
        Vec::with_capacity(HEADER_LEN + UUID_LEN + OTI_LEN + packet.len() + DATA_TAG_LEN);
    dgram.push(WIRE_VERSION);
    dgram.push(FRAME_DATA);
    dgram.extend_from_slice(bundle_id.as_bytes());
    dgram.extend_from_slice(oti);
    dgram.extend_from_slice(packet);
    let tag = hmac_sha256(subkey, &dgram);
    dgram.extend_from_slice(&tag[..DATA_TAG_LEN]);
    dgram
}

/// Parse a DATA frame body (everything after the 2-byte header).
///
/// The trailing [`DATA_TAG_LEN`]-byte integrity tag is stripped from `packet` here; parsing
/// classifies and locates the symbol but does **not** authenticate — call [`verify_data_tag`]
/// (which the receiver does before absorbing) to reject corruption.
pub(crate) fn parse_data_body(body: &[u8]) -> Result<DataParts<'_>, CoreError> {
    // Minimum: uuid + OTI + PayloadId(4) + at least one symbol byte + integrity tag.
    if body.len() < UUID_LEN + OTI_LEN + 5 + DATA_TAG_LEN {
        return Err(CoreError::MalformedFrame);
    }
    let bundle_id = read_uuid(body, 0)?;
    let oti: [u8; OTI_LEN] = body
        .get(UUID_LEN..UUID_LEN + OTI_LEN)
        .and_then(|s| s.try_into().ok())
        .ok_or(CoreError::MalformedFrame)?;
    Ok(DataParts {
        bundle_id,
        oti,
        packet: &body[UUID_LEN + OTI_LEN..body.len() - DATA_TAG_LEN],
    })
}

/// Strip the 2-byte header from a full DATA datagram and parse the body.
pub(crate) fn parse_data_frame(dgram: &[u8]) -> Result<DataParts<'_>, CoreError> {
    let (version, frame_type, body) = split_header(dgram)?;
    if version != WIRE_VERSION || frame_type != FRAME_DATA {
        return Err(CoreError::MalformedFrame);
    }
    parse_data_body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_round_trip_verifies() {
        let key = Key::generate();
        let id = Uuid::new_v4();
        let dgram = build_receipt(id, &key);
        assert_eq!(dgram.len(), RECEIPT_LEN);

        match parse_frame(&dgram) {
            Ok(Frame::Receipt {
                bundle_id,
                delivered,
            }) => {
                assert_eq!(bundle_id, id);
                assert!(delivered);
            }
            other => panic!("expected Receipt, got {other:?}"),
        }
        match verify_receipt(&dgram, &key) {
            Ok(verified) => assert_eq!(verified, id),
            Err(e) => panic!("authentic receipt must verify: {e}"),
        }
    }

    #[test]
    fn tampered_receipt_fails_verification() {
        let key = Key::generate();
        let mut dgram = build_receipt(Uuid::new_v4(), &key);
        let last = dgram.len() - 1;
        dgram[last] ^= 0x01; // flip a tag bit
        assert!(matches!(
            verify_receipt(&dgram, &key),
            Err(CoreError::Crypto)
        ));
    }

    #[test]
    fn receipt_uuid_swap_fails_verification() {
        let key = Key::generate();
        let mut dgram = build_receipt(Uuid::new_v4(), &key);
        dgram[2..18].copy_from_slice(Uuid::new_v4().as_bytes());
        assert!(
            matches!(verify_receipt(&dgram, &key), Err(CoreError::Crypto)),
            "a receipt re-targeted at another bundle must not verify"
        );
    }

    #[test]
    fn receipt_wrong_key_fails_verification() {
        let dgram = build_receipt(Uuid::new_v4(), &Key::generate());
        assert!(matches!(
            verify_receipt(&dgram, &Key::generate()),
            Err(CoreError::Crypto)
        ));
    }

    #[test]
    fn nack_round_trips_through_parse_frame() {
        let nack = NackFrame {
            bundle_id: Uuid::new_v4(),
            needed: vec![3, 0, 7],
        };
        let dgram = encode_nack(&nack);
        match parse_frame(&dgram) {
            Ok(Frame::Nack(parsed)) => assert_eq!(parsed, nack),
            other => panic!("expected Nack, got {other:?}"),
        }
    }

    #[test]
    fn data_frame_round_trips() {
        let subkey = data_subkey(&Key::generate());
        let id = Uuid::new_v4();
        let oti = [7u8; OTI_LEN];
        let packet = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let dgram = build_data_frame(id, &oti, &packet, &subkey);

        match parse_frame(&dgram) {
            Ok(Frame::Data { bundle_id }) => assert_eq!(bundle_id, id),
            other => panic!("expected Data, got {other:?}"),
        }
        let parts = match parse_data_frame(&dgram) {
            Ok(p) => p,
            Err(e) => panic!("data frame must parse: {e}"),
        };
        assert_eq!(parts.bundle_id, id);
        assert_eq!(parts.oti, oti);
        assert_eq!(
            parts.packet,
            packet.as_slice(),
            "the integrity tag must be stripped back off the symbol"
        );
    }

    #[test]
    fn authentic_data_tag_verifies_and_bitflip_is_rejected() {
        let subkey = data_subkey(&Key::generate());
        let id = Uuid::new_v4();
        let oti = [7u8; OTI_LEN];
        let packet = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let dgram = build_data_frame(id, &oti, &packet, &subkey);
        assert!(
            verify_data_tag(&dgram, &subkey).is_ok(),
            "an untouched datagram must verify"
        );

        // Flip a bit inside the symbol region (the exact radio-corruption case).
        let mut corrupt = dgram.clone();
        let mid = HEADER_LEN + UUID_LEN + OTI_LEN + 2;
        corrupt[mid] ^= 0x01;
        assert!(
            matches!(
                verify_data_tag(&corrupt, &subkey),
                Err(CoreError::MalformedFrame)
            ),
            "a single flipped symbol bit must fail the integrity tag"
        );

        // Flip a bit inside the tag itself — also rejected.
        let mut tag_corrupt = dgram.clone();
        let last = tag_corrupt.len() - 1;
        tag_corrupt[last] ^= 0x80;
        assert!(matches!(
            verify_data_tag(&tag_corrupt, &subkey),
            Err(CoreError::MalformedFrame)
        ));
    }

    #[test]
    fn data_tag_from_wrong_psk_is_rejected() {
        let sender_subkey = data_subkey(&Key::generate());
        let receiver_subkey = data_subkey(&Key::generate());
        let dgram = build_data_frame(
            Uuid::new_v4(),
            &[7u8; OTI_LEN],
            &[1, 2, 3, 4, 5],
            &sender_subkey,
        );
        assert!(
            matches!(
                verify_data_tag(&dgram, &receiver_subkey),
                Err(CoreError::MalformedFrame)
            ),
            "a datagram tagged under a different PSK must not verify"
        );
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert!(matches!(parse_frame(&[]), Err(CoreError::MalformedFrame)));
        assert!(matches!(
            parse_frame(&[WIRE_VERSION]),
            Err(CoreError::MalformedFrame)
        ));
        // Unknown frame type.
        assert!(matches!(
            parse_frame(&[WIRE_VERSION, 0x7F, 0, 0]),
            Err(CoreError::MalformedFrame)
        ));
        // Wrong version.
        assert!(matches!(
            parse_frame(&[0x02, FRAME_DATA, 0, 0]),
            Err(CoreError::MalformedFrame)
        ));
        // Truncated DATA body.
        assert!(matches!(
            parse_frame(&[WIRE_VERSION, FRAME_DATA, 1, 2, 3]),
            Err(CoreError::MalformedFrame)
        ));
    }
}
