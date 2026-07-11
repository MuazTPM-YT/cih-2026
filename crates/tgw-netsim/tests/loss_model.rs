//! Executable spec for the netsim drop policy (Phase C). Socket-free and deterministic.
//! Currently RED (`LossModel::decide` is `todo!()`). Never weaken a test to pass.

use std::time::Duration;
use tgw_netsim::{LossModel, NetsimConfig};

fn cfg(seed: u64) -> NetsimConfig {
    NetsimConfig {
        loss: 0.25,
        burst_every: Duration::from_secs(5),
        burst_len: Duration::from_millis(800),
        rate_bps: 64_000,
        jitter: Duration::from_millis(40),
        seed,
    }
}

#[test]
fn same_seed_is_deterministic() {
    let mut a = LossModel::new(&cfg(42));
    let mut b = LossModel::new(&cfg(42));
    let seq: Vec<Duration> = (0..1000).map(Duration::from_millis).collect();
    let ra: Vec<bool> = seq.iter().map(|&e| a.decide(e)).collect();
    let rb: Vec<bool> = seq.iter().map(|&e| b.decide(e)).collect();
    assert_eq!(ra, rb, "same seed must reproduce the exact drop sequence");
}

#[test]
fn different_seeds_differ() {
    let mut a = LossModel::new(&cfg(1));
    let mut b = LossModel::new(&cfg(2));
    let ra: Vec<bool> = (0..1000)
        .map(|i| a.decide(Duration::from_millis(i)))
        .collect();
    let rb: Vec<bool> = (0..1000)
        .map(|i| b.decide(Duration::from_millis(i)))
        .collect();
    assert_ne!(ra, rb, "different seeds should produce different patterns");
}

#[test]
fn loss_fraction_matches_configured_rate() {
    let mut m = LossModel::new(&cfg(7));
    let n: usize = 10_000;
    // Keep elapsed under burst_every (5s) so this isolates per-packet loss.
    let dropped = (0..n)
        .filter(|i| m.decide(Duration::from_millis((i % 4000) as u64)))
        .count();
    let frac = dropped as f64 / n as f64;
    assert!(
        (0.235..=0.265).contains(&frac),
        "loss fraction {frac} not within 0.235..=0.265"
    );
}

#[test]
fn burst_window_drops_everything() {
    let mut m = LossModel::new(&cfg(7));
    // Inside the first burst window [5s, 5.8s).
    let inside = Duration::from_millis(5_100);
    let n = 200;
    let dropped = (0..n).filter(|_| m.decide(inside)).count();
    assert_eq!(dropped, n, "every packet inside a burst window must drop");
}

#[test]
fn burst_free_window_forwards_some() {
    let mut m = LossModel::new(&cfg(7));
    let e = Duration::from_millis(100); // < burst_every -> burst-free
    let n = 200;
    let dropped = (0..n).filter(|_| m.decide(e)).count();
    assert!(
        dropped < n,
        "outside bursts, not everything drops (loss ~0.25)"
    );
}
