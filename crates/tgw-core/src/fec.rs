//! RaptorQ symbol framing (docs/ARCHITECTURE.md §1/§4) — the layer that survives loss.
//!
//! Sender: sealed envelope → RaptorQ symbols → DATA datagrams, bursting source + repair
//! at `overhead_factor`, and minting **fresh** repair symbols (never re-sent ESIs) for
//! NACK responses and timeout re-bursts — a fountain code can do this indefinitely.
//!
//! Receiver: per-bundle state machine ([`BundleReceiver`]) that absorbs whichever
//! datagrams happen to arrive, reports progress for the NACK timer and the dashboard,
//! and only surfaces a bundle after the AEAD envelope authenticates.
//!
//! Usage idioms for `raptorq` 2.0.1 were validated in the H1–2 spike
//! (`tests/raptorq_spike.rs`); the FEC codec stays swappable behind
//! `encode_bundle`/`BundleReceiver` per the slip rules in docs/BUILD_PLAN.md.

use std::collections::HashSet;

use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation, PayloadId};
use uuid::Uuid;

use crate::config::FecConfig;
use crate::envelope::{open_envelope, seal_bundle};
use crate::error::CoreError;
use crate::key::Key;
use crate::model::{Bundle, Datagram};
use crate::wire::{self, NackFrame, OTI_LEN};

/// Result of feeding one datagram to a [`BundleReceiver`].
#[derive(Debug)]
pub enum Absorb {
    /// Not enough symbols yet; keep absorbing.
    NeedMore,
    /// Bundle fully decoded and authenticated. Returned again (idempotently) for any
    /// datagram of an already-completed bundle, so duplicate arrivals still trigger a
    /// receipt.
    Complete(Bundle),
    /// Reserved in the `Absorb` contract for decode-stall signaling. `absorb` itself is
    /// packet-driven and cannot observe time, so stalls are detected by the gateway's
    /// NACK timer calling [`BundleReceiver::build_nack`]; this variant is currently
    /// never constructed by `absorb`.
    Nack(NackFrame),
}

/// Extra repair symbols requested beyond the arithmetic shortfall, absorbing RaptorQ's
/// small (~2-symbol) decode overhead and further in-flight loss.
const NACK_MARGIN: u32 = 2;

/// Encode a bundle into UDP `DATA` datagrams at the configured overhead (field → gw).
///
/// One-shot form of [`BundleSender`] for callers that don't run a repair loop.
pub fn encode_bundle(b: &Bundle, key: &Key, cfg: &FecConfig) -> Result<Vec<Datagram>, CoreError> {
    Ok(BundleSender::new(b, key, cfg)?.initial_burst())
}

/// Sending side of one bundle: seals, encodes, and mints repair symbols on demand.
///
/// Keep it alive for the bundle's whole delivery attempt — it tracks which repair ESIs
/// have been used so every repair burst carries new information.
pub struct BundleSender {
    bundle_id: Uuid,
    encoder: Encoder,
    oti_bytes: [u8; OTI_LEN],
    /// Per source block: how many repair symbols have been minted so far.
    repair_cursor: Vec<u32>,
    /// Per source block: source symbol count (for burst sizing).
    source_symbols: Vec<u32>,
    overhead_factor: f32,
}

impl BundleSender {
    /// Seal `bundle` under `key` and prepare the RaptorQ encoder.
    pub fn new(bundle: &Bundle, key: &Key, cfg: &FecConfig) -> Result<Self, CoreError> {
        let envelope = seal_bundle(bundle, key)?;
        Self::from_envelope(bundle.id, &envelope, cfg)
    }

