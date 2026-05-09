//! Logical queue identity and per-tick outcome (SPEC v2 §5/ARCHITECTURE v2 §7.1).
//!
//! Symphony-RS v2 replaces the flat poll loop with a fan of *logical queues*
//! that share a polling cadence (`polling.interval_ms`, optionally jittered)
//! but each own a distinct slice of work:
//!
//! * `intake` — tracker-poll/active-set ingestion and triage.
//! * `specialist` — claiming routable specialist work and emitting dispatch
//!   requests.
//! * `integration` — draining the integration queue under integration gates.
//! * `qa` — draining the QA queue under blocker/integration gates.
//! * `followup_approval` — approval-routed follow-up requests waiting on an
//!   operator decision.
//! * `budget_pause` — durable budget pauses awaiting policy/operator resume.
//! * `recovery` — expired-lease and orphaned-claim reconciliation that runs
//!   on every cadence (distinct from the operator `recovery` command).
//!
//! This module owns the *identity* of those queues and the *shape* of a
//! single tick's report. The per-queue tick trait, the concrete tick
//! implementations, and the multi-queue scheduler are layered on top in
//! later checklist items; keeping the enum and the outcome struct in their
//! own module lets state, status surfaces, and tests refer to them without
//! pulling in scheduler internals.
//!
//! Both types are `Serialize` + `Deserialize` because they cross persistence,
//! event-bus, and JSON status boundaries. The enum uses `snake_case` so the
//! wire form matches the names used in SPEC v2 / ARCHITECTURE v2 §7.1
//! verbatim.

use serde::{Deserialize, Serialize};

/// Identity of a single logical queue in the v2 scheduler fan.
///
/// Variants are listed in the same order they appear in
/// ARCHITECTURE v2 §7.1; `BudgetPause` and `Recovery` extend that list with
/// the durable-budget and lease-reconciliation queues called out in
/// SPEC v2 §5 / Phase 11 of the v2 checklist. The `snake_case` rename keeps
/// the JSON / event payload form identical to the prose names used in the
/// architecture doc and avoids divergence between code, durable events, and
/// operator-facing status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalQueue {
    /// Tracker-poll / active-set ingestion and triage.
    Intake,
    /// Claiming routable specialist work and emitting dispatch requests.
    Specialist,
    /// Draining `integration_queue` entries under integration gates.
    Integration,
    /// Draining `qa_queue` entries under blocker/integration gates.
    Qa,
    /// Approval-routed follow-up requests awaiting operator decision.
    FollowupApproval,
    /// Durable budget pauses awaiting policy/operator resume.
    BudgetPause,
    /// Expired-lease and orphaned-claim reconciliation per cadence.
    Recovery,
}

impl LogicalQueue {
    /// All variants in declaration order. Useful for scheduler wiring,
    /// dashboards, and exhaustiveness tests.
    pub const ALL: [Self; 7] = [
        Self::Intake,
        Self::Specialist,
        Self::Integration,
        Self::Qa,
        Self::FollowupApproval,
        Self::BudgetPause,
        Self::Recovery,
    ];

    /// Stable lowercase identifier matching the serde rename. Used directly
    /// in event payloads, log lines, and status JSON so that the wire form
    /// never drifts from `Display`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Specialist => "specialist",
            Self::Integration => "integration",
            Self::Qa => "qa",
            Self::FollowupApproval => "followup_approval",
            Self::BudgetPause => "budget_pause",
            Self::Recovery => "recovery",
        }
    }
}

impl std::fmt::Display for LogicalQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Per-tick observability summary produced by a single queue tick.
///
/// Modeled after [`crate::poll_loop::TickReport`] but generalized across the
/// v2 logical queues. Each tick reports four small counts:
///
/// * `considered` — entries the tick examined (rows in the queue table,
///   active-set members, expired leases, etc.).
/// * `processed` — entries the tick acted on successfully (claims taken,
///   dispatches emitted, leases reaped, follow-ups approved, …). Always
///   `<= considered`.
/// * `deferred` — entries the tick intentionally skipped because of a gate,
///   capacity cap, or pending dependency. These are *not* failures; they
///   are expected to be retried on a later tick.
/// * `errors` — entries the tick failed to process because of an
///   unrecoverable-this-tick error (tracker outage, SQLite contention, …).
///
/// The shape stays small integer-only on purpose: per-tick reports flow
/// through events, logs, and JSON status; carrying full payload vectors
/// would balloon those surfaces. Tick implementations record richer
/// per-entry detail through the durable event log instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueTickOutcome {
    /// Which logical queue this report describes.
    pub queue: LogicalQueue,
    /// Entries the tick examined.
    pub considered: usize,
    /// Entries successfully processed (claims, dispatches, reaps).
    pub processed: usize,
    /// Entries intentionally skipped (gates, capacity, dependencies).
    pub deferred: usize,
    /// Entries that failed with an unrecoverable-this-tick error.
    pub errors: usize,
}

