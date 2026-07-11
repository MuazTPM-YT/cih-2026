//! THE integration test — the submission's central evidence (brief §7: "evidence, not
//! claims"). OWNER: Twaha. Implement in Phase D of tasks/twaha-agent-prompt.md.
//!
//! Goal: spawn the gateway + field sender in-process, link them through `tgw-netsim` at
//! 25% random loss + a burst episode + a 64 kbps rate cap + jitter, and assert that
//! 5 vitals bundles and one ~25 KB image bundle all arrive INTACT (hash equality), that
//! delivery receipts are issued, and that it all completes within a bounded time.
//! Seed the netsim RNG so the run is deterministic and repeatable in CI.

#[test]
#[ignore = "PHASE-D: not yet implemented — see tasks/twaha-agent-prompt.md"]
fn vitals_and_image_survive_25pct_loss() {
    todo!("PHASE-D: seeded lossy-delivery integration test")
}