    /// Prepare a sender from an **already-sealed** envelope (the store-and-forward
    /// resume path: the redb queue holds sealed envelopes at rest, and a re-burst after
    /// a crash must not re-encrypt).
    pub fn from_envelope(
        bundle_id: Uuid,
        envelope: &[u8],
        cfg: &FecConfig,
    ) -> Result<Self, CoreError> {
        if cfg.symbol_size < 64 {
            return Err(CoreError::Config("symbol_size must be ≥ 64 bytes".into()));
        }
        // NaN also fails this comparison, which is exactly what we want.
        if cfg.overhead_factor < 1.0 || cfg.overhead_factor.is_nan() {
            return Err(CoreError::Config("overhead_factor must be ≥ 1.0".into()));
        }
        if envelope.is_empty() {
            return Err(CoreError::Encode("empty envelope".into()));
        }
        let oti =
            ObjectTransmissionInformation::with_defaults(envelope.len() as u64, cfg.symbol_size);
        let encoder = Encoder::new(envelope, oti);
        let source_symbols: Vec<u32> = encoder
            .get_block_encoders()
            .iter()
            .map(|_| 0u32) // placeholder, replaced below from actual source packets
            .collect();
        let mut sender = BundleSender {
            bundle_id,
            oti_bytes: oti.serialize(),
            repair_cursor: vec![0; source_symbols.len()],
            source_symbols,
            encoder,
            overhead_factor: cfg.overhead_factor,
        };
        // Source symbol counts per block, derived from actual source packets — exact,
        // no re-implementation of the RFC 6330 partition function on the send side.
        for (i, block) in sender.encoder.get_block_encoders().iter().enumerate() {
            sender.source_symbols[i] = block.source_packets().len() as u32;
        }
        Ok(sender)
    }

    /// The bundle this sender is delivering.
    #[must_use]
    pub fn bundle_id(&self) -> Uuid {
        self.bundle_id
    }

    /// Total source symbols across blocks (progress denominators, logging).
    #[must_use]
    pub fn total_source_symbols(&self) -> u32 {
        self.source_symbols.iter().sum()
    }

    /// First burst: every source symbol plus `ceil(K · (overhead_factor − 1))` fresh
    /// repair symbols per block (minimum 2, so even tiny bundles tolerate loss).
    pub fn initial_burst(&mut self) -> Vec<Datagram> {
        let mut datagrams = Vec::new();
        let block_count = self.source_symbols.len();
        for block_index in 0..block_count {
            let k = self.source_symbols[block_index];
            let repair = (((k as f32) * (self.overhead_factor - 1.0)).ceil() as u32).max(2);
            let block = &self.encoder.get_block_encoders()[block_index];
            for packet in block.source_packets() {
                datagrams.push(self.frame(&packet));
            }
            let start = self.repair_cursor[block_index];
            for packet in block.repair_packets(start, repair) {
                datagrams.push(self.frame(&packet));
            }
            self.repair_cursor[block_index] = start + repair;
        }
        datagrams
    }

    /// Mint fresh repair symbols answering a gateway NACK. Ignores NACKs for other
    /// bundles (returns empty) so a confused peer can't make us burn bandwidth.
    pub fn respond_to_nack(&mut self, nack: &NackFrame) -> Vec<Datagram> {
        if nack.bundle_id != self.bundle_id {
            return Vec::new();
        }
        let mut datagrams = Vec::new();
        for (block_index, &needed) in nack.needed.iter().enumerate() {
            if block_index >= self.repair_cursor.len() || needed == 0 {
                continue;
            }
            let count = needed + NACK_MARGIN;
            let start = self.repair_cursor[block_index];
            let block = &self.encoder.get_block_encoders()[block_index];
            for packet in block.repair_packets(start, count) {
                datagrams.push(self.frame(&packet));
            }
            self.repair_cursor[block_index] = start + count;
        }
        datagrams
    }

    /// Mint a fresh repair-only re-burst for the silence/timeout path: `fraction` of
    /// each block's source count (e.g. `0.5` ⇒ half a window of new symbols). Fountain
    /// property: repair-only bursts still complete a decode from zero if repeated.
    pub fn repair_burst(&mut self, fraction: f32) -> Vec<Datagram> {
        let mut datagrams = Vec::new();
        let block_count = self.source_symbols.len();
        for block_index in 0..block_count {
            let k = self.source_symbols[block_index];
            let count = (((k as f32) * fraction).ceil() as u32).max(2);
            let start = self.repair_cursor[block_index];
            let block = &self.encoder.get_block_encoders()[block_index];
            for packet in block.repair_packets(start, count) {
                datagrams.push(self.frame(&packet));
            }
            self.repair_cursor[block_index] = start + count;
        }
        datagrams
    }

