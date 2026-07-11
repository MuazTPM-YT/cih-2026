//! Fast-fail circuit breaker for a dead direct link (Fix F4).
//!
//! On a link with 100% loss, every bundle otherwise spends its full linear retry budget
//! (`retry_backoff_ms × (1 + 2 + … + max_retries)`) before reaching `stuck` — ~8 s per bundle
//! at the stress sweep's settings, so a 15-bundle blackout burns ~2 minutes of battery and wall
//! clock before the daemon gives up on any of them.
//!
//! [`LinkBreaker`] watches the daemon's terminal delivery outcomes. After
//! `circuit_breaker_threshold` consecutive `stuck` results it *trips*: the link is treated as
//! down and subsequent bundles are probed with a shrunk 1-retry budget (see
//! [`probe_budget`]) instead of the full schedule, so the queue reaches its (kept, never
//! dropped) `stuck` state far faster and stops burning power. Any delivery — direct or via the
//! peer relay — resets the counter and restores the full budget. The breaker never changes
//! *what* happens to a bundle (nothing is ever dropped); it only changes *how long* a hopeless
//! link is flogged.

use tgw_core::RetryConfig;

use crate::sender::Outcome;

/// A shrunk retry budget for probing a link the breaker considers down: the full schedule with
/// `max_retries` clamped to 1, so a probe costs one backoff instead of the full linear sum.
#[must_use]
pub fn probe_budget(full: &RetryConfig) -> RetryConfig {
    RetryConfig {
        max_retries: 1,
        ..full.clone()
    }
}

/// What a recorded outcome did to the breaker's state — the daemon uses this to log transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerEvent {
    /// The link was already up (or already tripped) and nothing changed.
    Unchanged,
    /// This outcome pushed consecutive stucks to the threshold: the link is now treated as down.
    Tripped,
    /// A delivery arrived while tripped: the link recovered and the full budget is restored.
    Recovered,
}

/// Tracks consecutive `stuck` outcomes on the direct link and decides the effective retry budget.
#[derive(Debug, Clone)]
pub struct LinkBreaker {
    threshold: u32,
    consecutive_stuck: u32,
}

impl LinkBreaker {
    /// New breaker that trips after `threshold` consecutive stucks. A `threshold` of 0 disables
    /// the breaker entirely (it never trips; the full budget is always used).
    #[must_use]
    pub fn new(threshold: u32) -> Self {
        LinkBreaker {
            threshold,
            consecutive_stuck: 0,
        }
    }

    /// Whether the breaker is currently tripped (link treated as down).
    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.threshold > 0 && self.consecutive_stuck >= self.threshold
    }

    /// The retry budget to use for the next bundle: `probe` while tripped, else `full`.
    #[must_use]
    pub fn budget<'a>(&self, full: &'a RetryConfig, probe: &'a RetryConfig) -> &'a RetryConfig {
        if self.is_tripped() { probe } else { full }
    }

    /// Fold one terminal delivery outcome into the breaker's state.
    pub fn record(&mut self, outcome: Outcome) -> BreakerEvent {
        match outcome {
            Outcome::Delivered => {
                let was_tripped = self.is_tripped();
                self.consecutive_stuck = 0;
                if was_tripped {
                    BreakerEvent::Recovered
                } else {
                    BreakerEvent::Unchanged
                }
            }
            Outcome::Stuck => {
                let was_tripped = self.is_tripped();
                self.consecutive_stuck = self.consecutive_stuck.saturating_add(1);
                if !was_tripped && self.is_tripped() {
                    BreakerEvent::Tripped
                } else {
                    BreakerEvent::Unchanged
                }
            }
            // A preemption is not a link verdict — it leaves the breaker untouched.
            Outcome::Preempted => BreakerEvent::Unchanged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> RetryConfig {
        // retry_backoff_ms = 400, max_retries = 6 → the stress sweep's dead-link budget.
        RetryConfig {
            retry_backoff_ms: 400,
            max_retries: 6,
            circuit_breaker_threshold: 3,
            ..RetryConfig::default()
        }
    }

    /// The linear budget a full pass would spend on a silent link: `backoff × (1+2+…+R)`.
    fn full_budget_ms(retry: &RetryConfig) -> u64 {
        let r = u64::from(retry.max_retries);
        retry.retry_backoff_ms * (r * (r + 1) / 2)
    }

    #[test]
    fn trips_after_threshold_consecutive_stucks() {
        let mut b = LinkBreaker::new(3);
        assert_eq!(b.record(Outcome::Stuck), BreakerEvent::Unchanged);
        assert_eq!(b.record(Outcome::Stuck), BreakerEvent::Unchanged);
        assert!(!b.is_tripped(), "not tripped before the threshold");
        assert_eq!(b.record(Outcome::Stuck), BreakerEvent::Tripped);
        assert!(b.is_tripped(), "tripped exactly at the threshold");
        // Stays tripped without re-announcing.
        assert_eq!(b.record(Outcome::Stuck), BreakerEvent::Unchanged);
    }

    #[test]
    fn budget_shrinks_to_probe_once_tripped() {
        let (full, probe) = (full(), probe_budget(&full()));
        let mut b = LinkBreaker::new(3);
        assert_eq!(b.budget(&full, &probe).max_retries, full.max_retries);
        for _ in 0..3 {
            b.record(Outcome::Stuck);
        }
        assert_eq!(
            b.budget(&full, &probe).max_retries,
            1,
            "a tripped breaker probes with a 1-retry budget"
        );
    }

    #[test]
    fn a_delivery_resets_and_restores_full_budget() {
        let mut b = LinkBreaker::new(2);
        b.record(Outcome::Stuck);
        b.record(Outcome::Stuck);
        assert!(b.is_tripped());
        assert_eq!(b.record(Outcome::Delivered), BreakerEvent::Recovered);
        assert!(!b.is_tripped(), "a delivery clears the breaker");
        let (full, probe) = (full(), probe_budget(&full()));
        assert_eq!(b.budget(&full, &probe).max_retries, full.max_retries);
    }

    #[test]
    fn blackout_wall_clock_drops_far_below_full_budget_sum() {
        // Model a 15-bundle blackout: every bundle is stuck. Without the breaker each spends the
        // full linear budget; with the breaker, bundles past the threshold spend only a probe.
        let full = full();
        let probe = probe_budget(&full);
        let full_cost = full_budget_ms(&full);
        let probe_cost = full_budget_ms(&probe);

        let mut b = LinkBreaker::new(full.circuit_breaker_threshold);
        let mut modeled_ms = 0u64;
        for _ in 0..15 {
            modeled_ms += full_budget_ms(b.budget(&full, &probe));
            b.record(Outcome::Stuck);
        }

        let no_breaker_ms = full_cost * 15;
        // First `threshold` bundles at full cost, the remaining 12 at probe cost.
        let expected = full_cost * u64::from(full.circuit_breaker_threshold) + probe_cost * 12;
        assert_eq!(modeled_ms, expected);
        assert!(
            modeled_ms * 3 < no_breaker_ms,
            "breaker must cut blackout give-up time to well under a third: {modeled_ms}ms vs {no_breaker_ms}ms"
        );
    }
}
