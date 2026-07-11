//! Password-authenticated key agreement (SPAKE2) for keyless pairing.
//!
//! A short human pairing code drives SPAKE2 (balanced PAKE) so the field and hospital derive
//! the SAME fresh 256-bit session key without ever transmitting it. A wrong code yields
//! DIFFERENT secrets on each side — never an error — so key-confirmation MACs (`*_confirm`) are
//! mandatory before any PHI moves. The derived session key feeds the existing
//! envelope/receipt/integrity crypto unchanged.
//!
//! The module also carries the pairing wire frames and a stateless anti-spoof cookie used by
//! the gateway's public-port responder (see `tgw-gateway::pairing`).

use std::net::SocketAddr;

use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::CoreError;
use crate::key::Key;
use crate::mac::{ct_eq, hkdf_sha256, hmac_sha256};

/// Fixed protocol identities so both ends agree on the asymmetric SPAKE2 roles.
const ID_FIELD: &[u8] = b"tgw-field";
const ID_HOSPITAL: &[u8] = b"tgw-hospital";
/// Non-secret salt binding the pairing derivations to this protocol's key schedule.
const PAIR_SALT: &[u8] = b"tgw/pair-schedule/v1";
/// HKDF info labels — domain-separated so the session key and confirm key never collide.
const SESSION_LABEL: &[u8] = b"tgw/pair-session/v1";
const CONFIRM_LABEL: &[u8] = b"tgw/pair-confirm/v1";

/// Field-side (initiator "A") pairing state, holding our outbound message for the transcript.
pub struct PairInitiator {
    inner: Spake2<Ed25519Group>,
    msg_a: Vec<u8>,
}

/// Hospital-side (responder "B") pairing state, holding our outbound message for the transcript.
pub struct PairResponder {
    inner: Spake2<Ed25519Group>,
    msg_b: Vec<u8>,
}

/// A completed pairing: the derived session key plus the confirmation-MAC material.
pub struct PairSession {
    session_key: Key,
    confirm_key: [u8; 32],
    msg_a: Vec<u8>,
    msg_b: Vec<u8>,
}

/// Begin pairing as the field (initiator). Returns the state and `msg_a` to transmit.
#[must_use]
pub fn start_initiator(code: &str) -> (PairInitiator, Vec<u8>) {
    let (inner, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_FIELD),
        &Identity::new(ID_HOSPITAL),
    );
    (
        PairInitiator {
            inner,
            msg_a: msg_a.clone(),
        },
        msg_a,
    )
}

/// Begin pairing as the hospital (responder). Returns the state and `msg_b` to transmit.
#[must_use]
pub fn start_responder(code: &str) -> (PairResponder, Vec<u8>) {
    let (inner, msg_b) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_bytes()),
        &Identity::new(ID_FIELD),
        &Identity::new(ID_HOSPITAL),
    );
    (
        PairResponder {
            inner,
            msg_b: msg_b.clone(),
        },
        msg_b,
    )
}

fn derive(secret: &[u8], msg_a: Vec<u8>, msg_b: Vec<u8>) -> PairSession {
    let session_bytes = hkdf_sha256(PAIR_SALT, secret, SESSION_LABEL);
    let confirm_key = hkdf_sha256(PAIR_SALT, secret, CONFIRM_LABEL);
    PairSession {
        session_key: Key::from_bytes(session_bytes),
        confirm_key,
        msg_a,
        msg_b,
    }
}

impl PairInitiator {
    /// Complete pairing given the responder's `msg_b`.
    pub fn finish(self, peer_msg_b: &[u8]) -> Result<PairSession, CoreError> {
        let secret = self
            .inner
            .finish(peer_msg_b)
            .map_err(|_| CoreError::Crypto)?;
        Ok(derive(secret.as_ref(), self.msg_a, peer_msg_b.to_vec()))
    }
}

impl PairResponder {
    /// Complete pairing given the initiator's `msg_a`.
    pub fn finish(self, peer_msg_a: &[u8]) -> Result<PairSession, CoreError> {
        let secret = self
            .inner
            .finish(peer_msg_a)
            .map_err(|_| CoreError::Crypto)?;
        Ok(derive(secret.as_ref(), peer_msg_a.to_vec(), self.msg_b))
    }
}

impl PairSession {
    /// The derived per-session key — feeds the existing envelope/receipt/integrity crypto.
    #[must_use]
    pub fn session_key(&self) -> &Key {
        &self.session_key
    }

    /// Consume the session, yielding the derived key (after confirmations have been checked).
    #[must_use]
    pub fn into_key(self) -> Key {
        self.session_key
    }

    fn transcript_tag(&self, who: u8) -> [u8; 32] {
        let mut msg = Vec::with_capacity(self.msg_a.len() + self.msg_b.len() + 1);
        msg.extend_from_slice(&self.msg_a);
        msg.extend_from_slice(&self.msg_b);
        msg.push(who);
        hmac_sha256(&self.confirm_key, &msg)
    }

