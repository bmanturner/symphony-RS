//! Typed run lifecycle status (SPEC v2 §4.5 cancellation path).
//!
//! Today `runs.status` is a free-form `TEXT` column at the storage layer
//! (see `symphony-state::repository`). The established string labels in
//! flight across the codebase are `"queued"`, `"running"`, `"completed"`,
//! `"failed"`, and `"cancelled"`. The cancellation path adds a sixth
//! intermediate state, `"cancel_requested"`, so a runner that observes a
//! pending cancel mid-flight can record the cooperative-cancel intent
//! durably before reaching the terminal `Cancelled` state.
//!
//! This module is the kernel-side typed view of those labels. It does not
//! yet change storage — that is a separate checklist subtask which will
//! add a CHECK constraint and a `update_run_status_typed` repository
//! wrapper that flows through [`RunStatus::valid_transition`].

use serde::{Deserialize, Serialize};

/// Lifecycle state of a single run row.
///
/// The wire representation is the snake_case label string used by the
/// storage layer (`as_str` and `from_str` are the round-trip seam).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Reserved a row, not yet dispatched.
    Queued,
    /// Lease acquired and the agent is doing work.
    Running,
    /// Cooperative cancellation requested; the runner has observed the
    /// request and is winding down. Terminal transition is `Cancelled`,
    /// but `Completed` and `Failed` are also accepted for the race where
    /// the runner finishes between observation and lease release.
    CancelRequested,
    /// Terminal: agent finished successfully.
    Completed,
    /// Terminal: agent finished with an error.
    Failed,
    /// Terminal: cancelled before reaching `Completed`/`Failed`.
    Cancelled,
}

impl RunStatus {
    /// Every variant in declaration order. Useful for exhaustive iteration
    /// in tests and config validation.
    pub const ALL: [Self; 6] = [
        Self::Queued,
        Self::Running,
        Self::CancelRequested,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    /// Stable wire/storage label. Matches the existing free-form values in
    /// `runs.status` so adopting the typed API is a no-op for the column.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a wire/storage label produced by [`Self::as_str`]. Returns
    /// `None` for any other string. Match is exact (not case-insensitive)
    /// because storage is authoritative for this column.
    pub fn from_label(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == s)
    }

    /// Terminal statuses do not transition further. The cancellation path
    /// treats `CancelRequested` as a non-terminal intermediate state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Allowed transitions:
    ///
    /// * `Queued` → `Running`, `CancelRequested`, `Cancelled`
    /// * `Running` → `Completed`, `Failed`, `CancelRequested`, `Cancelled`
    /// * `CancelRequested` → `Cancelled`, `Failed`, `Completed`
    ///   (`Completed`/`Failed` allowed because a runner may finish before
    ///   observing the cancel)
    ///
    /// All transitions out of a terminal state are rejected, including
    /// terminal-to-self (idempotence is the caller's responsibility — a
    /// repeated `Cancelled` write would mask a logic bug).
    pub fn valid_transition(from: Self, to: Self) -> bool {
        matches!(
            (from, to),
            (Self::Queued, Self::Running)
                | (Self::Queued, Self::CancelRequested)
                | (Self::Queued, Self::Cancelled)
                | (Self::Running, Self::Completed)
                | (Self::Running, Self::Failed)
                | (Self::Running, Self::CancelRequested)
                | (Self::Running, Self::Cancelled)
                | (Self::CancelRequested, Self::Cancelled)
                | (Self::CancelRequested, Self::Failed)
                | (Self::CancelRequested, Self::Completed)
        )
    }
}

impl std::str::FromStr for RunStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_label(s).ok_or(())
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGAL: &[(RunStatus, RunStatus)] = &[
        (RunStatus::Queued, RunStatus::Running),
        (RunStatus::Queued, RunStatus::CancelRequested),
        (RunStatus::Queued, RunStatus::Cancelled),
        (RunStatus::Running, RunStatus::Completed),
        (RunStatus::Running, RunStatus::Failed),
        (RunStatus::Running, RunStatus::CancelRequested),
        (RunStatus::Running, RunStatus::Cancelled),
        (RunStatus::CancelRequested, RunStatus::Cancelled),
        (RunStatus::CancelRequested, RunStatus::Failed),
        (RunStatus::CancelRequested, RunStatus::Completed),
    ];

    #[test]
    fn every_legal_transition_is_accepted() {
        for &(from, to) in LEGAL {
            assert!(
                RunStatus::valid_transition(from, to),
                "expected {from:?} -> {to:?} to be legal"
            );
        }
    }

    #[test]
    fn every_other_transition_is_rejected() {
        let legal: std::collections::HashSet<_> = LEGAL.iter().copied().collect();
        for &from in &RunStatus::ALL {
            for &to in &RunStatus::ALL {
                if legal.contains(&(from, to)) {
                    continue;
                }
                assert!(
                    !RunStatus::valid_transition(from, to),
                    "expected {from:?} -> {to:?} to be rejected"
                );
            }
        }
    }

    #[test]
    fn terminal_states_reject_self_transitions() {
        for &state in &[
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            assert!(!RunStatus::valid_transition(state, state));
        }
    }

    #[test]
    fn terminal_states_reject_all_outgoing_transitions() {
        for &from in &[
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            assert!(from.is_terminal());
            for &to in &RunStatus::ALL {
                assert!(
                    !RunStatus::valid_transition(from, to),
                    "terminal {from:?} should not transition to {to:?}"
                );
            }
        }
    }

    #[test]
    fn cancel_requested_is_not_terminal() {
        assert!(!RunStatus::CancelRequested.is_terminal());
    }

    #[test]
    fn label_round_trip_for_every_variant() {
        for &status in &RunStatus::ALL {
            let label = status.as_str();
            assert_eq!(RunStatus::from_label(label), Some(status));
        }
    }

    #[test]
    fn from_label_rejects_unknown_labels() {
        for bad in [
            "",
            "QUEUED",
            "Queued",
            "done",
            "cancel-requested",
            "in_progress",
            "cancel",
        ] {
            assert!(
                RunStatus::from_label(bad).is_none(),
                "expected `{bad}` to be rejected by from_str"
            );
        }
    }

    #[test]
    fn serde_round_trips_every_variant() {
        for &status in &RunStatus::ALL {
            let json = serde_json::to_string(&status).expect("serialize");
            let back: RunStatus = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, status);
            // The JSON form is the quoted snake_case label.
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
    }

    #[test]
    fn from_str_trait_round_trips_known_labels() {
        for &status in &RunStatus::ALL {
            let parsed: RunStatus = status.as_str().parse().expect("parse");
            assert_eq!(parsed, status);
        }
        assert!("nonsense".parse::<RunStatus>().is_err());
    }

    #[test]
    fn display_matches_as_str() {
        for &status in &RunStatus::ALL {
            assert_eq!(format!("{status}"), status.as_str());
        }
    }
}