    fn frame(&self, packet: &EncodingPacket) -> Datagram {
        wire::build_data_frame(self.bundle_id, &self.oti_bytes, &packet.serialize())
    }
}

/// Reassembles one in-flight bundle from incoming symbols. One instance per `Uuid`
/// (Contract 1) — the gateway keys a map of these by bundle id.
pub struct BundleReceiver {
    key: Key,
    state: State,
}

enum State {
    /// No DATA frame seen yet.
    Idle,
    /// Decoding in progress.
    Active(Box<Active>),
    /// Decoded and authenticated.
    Done(Box<Bundle>),
    /// Envelope failed authentication — poisoned, never yields data.
    Failed,
}

struct Active {
    bundle_id: Uuid,
    oti_bytes: [u8; OTI_LEN],
    decoder: Decoder,
    symbol_size: u16,
    /// Distinct `(block, encoding symbol id)` pairs seen — dedup'd progress counter.
    seen: HashSet<(u8, u32)>,
    /// Source symbols per block (RFC 6330 partitioning), for NACK arithmetic.
    source_symbols: Vec<u32>,
}

impl BundleReceiver {
    /// Create a receiver that will authenticate completed bundles with `key`.
    ///
    /// (H2 stub took no key; decryption made it a required argument — flagged to the
    /// team as a Contract 1 constructor change, `absorb`'s signature is unchanged.)
    #[must_use]
    pub fn new(key: Key) -> Self {
        BundleReceiver {
            key,
            state: State::Idle,
        }
    }

    /// Feed one datagram; drive the decode state machine.
    ///
    /// Errors: [`CoreError::MalformedFrame`] for non-DATA or corrupt frames (drop and
    /// carry on), [`CoreError::Crypto`] if the completed envelope fails authentication
    /// (drop the whole bundle — never partial-accept).
    pub fn absorb(&mut self, dgram: &[u8]) -> Result<Absorb, CoreError> {
        let parts = wire::parse_data_frame(dgram)?;
        match &mut self.state {
            State::Done(bundle) => {
                if bundle.id == parts.bundle_id {
                    Ok(Absorb::Complete((**bundle).clone()))
                } else {
                    Err(CoreError::MalformedFrame)
                }
            }
            State::Failed => Err(CoreError::Crypto),
            State::Idle => {
                let active = Active::from_first_frame(&parts)?;
                self.state = State::Active(Box::new(active));
                self.absorb_into_active(&parts)
            }
            State::Active(_) => self.absorb_into_active(&parts),
        }
    }

    fn absorb_into_active(&mut self, parts: &wire::DataParts<'_>) -> Result<Absorb, CoreError> {
        let State::Active(active) = &mut self.state else {
            return Err(CoreError::MalformedFrame);
        };
        if parts.bundle_id != active.bundle_id || parts.oti != active.oti_bytes {
            // Misrouted or inconsistent symbol: reject the datagram, keep our state.
            return Err(CoreError::MalformedFrame);
        }
        // Guard EncodingPacket::deserialize (it assumes well-formed input): a DATA
        // frame's FEC portion is exactly 4 B PayloadId + one full symbol.
        if parts.packet.len() != 4 + usize::from(active.symbol_size) {
            return Err(CoreError::MalformedFrame);
        }
        let packet = EncodingPacket::deserialize(parts.packet);
        let id: &PayloadId = packet.payload_id();
        active
            .seen
            .insert((id.source_block_number(), id.encoding_symbol_id()));

        match active.decoder.decode(packet) {
            Some(envelope) => match open_envelope(active.bundle_id, &envelope, &self.key) {
                Ok(bundle) => {
                    self.state = State::Done(Box::new(bundle.clone()));
                    Ok(Absorb::Complete(bundle))
                }
                Err(_) => {
                    self.state = State::Failed;
                    Err(CoreError::Crypto)
                }
            },
            None => Ok(Absorb::NeedMore),
        }
    }

    /// The bundle id this receiver is assembling (once known).
    #[must_use]
    pub fn bundle_id(&self) -> Option<Uuid> {
        match &self.state {
            State::Idle | State::Failed => None,
            State::Active(a) => Some(a.bundle_id),
            State::Done(b) => Some(b.id),
        }
    }