    /// The confirmation the hospital sends to the field (proves it derived the same key).
    #[must_use]
    pub fn responder_confirm(&self) -> [u8; 32] {
        self.transcript_tag(b'B')
    }

    /// The confirmation the field sends to the hospital.
    #[must_use]
    pub fn initiator_confirm(&self) -> [u8; 32] {
        self.transcript_tag(b'A')
    }

    /// Constant-time check of the hospital's confirmation (call before accepting the key).
    #[must_use]
    pub fn verify_responder_confirm(&self, tag: &[u8]) -> bool {
        ct_eq(&self.responder_confirm(), tag)
    }

    /// Constant-time check of the field's confirmation.
    #[must_use]
    pub fn verify_initiator_confirm(&self, tag: &[u8]) -> bool {
        ct_eq(&self.initiator_confirm(), tag)
    }
}

/// Magic identifying a pairing datagram (`TGW Pair`). Distinct from DATA/relay/announce.
const PAIR_MAGIC: &[u8; 4] = b"TGWP";
/// Pairing frame format version.
const PAIR_VERSION: u8 = 1;
const T_INIT: u8 = 1;
const T_RESP: u8 = 2;
const T_CONFIRM: u8 = 3;
/// Bound so a malformed datagram can never allocate unreasonably (SPAKE2 msgs are 33 B).
const MAX_FIELD: usize = 128;

/// A pairing-handshake datagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairFrame {
    /// field → hospital: (optional) cookie echo + SPAKE2 message A.
    Init {
        /// Return-routability cookie echoed back from a prior challenge (empty on first try).
        cookie: Vec<u8>,
        /// SPAKE2 message A.
        msg: Vec<u8>,
    },
    /// hospital → field: cookie + SPAKE2 message B + responder confirmation.
    ///
    /// A cookie-only challenge carries an empty `msg` and `confirm`.
    Resp {
        /// The cookie the field must echo (challenge) or has echoed (accepted).
        cookie: Vec<u8>,
        /// SPAKE2 message B (empty for a bare cookie challenge).
        msg: Vec<u8>,
        /// Responder key-confirmation MAC (empty for a bare cookie challenge).
        confirm: Vec<u8>,
    },
    /// field → hospital: initiator confirmation.
    Confirm {
        /// Initiator key-confirmation MAC.
        confirm: Vec<u8>,
    },
}

fn put_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u8::try_from(bytes.len()).unwrap_or(u8::MAX);
    out.push(len);
    out.extend_from_slice(&bytes[..usize::from(len)]);
}

fn take_field(bytes: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    let len = usize::from(*bytes.get(*cursor)?);
    if len > MAX_FIELD {
        return None;
    }
    let start = cursor.checked_add(1)?;
    let end = start.checked_add(len)?;
    let field = bytes.get(start..end)?.to_vec();
    *cursor = end;
    Some(field)
}

/// Encode a pairing frame: `MAGIC | version | type | length-prefixed fields`.
#[must_use]
pub fn encode_pair(frame: &PairFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(PAIR_MAGIC);
    out.push(PAIR_VERSION);
    match frame {
        PairFrame::Init { cookie, msg } => {
            out.push(T_INIT);
            put_field(&mut out, cookie);
            put_field(&mut out, msg);
        }
        PairFrame::Resp {
            cookie,
            msg,
            confirm,
        } => {
            out.push(T_RESP);
            put_field(&mut out, cookie);
            put_field(&mut out, msg);
            put_field(&mut out, confirm);
        }
        PairFrame::Confirm { confirm } => {
            out.push(T_CONFIRM);
            put_field(&mut out, confirm);
        }
    }
    out
}

/// Decode a pairing frame, returning `None` for anything malformed (a stray datagram on the
/// public port is ignored, never acted on).
#[must_use]
pub fn decode_pair(bytes: &[u8]) -> Option<PairFrame> {
    if bytes.len() < 6 || &bytes[0..4] != PAIR_MAGIC || bytes[4] != PAIR_VERSION {
        return None;
    }
    let mut cursor = 6;
    let frame = match bytes[5] {
        T_INIT => PairFrame::Init {
            cookie: take_field(bytes, &mut cursor)?,
            msg: take_field(bytes, &mut cursor)?,
        },
        T_RESP => PairFrame::Resp {
            cookie: take_field(bytes, &mut cursor)?,
            msg: take_field(bytes, &mut cursor)?,
            confirm: take_field(bytes, &mut cursor)?,
        },
        T_CONFIRM => PairFrame::Confirm {
            confirm: take_field(bytes, &mut cursor)?,
        },
        _ => return None,
    };
    // Reject trailing garbage so framing stays exact.
    if cursor != bytes.len() {
        return None;
    }
    Some(frame)
}

