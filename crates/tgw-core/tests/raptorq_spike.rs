//! H1–2 RaptorQ spike (tasks/muaz.md) — de-risks the FEC layer before anything depends on it.
//!
//! Proves, against `raptorq` 2.0.1 (RFC 6330), the three properties the transport design in
//! `docs/ARCHITECTURE.md` §1/§4 stands on:
//!
//! 1. A 20 KiB bundle encoded at symbol size 1100 survives a 30% random packet drop
//!    (worse than the 25% design point) and reconstructs byte-identical.
//! 2. The `ObjectTransmissionInformation` (OTI) round-trips through its 12-byte wire form,
//!    so DATA frames can carry it and the gateway can build a `Decoder` from the wire alone.
//! 3. Below the source-symbol count the decoder reports "not yet" (`None`) instead of
//!    producing wrong data — the signal the NACK/repair loop keys off.
//!
//! Everything is seeded, so a failure is reproducible, not a flaky roll of the dice.
//! Usage idioms validated against the crate's `examples/main.rs` at tag v2.0.1
//! (docs.rs coverage is thin — flagged in docs/RESEARCH_LOG.md).

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation};

/// Bundle size under test — the checklist's 20 KB, comfortably multi-symbol.
const PAYLOAD_LEN: usize = 20 * 1024;
/// Contract 2 symbol size: fits one UDP datagram under a 1500 MTU with headers to spare.
const SYMBOL_SIZE: u16 = 1100;
/// Repair symbols minted per source block. K = ceil(20480 / 1100) = 19 source symbols;
/// 19 + 20 packets with 30% dropped leaves ~27, safely above K plus RaptorQ's small
/// decode overhead. (The production default is `overhead_factor` 1.4; the spike stresses
/// a heavier 30% drop, hence the larger margin here.)
const REPAIR_PACKETS: u32 = 20;
/// Fraction of packets the lossy "link" eats.
const DROP_PERCENT: usize = 30;
/// Fixed seed — reruns are bit-identical (CI determinism, per docs/ARCHITECTURE.md §8).
const RNG_SEED: u64 = 0x2026_0711;

/// Deterministic pseudo-random payload standing in for an encrypted bundle.
fn seeded_payload(rng: &mut StdRng) -> Vec<u8> {
    let mut payload = vec![0u8; PAYLOAD_LEN];
    rng.fill(payload.as_mut_slice());
    payload
}

/// Encode `payload`, then shuffle and drop `DROP_PERCENT`% of the serialized packets,
/// simulating random loss on the UDP link. Returns `(survivors, total_sent)`.
fn encode_and_drop(encoder: &Encoder, rng: &mut StdRng) -> (Vec<Vec<u8>>, usize) {
    let mut wire: Vec<Vec<u8>> = encoder
        .get_encoded_packets(REPAIR_PACKETS)
        .iter()
        .map(EncodingPacket::serialize)
        .collect();
    let sent = wire.len();
    wire.shuffle(rng);
    wire.truncate(sent - (sent * DROP_PERCENT) / 100);
    (wire, sent)
}

/// Feed surviving packets to `decoder` until it reports completion (or we run out).
fn drain_into(decoder: &mut Decoder, wire: &[Vec<u8>]) -> Option<Vec<u8>> {
    for packet_bytes in wire {
        if let Some(decoded) = decoder.decode(EncodingPacket::deserialize(packet_bytes)) {
            return Some(decoded);
        }
    }
    None
}

/// The headline spike: 20 KiB → symbols → lose 30% → byte-identical reconstruction.
#[test]
fn reconstructs_20kib_after_30_percent_random_loss() {
    let mut rng = StdRng::seed_from_u64(RNG_SEED);
    let payload = seeded_payload(&mut rng);

    let encoder = Encoder::with_defaults(&payload, SYMBOL_SIZE);
    let (survivors, sent) = encode_and_drop(&encoder, &mut rng);

    // Sanity: every serialized packet is one symbol plus the 4-byte PayloadId, so a
    // DATA frame's FEC portion has a known, bounded wire size for the framing layer.
    let symbol_size = usize::from(encoder.get_config().symbol_size());
    for packet_bytes in &survivors {
        assert_eq!(
            packet_bytes.len(),
            symbol_size + 4,
            "serialized EncodingPacket must be symbol_size + 4-byte PayloadId"
        );
    }

    let mut decoder = Decoder::new(encoder.get_config());
    let decoded = drain_into(&mut decoder, &survivors);

    assert_eq!(
        decoded.as_deref(),
        Some(payload.as_slice()),
        "20 KiB payload must reconstruct byte-identical from {} of {sent} packets",
        survivors.len(),
    );
}

/// The OTI must survive its 12-byte serialized form: the gateway will only ever see the
/// wire, so a decoder built from a *deserialized* OTI must work as well as one built from
/// the encoder's own config. This is what lets DATA frames carry the OTI (Contract 2).
#[test]
fn decoder_built_from_wire_format_oti_reconstructs() {
    let mut rng = StdRng::seed_from_u64(RNG_SEED.wrapping_add(1));
    let payload = seeded_payload(&mut rng);

    let encoder = Encoder::with_defaults(&payload, SYMBOL_SIZE);
    let oti_wire: [u8; 12] = encoder.get_config().serialize();
    let received_oti = ObjectTransmissionInformation::deserialize(&oti_wire);

    assert_eq!(
        received_oti.transfer_length(),
        PAYLOAD_LEN as u64,
        "transfer length must survive the OTI round-trip"
    );
    assert_eq!(
        received_oti.symbol_size(),
        encoder.get_config().symbol_size(),
        "symbol size must survive the OTI round-trip"
    );

    let (survivors, _sent) = encode_and_drop(&encoder, &mut rng);
    let mut decoder = Decoder::new(received_oti);
    let decoded = drain_into(&mut decoder, &survivors);

    assert_eq!(
        decoded.as_deref(),
        Some(payload.as_slice()),
        "decoder constructed from deserialized OTI must reconstruct the payload"
    );
}

/// Starve the decoder below K source symbols: it must keep answering `None` (need more)
/// rather than fabricating output. The gateway's NACK timer is armed by exactly this state.
#[test]
fn decode_stays_incomplete_below_source_symbol_count() {
    let mut rng = StdRng::seed_from_u64(RNG_SEED.wrapping_add(2));
    let payload = seeded_payload(&mut rng);

    let encoder = Encoder::with_defaults(&payload, SYMBOL_SIZE);
    let mut wire: Vec<Vec<u8>> = encoder
        .get_encoded_packets(REPAIR_PACKETS)
        .iter()
        .map(EncodingPacket::serialize)
        .collect();

    // 10 packets * 1100 B < 20 KiB: reconstruction is information-theoretically impossible.
    wire.shuffle(&mut rng);
    wire.truncate(10);

    let mut decoder = Decoder::new(encoder.get_config());
    let decoded = drain_into(&mut decoder, &wire);

    assert!(
        decoded.is_none(),
        "decoder must report NeedMore (None) when fed fewer symbols than the payload requires"
    );
    assert!(
        decoder.get_result().is_none(),
        "get_result must agree that the decode is incomplete"
    );
}