    /// Whether the bundle has fully decoded and authenticated.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self.state, State::Done(_))
    }

    /// Distinct symbols absorbed so far (dashboard: `symbols_received`).
    #[must_use]
    pub fn symbols_received(&self) -> u32 {
        match &self.state {
            State::Active(a) => a.seen.len() as u32,
            _ => 0,
        }
    }

    /// Source-symbol total the decode needs, once the OTI is known
    /// (dashboard: `symbols_needed`).
    #[must_use]
    pub fn symbols_needed(&self) -> Option<u32> {
        match &self.state {
            State::Active(a) => Some(a.source_symbols.iter().sum()),
            _ => None,
        }
    }

    /// Repair request for the gateway's stall timer: per-block shortfall plus a small
    /// margin. `None` when nothing is in flight or the decode already finished.
    #[must_use]
    pub fn build_nack(&self) -> Option<NackFrame> {
        let State::Active(active) = &self.state else {
            return None;
        };
        let mut received_per_block = vec![0u32; active.source_symbols.len()];
        for (block, _esi) in &active.seen {
            if let Some(count) = received_per_block.get_mut(usize::from(*block)) {
                *count += 1;
            }
        }
        let needed: Vec<u32> = active
            .source_symbols
            .iter()
            .zip(&received_per_block)
            .map(|(&k, &got)| k.saturating_sub(got) + NACK_MARGIN)
            .collect();
        Some(NackFrame {
            bundle_id: active.bundle_id,
            needed,
        })
    }
}

impl Active {
    fn from_first_frame(parts: &wire::DataParts<'_>) -> Result<Self, CoreError> {
        let oti = ObjectTransmissionInformation::deserialize(&parts.oti);
        if oti.transfer_length() == 0 || oti.symbol_size() == 0 {
            return Err(CoreError::MalformedFrame);
        }
        let source_symbols = partition_source_symbols(
            oti.transfer_length(),
            oti.symbol_size(),
            oti.source_blocks(),
        );
        Ok(Active {
            bundle_id: parts.bundle_id,
            oti_bytes: parts.oti,
            decoder: Decoder::new(oti),
            symbol_size: oti.symbol_size(),
            seen: HashSet::new(),
            source_symbols,
        })
    }
}