/// A per-process secret keying stateless anti-spoof cookies (return-routability tokens).
pub struct CookieKey([u8; 32]);

impl CookieKey {
    /// Fresh random cookie key from the OS CSPRNG.
    #[must_use]
    pub fn random() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        CookieKey(bytes)
    }

    /// Mint a 16-byte cookie binding `src` and `epoch`.
    #[must_use]
    pub fn mint(&self, src: &SocketAddr, epoch: u64) -> [u8; 16] {
        let mut msg = src.to_string().into_bytes();
        msg.extend_from_slice(&epoch.to_be_bytes());
        let full = hmac_sha256(&self.0, &msg);
        let mut cookie = [0u8; 16];
        cookie.copy_from_slice(&full[..16]);
        cookie
    }

    /// Verify a cookie for `src`, accepting the current or immediately-previous epoch (so a
    /// cookie minted just before an epoch tick still works). Constant-time comparison.
    #[must_use]
    pub fn verify(&self, src: &SocketAddr, now_epoch: u64, cookie: &[u8]) -> bool {
        ct_eq(&self.mint(src, now_epoch), cookie)
            || ct_eq(&self.mint(src, now_epoch.saturating_sub(1)), cookie)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_code_derives_equal_session_key_and_confirmations() {
        let (initiator, msg_a) = start_initiator("4-otter-cobalt");
        let (responder, msg_b) = start_responder("4-otter-cobalt");

        let field = initiator.finish(&msg_b).expect("initiator finishes");
        let hospital = responder.finish(&msg_a).expect("responder finishes");

        // Same session key both sides (compare via hex — Key is otherwise opaque).
        assert_eq!(
            field.session_key().to_hex(),
            hospital.session_key().to_hex()
        );
        // Confirmations cross-verify.
        assert!(field.verify_responder_confirm(&hospital.responder_confirm()));
        assert!(hospital.verify_initiator_confirm(&field.initiator_confirm()));
    }

    #[test]
    fn wrong_code_fails_key_confirmation() {
        let (initiator, msg_a) = start_initiator("right-code");
        let (responder, msg_b) = start_responder("WRONG-code");
        let field = initiator
            .finish(&msg_b)
            .expect("finishes (no error on wrong code)");
        let hospital = responder.finish(&msg_a).expect("finishes");
        // The security property: mismatched code ⇒ confirmation rejects, so no PHI moves.
        assert!(!field.verify_responder_confirm(&hospital.responder_confirm()));
        assert!(!hospital.verify_initiator_confirm(&field.initiator_confirm()));
    }

    #[test]
    fn pair_frames_round_trip() {
        let init = PairFrame::Init {
            cookie: vec![],
            msg: vec![1, 2, 3],
        };
        assert_eq!(decode_pair(&encode_pair(&init)), Some(init));
        let resp = PairFrame::Resp {
            cookie: vec![9; 16],
            msg: vec![7; 33],
            confirm: vec![5; 32],
        };
        assert_eq!(decode_pair(&encode_pair(&resp)), Some(resp));
        let conf = PairFrame::Confirm {
            confirm: vec![4; 32],
        };
        assert_eq!(decode_pair(&encode_pair(&conf)), Some(conf));
    }

    #[test]
    fn decode_pair_rejects_malformed() {
        assert!(decode_pair(b"").is_none());
        assert!(decode_pair(b"XXXX\x01\x01").is_none(), "wrong magic");
        let mut good = encode_pair(&PairFrame::Confirm {
            confirm: vec![1; 32],
        });
        good[4] = 0x02; // bad version
        assert!(decode_pair(&good).is_none());
        // A length field that runs off the end must not panic.
        let mut truncated = encode_pair(&PairFrame::Resp {
            cookie: vec![1; 16],
            msg: vec![2; 33],
            confirm: vec![3; 32],
        });
        truncated.truncate(10);
        assert!(decode_pair(&truncated).is_none());
    }

    #[test]
    fn cookie_binds_to_source_and_epoch() {
        let key = CookieKey::random();
        let a: SocketAddr = "203.0.113.5:5000".parse().expect("addr");
        let b: SocketAddr = "203.0.113.6:5000".parse().expect("addr");
        let cookie = key.mint(&a, 100);
        assert!(key.verify(&a, 100, &cookie), "same addr+epoch verifies");
        assert!(key.verify(&a, 101, &cookie), "previous epoch still accepted");
        assert!(!key.verify(&b, 100, &cookie), "different source rejected");
        assert!(!key.verify(&a, 102, &cookie), "too-old epoch rejected");
        assert!(!key.verify(&a, 100, &[0u8; 16]), "forged cookie rejected");
    }
}
