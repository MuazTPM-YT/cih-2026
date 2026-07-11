//! Executable spec for the token-bucket pacer (Phase C). Pure timing math, no sockets.
//! Currently RED (`Pacer::schedule` is `todo!()`). Semantics are fixed in the type docs.

use std::time::Duration;
use tgw_netsim::Pacer;

const RATE: u64 = 64_000; // bits per second

#[test]
fn single_packet_on_idle_link_has_zero_delay() {
    let mut p = Pacer::new(RATE);
    let d = p.schedule(Duration::ZERO, 800);
    assert_eq!(
        d,
        Duration::ZERO,
        "an idle link imposes no delay on the first packet"
    );
}

#[test]
fn back_to_back_packets_serialise_at_rate() {
    let mut p = Pacer::new(RATE);
    let bits = 8_000u64;
    let now = Duration::ZERO;
    // All arrive simultaneously; packet k must wait for the previous k packets to drain.
    let delays: Vec<Duration> = (0..5).map(|_| p.schedule(now, bits)).collect();
    for (k, d) in delays.iter().enumerate() {
        let expected = k as f64 * bits as f64 / RATE as f64;
        let diff = (d.as_secs_f64() - expected).abs();
        assert!(
            diff < 1e-6,
            "packet {k}: delay {d:?} != expected {expected}s"
        );
    }
}

#[test]
fn sending_n_bits_takes_at_least_n_over_rate() {
    let mut p = Pacer::new(RATE);
    let now = Duration::ZERO;
    let packets = 10u64;
    let bits = 6_400u64;
    let last = (0..packets)
        .map(|_| p.schedule(now, bits))
        .last()
        .expect("at least one packet");
    // Link stays busy until the last packet finishes transmitting.
    let total = last.as_secs_f64() + bits as f64 / RATE as f64;
    let floor = (packets * bits) as f64 / RATE as f64;
    assert!(total + 1e-9 >= floor, "total {total}s < N/rate {floor}s");
}