impl QueueTickOutcome {
    /// Empty outcome for `queue` — the tick ran but had no work to do.
    pub fn idle(queue: LogicalQueue) -> Self {
        Self {
            queue,
            ..Self::default()
        }
    }

    /// `true` iff nothing was considered, processed, deferred, or errored.
    /// Useful for tests and operator surfaces that want to collapse
    /// no-op ticks visually.
    pub fn is_idle(&self) -> bool {
        self.considered == 0 && self.processed == 0 && self.deferred == 0 && self.errors == 0
    }
}

impl Default for LogicalQueue {
    /// `Intake` is the canonical "first" queue in the cadence; using it as
    /// the default makes [`QueueTickOutcome::default`] meaningful for
    /// builders/tests without forcing a queue selection. Real scheduler
    /// code always sets `queue` explicitly.
    fn default() -> Self {
        Self::Intake
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_variant_in_declaration_order() {
        let names: Vec<_> = LogicalQueue::ALL.iter().map(|q| q.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "intake",
                "specialist",
                "integration",
                "qa",
                "followup_approval",
                "budget_pause",
                "recovery",
            ]
        );
    }

    #[test]
    fn display_matches_as_str_for_every_variant() {
        for q in LogicalQueue::ALL {
            assert_eq!(format!("{q}"), q.as_str());
        }
    }

    #[test]
    fn logical_queue_serializes_as_snake_case() {
        for q in LogicalQueue::ALL {
            let json = serde_json::to_string(&q).unwrap();
            assert_eq!(json, format!("\"{}\"", q.as_str()));
        }
    }

    #[test]
    fn logical_queue_round_trips_through_json() {
        for q in LogicalQueue::ALL {
            let json = serde_json::to_string(&q).unwrap();
            let back: LogicalQueue = serde_json::from_str(&json).unwrap();
            assert_eq!(back, q);
        }
    }

    #[test]
    fn logical_queue_rejects_unknown_wire_value() {
        let err = serde_json::from_str::<LogicalQueue>("\"unknown_queue\"").unwrap_err();
        assert!(err.to_string().contains("unknown_queue"));
    }

    #[test]
    fn logical_queue_rejects_camel_case_wire_value() {
        // Guards against accidental rename-policy drift: the wire form
        // is locked to snake_case so SPEC v2 / ARCHITECTURE v2 names
        // round-trip verbatim.
        assert!(serde_json::from_str::<LogicalQueue>("\"FollowupApproval\"").is_err());
        assert!(serde_json::from_str::<LogicalQueue>("\"followupApproval\"").is_err());
    }

    #[test]
    fn outcome_idle_constructor_produces_zeroed_counts_for_queue() {
        for q in LogicalQueue::ALL {
            let o = QueueTickOutcome::idle(q);
            assert_eq!(o.queue, q);
            assert_eq!(o.considered, 0);
            assert_eq!(o.processed, 0);
            assert_eq!(o.deferred, 0);
            assert_eq!(o.errors, 0);
            assert!(o.is_idle());
        }
    }

    #[test]
    fn outcome_is_idle_only_when_all_counts_zero() {
        let mut o = QueueTickOutcome::idle(LogicalQueue::Specialist);
        assert!(o.is_idle());
        o.considered = 1;
        assert!(!o.is_idle());
        o = QueueTickOutcome::idle(LogicalQueue::Specialist);
        o.processed = 1;
        assert!(!o.is_idle());
        o = QueueTickOutcome::idle(LogicalQueue::Specialist);
        o.deferred = 1;
        assert!(!o.is_idle());
        o = QueueTickOutcome::idle(LogicalQueue::Specialist);
        o.errors = 1;
        assert!(!o.is_idle());
    }

    #[test]
    fn outcome_round_trips_through_json_with_full_field_set() {
        let o = QueueTickOutcome {
            queue: LogicalQueue::Qa,
            considered: 4,
            processed: 2,
            deferred: 1,
            errors: 1,
        };
        let json = serde_json::to_string(&o).unwrap();
        // Field names match snake_case Rust names, queue label is snake_case.
        assert!(json.contains("\"queue\":\"qa\""), "json: {json}");
        assert!(json.contains("\"considered\":4"), "json: {json}");
        assert!(json.contains("\"processed\":2"), "json: {json}");
        assert!(json.contains("\"deferred\":1"), "json: {json}");
        assert!(json.contains("\"errors\":1"), "json: {json}");
        let back: QueueTickOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, o);
    }

    #[test]
    fn outcome_default_uses_intake_with_zero_counts() {
        let o = QueueTickOutcome::default();
        assert_eq!(o.queue, LogicalQueue::Intake);
        assert!(o.is_idle());
    }

    #[test]
    fn outcome_round_trips_for_every_queue_via_idle() {
        for q in LogicalQueue::ALL {
            let o = QueueTickOutcome::idle(q);
            let json = serde_json::to_string(&o).unwrap();
            let back: QueueTickOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, o);
        }
    }
}
