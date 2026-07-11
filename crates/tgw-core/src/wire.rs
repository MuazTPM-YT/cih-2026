//! Wire protocol v1 (Contract 2, tasks/CONTRACTS.md).
//!
//! Every datagram: `[1 B version | 1 B frame type | body]`.
//! `0x01 DATA` (field → gw), `0x02 NACK` (gw → field), `0x03 RECEIPT` (gw → field).
//! Frame-type byte layout is the frozen contract; body internals are core-private.
//!
//! DATA body: `uuid (16) | OTI (12) | RaptorQ EncodingPacket (4 B PayloadId + symbol)`.
//! The 12-byte OTI rides in **every** DATA frame (≈1% overhead at symbol size 1100) so
//! the receiver can initialize its decoder from whichever symbol arrives first — with
//! 25% loss there is no "reliable first packet" to pin it to.
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

/// Build one DATA datagram: header | uuid | OTI | serialized EncodingPacket.
pub(crate) fn build_data_frame(bundle_id: Uuid, oti: &[u8; OTI_LEN], packet: &[u8]) -> Datagram {
    let mut dgram = Vec::with_capacity(HEADER_LEN + UUID_LEN + OTI_LEN + packet.len());
    dgram.push(WIRE_VERSION);
    dgram.push(FRAME_DATA);
    dgram.extend_from_slice(bundle_id.as_bytes());
    dgram.extend_from_slice(oti);
    dgram.extend_from_slice(packet);
    dgram
}

/// Parse a DATA frame body (everything after the 2-byte header).
pub(crate) fn parse_data_body(body: &[u8]) -> Result<DataParts<'_>, CoreError> {
    // Minimum: uuid + OTI + PayloadId(4) + at least one symbol byte.
    if body.len() < UUID_LEN + OTI_LEN + 5 {
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
        packet: &body[UUID_LEN + OTI_LEN..],
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
        let id = Uuid::new_v4();
        let oti = [7u8; OTI_LEN];
        let packet = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let dgram = build_data_frame(id, &oti, &packet);

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
        assert_eq!(parts.packet, packet.as_slice());
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
