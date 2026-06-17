//! Deterministic, server-free first-taker-wins resolution.
//!
//! When a passenger's ride request is taken, several drivers may publish
//! competing **acceptance** events near-simultaneously. With no aggregator to
//! arbitrate, every client must compute the *same* winner from the same set.
//! The rule:
//!
//! 1. Winner = the acceptance with the lowest `created_at`.
//! 2. Tie-break = the lexicographically smallest `event_id`.
//!
//! Because acceptances are stored (regular-kind) events, a passenger that was
//! briefly offline still resolves the identical winner on reconnect.
//!
//! The timestamp is best-effort "first" (client clocks can skew); the id
//! tie-break is what guarantees every client agrees. Fairness/sybil concerns
//! are explicitly out of scope for v1.

use std::collections::HashSet;

/// One driver's claim on a ride request, reduced to just what arbitration needs
/// (the [`crate::protocol`] layer builds these from Nostr events).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acceptance {
    /// Hex event id of the acceptance (tie-breaker).
    pub event_id: String,
    /// Unix seconds the acceptance was created.
    pub created_at: u64,
    /// Driver's public key (hex).
    pub driver: String,
    /// The request event id this acceptance targets (its `e` tag). The request
    /// is replaceable, so its id changes on each re-publish; the engine tracks
    /// the versions it has published this session.
    pub request_id: String,
}

/// Pick the winning acceptance: earliest `created_at`, tie-broken by the
/// lexicographically smallest `event_id`. `None` for an empty slice. The result
/// is independent of input order.
pub fn winner(acceptances: &[Acceptance]) -> Option<&Acceptance> {
    acceptances.iter().min_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.event_id.cmp(&b.event_id))
    })
}

/// Filter raw acceptances down to the ones that belong to the current request
/// session, de-duplicating by `event_id`:
///
/// - created at or after `session_start` (drops stale claims from a prior ride);
/// - targeting one of `known_request_ids` (the versions the passenger has
///   published this session). If that set is empty, the version check is
///   skipped (accept any) so a freshly-started session still works.
pub fn candidates(
    all: &[Acceptance],
    session_start: u64,
    known_request_ids: &HashSet<String>,
) -> Vec<Acceptance> {
    let mut seen = HashSet::new();
    all.iter()
        .filter(|a| a.created_at >= session_start)
        .filter(|a| known_request_ids.is_empty() || known_request_ids.contains(&a.request_id))
        .filter(|a| seen.insert(a.event_id.clone()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(id: &str, created_at: u64, driver: &str, request_id: &str) -> Acceptance {
        Acceptance {
            event_id: id.to_string(),
            created_at,
            driver: driver.to_string(),
            request_id: request_id.to_string(),
        }
    }

    #[test]
    fn no_acceptances_no_winner() {
        assert!(winner(&[]).is_none());
    }

    #[test]
    fn single_acceptance_wins() {
        let v = vec![acc("aaaa", 100, "drv1", "req1")];
        assert_eq!(winner(&v).unwrap().driver, "drv1");
    }

    #[test]
    fn earliest_created_at_wins() {
        let v = vec![
            acc("zzzz", 100, "early", "req1"),
            acc("aaaa", 200, "late", "req1"),
        ];
        // `early` wins despite a lexicographically larger id, because it's first.
        assert_eq!(winner(&v).unwrap().driver, "early");
    }

    #[test]
    fn tie_broken_by_smallest_event_id() {
        let v = vec![
            acc("bbbb", 100, "drvB", "req1"),
            acc("aaaa", 100, "drvA", "req1"),
            acc("cccc", 100, "drvC", "req1"),
        ];
        assert_eq!(winner(&v).unwrap().driver, "drvA");
    }

    #[test]
    fn winner_is_order_independent() {
        let mut v = vec![
            acc("bbbb", 100, "drvB", "req1"),
            acc("aaaa", 100, "drvA", "req1"),
            acc("cccc", 90, "drvC", "req1"),
        ];
        let w1 = winner(&v).unwrap().clone();
        v.reverse();
        let w2 = winner(&v).unwrap().clone();
        assert_eq!(w1, w2);
        assert_eq!(w1.driver, "drvC"); // earliest created_at
    }

    #[test]
    fn candidates_drop_pre_session_acceptances() {
        let all = vec![
            acc("old", 50, "stale", "reqOld"),
            acc("new", 150, "fresh", "req1"),
        ];
        let known: HashSet<String> = HashSet::new();
        let c = candidates(&all, 100, &known);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].driver, "fresh");
    }

    #[test]
    fn candidates_filter_by_known_request_versions() {
        let all = vec![
            acc("a1", 150, "drvA", "reqV1"),
            acc("a2", 160, "drvB", "reqOther"),
        ];
        let known: HashSet<String> = ["reqV1".to_string()].into_iter().collect();
        let c = candidates(&all, 100, &known);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].driver, "drvA");
    }

    #[test]
    fn candidates_dedup_by_event_id() {
        let all = vec![
            acc("dup", 150, "drvA", "req1"),
            acc("dup", 150, "drvA", "req1"),
        ];
        let known: HashSet<String> = HashSet::new();
        assert_eq!(candidates(&all, 100, &known).len(), 1);
    }

    #[test]
    fn end_to_end_offline_then_resolve() {
        // Passenger was offline; on reconnect it sees three stored acceptances
        // for two request versions and resolves deterministically.
        let all = vec![
            acc("e3", 212, "drvC", "v2"),
            acc("e1", 210, "drvA", "v1"),
            acc("e2", 210, "drvB", "v2"),
        ];
        let known: HashSet<String> = ["v1".to_string(), "v2".to_string()].into_iter().collect();
        let c = candidates(&all, 200, &known);
        assert_eq!(c.len(), 3);
        // created_at 210 ties between e1/e2 → smallest id "e1" (drvA).
        assert_eq!(winner(&c).unwrap().driver, "drvA");
    }
}
