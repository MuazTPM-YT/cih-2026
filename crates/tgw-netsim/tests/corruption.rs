//! Executable spec for the netsim bit-flip corruption policy. Socket-free and deterministic.
//!
//! Corruption is the degradation a pure-loss proxy cannot model: a datagram that *survives* the
//! radio link but arrives with flipped bits. It is what the gateway's per-datagram integrity tag
//! (Fix 1a) exists to reject before RaptorQ ever absorbs a symbol. This seam lets the end-to-end
//! harness inject reproducible corruption so that path is exercised by the live binaries, not
//! only by unit tests. Never weaken a test to pass.

use std::time::Duration;

use tgw_netsim::{Corruptor, NetsimConfig};

fn cfg(corrupt: f64, seed: u64) -> NetsimConfig {
    NetsimConfig {
        loss: 0.0,
        corrupt,
        burst_every: Duration::from_secs(5),
        burst_len: Duration::from_millis(800),
        rate_bps: 64_000,
        jitter: Duration::from_millis(40),
        seed,
    }
}

#[test]
fn same_seed_is_deterministic() {
    let mut a = Corruptor::new(&cfg(0.5, 42));
    let mut b = Corruptor::new(&cfg(0.5, 42));
    for i in 0..1000u16 {
        let mut pa = i.to_be_bytes().to_vec();
        let mut pb = pa.clone();
        let ra = a.corrupt(&mut pa);
        let rb = b.corrupt(&mut pb);
        assert_eq!(ra, rb, "same seed must reproduce the same corrupt decision");
        assert_eq!(pa, pb, "same seed must reproduce the same flipped bytes");
    }
}

#[test]
fn corrupt_fraction_matches_configured_rate() {
    let mut c = Corruptor::new(&cfg(0.3, 7));
    let n = 10_000;
    let flipped = (0..n).filter(|_| c.corrupt(&mut [0u8; 64])).count();
    let frac = flipped as f64 / n as f64;
    assert!(
        (0.285..=0.315).contains(&frac),
        "corrupt fraction {frac} not within 0.285..=0.315"
    );
}

#[test]
fn zero_rate_never_modifies() {
    let mut c = Corruptor::new(&cfg(0.0, 1));
    for i in 0..1000u32 {
        let original = i.to_be_bytes().to_vec();
        let mut packet = original.clone();
        assert!(!c.corrupt(&mut packet), "corrupt=0.0 must never flip");
        assert_eq!(packet, original, "corrupt=0.0 must leave bytes untouched");
    }
}

#[test]
fn a_flipped_packet_actually_differs_but_keeps_its_length() {
    let mut c = Corruptor::new(&cfg(1.0, 99));
    for i in 0..500u32 {
        let original = [0xAAu8; 40].to_vec();
        let mut packet = original.clone();
        let flipped = c.corrupt(&mut packet);
        assert!(flipped, "corrupt=1.0 must always flip");
        assert_eq!(packet.len(), original.len(), "length must be preserved");
        assert_ne!(packet, original, "a flipped packet must actually differ");
        let _ = i;
    }
}

#[test]
fn empty_packet_is_a_safe_noop() {
    let mut c = Corruptor::new(&cfg(1.0, 3));
    let mut empty: Vec<u8> = Vec::new();
    // Nothing to flip; must not panic and must report no modification.
    assert!(
        !c.corrupt(&mut empty),
        "an empty datagram cannot be corrupted"
    );
}
