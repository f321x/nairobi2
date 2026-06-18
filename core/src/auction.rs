//! The escalating-rate auction, expressed as pure functions of elapsed time.
//!
//! A passenger posts a ride request with a `start_rate` and `max_rate` per km
//! (whole currency units). The offered rate climbs in equal steps every
//! [`STEP_SECS`] for [`ESCALATION_SECS`] — 10 steps over 5 minutes — reaching
//! exactly `max_rate` at the end, then holds. The whole request stops after
//! [`MAX_LIFETIME_SECS`] if no driver has taken it.
//!
//! Everything here is integer/deterministic and exhaustively host-tested; the
//! engine just calls [`Auction::rate_at`] on each tick to decide what to
//! re-publish.

use serde::{Deserialize, Serialize};

/// Seconds between rate increases.
pub const STEP_SECS: u64 = 30;
/// Total escalation window (start → max).
pub const ESCALATION_SECS: u64 = 300;
/// Number of discrete steps (`ESCALATION_SECS / STEP_SECS`).
pub const STEPS: u64 = ESCALATION_SECS / STEP_SECS;
/// Overall request lifetime; past this the passenger is prompted to retry.
pub const MAX_LIFETIME_SECS: u64 = 900;

/// A passenger's escalating price offer (currency units per km).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Auction {
    pub start_rate: u32,
    pub max_rate: u32,
}

impl Auction {
    /// Construct an auction, normalizing so `max_rate >= start_rate`.
    pub fn new(start_rate: u32, max_rate: u32) -> Self {
        Self {
            start_rate,
            max_rate: max_rate.max(start_rate),
        }
    }

    /// The offered rate per km after `elapsed_secs`. Equals `start_rate` at
    /// `t = 0`, increases by one step every [`STEP_SECS`], and equals exactly
    /// `max_rate` at and after [`ESCALATION_SECS`].
    pub fn rate_at(&self, elapsed_secs: u64) -> u32 {
        if self.max_rate <= self.start_rate {
            return self.start_rate;
        }
        let step_index = (elapsed_secs / STEP_SECS).min(STEPS);
        let span = u64::from(self.max_rate - self.start_rate);
        // Reaches max exactly at step_index == STEPS; floor-rounds intermediate
        // steps (monotonic non-decreasing).
        let inc = span * step_index / STEPS;
        self.start_rate + inc as u32
    }

    /// Total fare for `distance_km` at the rate after `elapsed_secs`, rounded to
    /// the nearest whole currency unit.
    pub fn fare_at(&self, elapsed_secs: u64, distance_km: f64) -> u32 {
        fare(self.rate_at(elapsed_secs), distance_km)
    }
}

/// Total fare for a trip of `distance_km` at `rate` per km, rounded to nearest.
pub fn fare(rate: u32, distance_km: f64) -> u32 {
    (f64::from(rate) * distance_km).round().max(0.0) as u32
}

/// Whether escalation has completed (rate sits at `max_rate`).
pub fn at_max(elapsed_secs: u64) -> bool {
    elapsed_secs >= ESCALATION_SECS
}

/// Seconds remaining until the next rate step, while escalation is still
/// running. Counts down `STEP_SECS`→1 within each step and resets; returns
/// `None` once the rate has reached `max_rate` (no further steps).
pub fn secs_to_next_step(elapsed_secs: u64) -> Option<u64> {
    if at_max(elapsed_secs) {
        return None;
    }
    Some(STEP_SECS - (elapsed_secs % STEP_SECS))
}

/// Whether the request has outlived [`MAX_LIFETIME_SECS`] and should stop.
pub fn is_expired(elapsed_secs: u64) -> bool {
    elapsed_secs >= MAX_LIFETIME_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ten_steps_over_five_minutes() {
        assert_eq!(STEPS, 10);
    }

    #[test]
    fn starts_at_start_rate() {
        let a = Auction::new(20, 120);
        assert_eq!(a.rate_at(0), 20);
        assert_eq!(a.rate_at(29), 20); // still step 0
    }

    #[test]
    fn first_step_at_thirty_seconds() {
        let a = Auction::new(20, 120); // span 100, step 10
        assert_eq!(a.rate_at(30), 30);
        assert_eq!(a.rate_at(60), 40);
    }

    #[test]
    fn reaches_exactly_max_at_five_minutes() {
        let a = Auction::new(20, 120);
        assert_eq!(a.rate_at(ESCALATION_SECS), 120);
    }

    #[test]
    fn holds_at_max_after_escalation() {
        let a = Auction::new(20, 120);
        assert_eq!(a.rate_at(ESCALATION_SECS + 1), 120);
        assert_eq!(a.rate_at(10_000), 120);
    }

    #[test]
    fn rate_is_monotonic_non_decreasing() {
        let a = Auction::new(13, 99); // non-divisible span exercises rounding
        let mut prev = 0;
        for t in 0..=400 {
            let r = a.rate_at(t);
            assert!(r >= prev, "rate dropped at t={t}: {r} < {prev}");
            assert!(r >= a.start_rate && r <= a.max_rate);
            prev = r;
        }
        assert_eq!(a.rate_at(ESCALATION_SECS), 99);
    }

    #[test]
    fn equal_start_and_max_is_constant() {
        let a = Auction::new(50, 50);
        assert_eq!(a.rate_at(0), 50);
        assert_eq!(a.rate_at(1000), 50);
    }

    #[test]
    fn max_below_start_is_normalized() {
        let a = Auction::new(80, 40);
        assert_eq!(a.max_rate, 80);
        assert_eq!(a.rate_at(0), 80);
        assert_eq!(a.rate_at(1000), 80);
    }

    #[test]
    fn fare_rounds_to_nearest() {
        assert_eq!(fare(30, 4.2), 126); // 126.0
        assert_eq!(fare(30, 4.25), 128); // 127.5 → 128
        assert_eq!(fare(0, 10.0), 0);
    }

    #[test]
    fn fare_at_combines_rate_and_distance() {
        let a = Auction::new(20, 120);
        assert_eq!(a.fare_at(0, 5.0), 100); // 20 * 5
        assert_eq!(a.fare_at(ESCALATION_SECS, 5.0), 600); // 120 * 5
    }

    #[test]
    fn countdown_to_next_step() {
        // Counts down within a step and resets at each boundary.
        assert_eq!(secs_to_next_step(0), Some(30));
        assert_eq!(secs_to_next_step(1), Some(29));
        assert_eq!(secs_to_next_step(29), Some(1));
        assert_eq!(secs_to_next_step(30), Some(30));
        assert_eq!(secs_to_next_step(45), Some(15));
        // No more steps once the rate has reached max.
        assert_eq!(secs_to_next_step(ESCALATION_SECS), None);
        assert_eq!(secs_to_next_step(ESCALATION_SECS + 100), None);
    }

    #[test]
    fn lifecycle_boundaries() {
        assert!(!at_max(ESCALATION_SECS - 1));
        assert!(at_max(ESCALATION_SECS));
        assert!(!is_expired(MAX_LIFETIME_SECS - 1));
        assert!(is_expired(MAX_LIFETIME_SECS));
    }
}