/// RFC 6330 §4.4.1.2 partitioning: split `Kt = ceil(len / T)` source symbols across `Z`
/// blocks — `Z_L` long blocks of `K_L = ceil(Kt/Z)` and the rest short at `floor(Kt/Z)`.
fn partition_source_symbols(transfer_length: u64, symbol_size: u16, blocks: u8) -> Vec<u32> {
    let z = u64::from(blocks.max(1));
    let kt = transfer_length.div_ceil(u64::from(symbol_size.max(1)));
    let k_long = kt.div_ceil(z);
    let k_short = kt / z;
    let long_blocks = kt - k_short * z;
    (0..z)
        .map(|i| {
            if i < long_blocks {
                k_long as u32
            } else {
                k_short as u32
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BundlePayload, Priority};
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};

    const SEED: u64 = 0x2026_0711_0002;

    fn fec() -> FecConfig {
        FecConfig {
            symbol_size: 1100,
            overhead_factor: 1.4,
        }
    }

    fn test_bundle(len: usize, rng: &mut StdRng) -> Bundle {
        let mut data = vec![0u8; len];
        rng.fill(data.as_mut_slice());
        Bundle {
            id: Uuid::new_v4(),
            priority: Priority::Image,
            payload: BundlePayload::Image {
                mime: "image/jpeg".into(),
                data,
                patient_id: "P-TEST".into(),
            },
        }
    }

    fn absorb_all(receiver: &mut BundleReceiver, datagrams: &[Datagram]) -> Option<Bundle> {
        for dgram in datagrams {
            match receiver.absorb(dgram) {
                Ok(Absorb::Complete(bundle)) => return Some(bundle),
                Ok(_) => {}
                Err(e) => panic!("absorb must not error on authentic datagrams: {e}"),
            }
        }
        None
    }

    #[test]
    fn sender_resumes_from_stored_envelope() {
        // Crash-recovery path: seal once, persist, rebuild the sender from the sealed
        // envelope, and the receiver must still decode and authenticate.
        let mut rng = StdRng::seed_from_u64(SEED + 7);
        let key = Key::generate();
        let bundle = test_bundle(12_000, &mut rng);
        let envelope = match crate::envelope::seal_bundle(&bundle, &key) {
            Ok(e) => e,
            Err(e) => panic!("seal must succeed: {e}"),
        };

        let mut sender = match BundleSender::from_envelope(bundle.id, &envelope, &fec()) {
            Ok(s) => s,
            Err(e) => panic!("resume sender must build: {e}"),
        };
        let mut receiver = BundleReceiver::new(key);
        let decoded = absorb_all(&mut receiver, &sender.initial_burst());
        assert_eq!(decoded, Some(bundle));
    }

    #[test]
    fn clean_link_round_trip() {
        let mut rng = StdRng::seed_from_u64(SEED);
        let key = Key::generate();
        let bundle = test_bundle(25_000, &mut rng);

        let datagrams = match encode_bundle(&bundle, &key, &fec()) {
            Ok(d) => d,
            Err(e) => panic!("encode must succeed: {e}"),
        };
        // Datagram size bound: header(2) + uuid(16) + OTI(12) + PayloadId(4) + symbol.
        for dgram in &datagrams {
            assert!(dgram.len() <= 2 + 16 + 12 + 4 + 1100, "oversized datagram");
        }

        let mut receiver = BundleReceiver::new(key);
        let decoded = absorb_all(&mut receiver, &datagrams);
        assert_eq!(decoded, Some(bundle));
        assert!(receiver.is_complete());
    }

    #[test]
    fn thirty_percent_loss_then_nack_repair_round_trip() {
        let mut rng = StdRng::seed_from_u64(SEED + 1);
        let key = Key::generate();
        let bundle = test_bundle(25_000, &mut rng);

        let mut sender = match BundleSender::new(&bundle, &key, &fec()) {
            Ok(s) => s,
            Err(e) => panic!("sender must build: {e}"),
        };
        let burst = sender.initial_burst();

        // Drop 30% — deterministic RNG. At 1.4× overhead this can go either way per
        // seed; the NACK loop below must close whatever gap remains.
        let survivors: Vec<Datagram> = burst
            .into_iter()
            .filter(|_| rng.gen_range(0..100) >= 30)
            .collect();

        let mut receiver = BundleReceiver::new(key);
        let mut decoded = absorb_all(&mut receiver, &survivors);

        // NACK/repair rounds until complete — bounded so a regression fails loudly
        // instead of spinning forever.
        let mut rounds = 0;
        while decoded.is_none() {
            rounds += 1;
            assert!(rounds <= 8, "NACK loop failed to converge in 8 rounds");
            let nack = match receiver.build_nack() {
                Some(n) => n,
                None => panic!("incomplete receiver must produce a NACK"),
            };
            let repairs = sender.respond_to_nack(&nack);
            assert!(
                !repairs.is_empty(),
                "sender must answer a NACK with symbols"
            );
            // The repair burst crosses the same lossy link.
            let surviving_repairs: Vec<Datagram> = repairs
                .into_iter()
                .filter(|_| rng.gen_range(0..100) >= 30)
                .collect();
            decoded = absorb_all(&mut receiver, &surviving_repairs);
        }
        assert_eq!(decoded, Some(bundle));
    }

    #[test]
    fn repair_bursts_carry_fresh_symbols() {
        let mut rng = StdRng::seed_from_u64(SEED + 2);
        let key = Key::generate();
        let bundle = test_bundle(20_000, &mut rng);
        let mut sender = match BundleSender::new(&bundle, &key, &fec()) {
            Ok(s) => s,
            Err(e) => panic!("sender must build: {e}"),
        };
        let first = sender.initial_burst();
        let second = sender.repair_burst(0.5);
        let third = sender.respond_to_nack(&NackFrame {
            bundle_id: sender.bundle_id(),
            needed: vec![4],
        });

        let mut seen = HashSet::new();
        for dgram in first.iter().chain(&second).chain(&third) {
            let parts = match wire::parse_data_frame(dgram) {
                Ok(p) => p,
                Err(e) => panic!("own datagrams must parse: {e}"),
            };
            let packet = EncodingPacket::deserialize(parts.packet);
            let id = packet.payload_id();
            assert!(
                seen.insert((id.source_block_number(), id.encoding_symbol_id())),
                "duplicate ESI minted — repair symbols must always be fresh"
            );
        }
    }

    #[test]
    fn wrong_key_poisons_receiver_not_partial_data() {
        let mut rng = StdRng::seed_from_u64(SEED + 3);
        let bundle = test_bundle(8_000, &mut rng);
        let datagrams = match encode_bundle(&bundle, &Key::generate(), &fec()) {
            Ok(d) => d,
            Err(e) => panic!("encode must succeed: {e}"),
        };

        let mut receiver = BundleReceiver::new(Key::generate()); // different key
        let mut got_crypto_error = false;
        for dgram in &datagrams {
            match receiver.absorb(dgram) {
                Ok(Absorb::Complete(_)) => panic!("wrong key must never yield a bundle"),
                Ok(_) => {}
                Err(CoreError::Crypto) => {
                    got_crypto_error = true;
                    break;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(
            got_crypto_error,
            "completion under a wrong key must fail closed"
        );
        assert!(!receiver.is_complete());
    }

    #[test]
    fn duplicates_do_not_inflate_progress() {
        let mut rng = StdRng::seed_from_u64(SEED + 4);
        let key = Key::generate();
        let bundle = test_bundle(10_000, &mut rng);
        let datagrams = match encode_bundle(&bundle, &key, &fec()) {
            Ok(d) => d,
            Err(e) => panic!("encode must succeed: {e}"),
        };

        let mut receiver = BundleReceiver::new(key);
        match receiver.absorb(&datagrams[0]) {
            Ok(Absorb::NeedMore) => {}
            other => panic!("first symbol of a multi-symbol bundle: {other:?}"),
        }
        match receiver.absorb(&datagrams[0]) {
            Ok(Absorb::NeedMore) => {}
            other => panic!("duplicate absorb should be harmless: {other:?}"),
        }
        assert_eq!(
            receiver.symbols_received(),
            1,
            "duplicates must not double-count"
        );
    }

    #[test]
    fn completed_receiver_reports_complete_for_duplicates() {
        let mut rng = StdRng::seed_from_u64(SEED + 5);
        let key = Key::generate();
        let bundle = test_bundle(5_000, &mut rng);
        let datagrams = match encode_bundle(&bundle, &key, &fec()) {
            Ok(d) => d,
            Err(e) => panic!("encode must succeed: {e}"),
        };

        let mut receiver = BundleReceiver::new(key);
        let decoded = absorb_all(&mut receiver, &datagrams);
        assert!(decoded.is_some());

        // A late duplicate (e.g. the sender re-bursting because the receipt was lost)
        // must surface Complete again so the gateway re-sends the idempotent receipt.
        match receiver.absorb(&datagrams[0]) {
            Ok(Absorb::Complete(again)) => assert_eq!(again.id, bundle.id),
            other => panic!("expected idempotent Complete, got {other:?}"),
        }
    }

    #[test]
    fn progress_counters_feed_the_dashboard() {
        let mut rng = StdRng::seed_from_u64(SEED + 6);
        let key = Key::generate();
        let bundle = test_bundle(22_000, &mut rng);
        let datagrams = match encode_bundle(&bundle, &key, &fec()) {
            Ok(d) => d,
            Err(e) => panic!("encode must succeed: {e}"),
        };

        let mut receiver = BundleReceiver::new(key);
        assert_eq!(receiver.symbols_received(), 0);
        assert_eq!(receiver.symbols_needed(), None);

        let _ = receiver.absorb(&datagrams[0]);
        assert_eq!(receiver.symbols_received(), 1);
        let needed = receiver.symbols_needed();
        assert!(
            matches!(needed, Some(n) if n > 0),
            "after the first symbol the receiver must know the target: {needed:?}"
        );
        let nack = receiver.build_nack();
        assert!(
            matches!(&nack, Some(n) if n.needed.iter().sum::<u32>() > 0),
            "an incomplete decode must request symbols: {nack:?}"
        );
    }
}
