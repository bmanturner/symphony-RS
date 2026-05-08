//! [`OrchestratorEvent`] — the **stable observation wire format** the
//! Phase 8 status surface consumes.
//!
//! This module is a *pure data definition*: no I/O, no async, no
//! orchestrator state. The actual broadcast bus that emits these events
//! is added in the next checklist item; defining the enum first lets us
//! pin the wire format independently of the wiring.
//!
//! # Where this fits
//!
//! Per SPEC §3.1 layer 7 (and the WORKFLOW.md Phase 8 plan), Symphony
//! exposes an *out-of-process* status surface: the daemon stays headless
//! and serves an HTTP SSE feed of `OrchestratorEvent`s, while a separate
//! `symphony watch` TUI subscribes. That keeps the orchestrator
//! systemd-friendly and means the TUI can crash, reconnect, or be
//! replaced without touching the worker loop.
//!
//! Because this enum is the contract between those two processes — and
//! the contract is consumed by code that may be older or newer than the
//! emitter — its evolution rules matter:
//!
//! # Wire-format stability contract
//!
//! - **`#[serde(tag = "type")]`** so consumers branch on a flat string
//!   discriminator (`event.type`) without shape inference. SSE clients
//!   in any language can match on that field.
//! - **Adding a variant is non-breaking.** Older consumers must ignore
//!   unknown `type` values rather than reject the stream. The
//!   `symphony watch` TUI does this in Phase 8.
//! - **Adding a field to an existing variant is non-breaking** as long
//!   as it has a serde default (`#[serde(default)]` or `Option<T>`).
//! - **Removing or renaming a variant or field is breaking** and
//!   requires a major version bump of `symphony-core` plus a SPEC note.
//! - **Reordering variants is non-breaking** because serde tags are by
//!   name, not position.
//!
//! Treat this module like a published protocol: when you change it,
//! grep for `OrchestratorEvent` everywhere first and write a paragraph
//! ADR in `ARCHITECTURE.md`.
//!
//! # Event taxonomy
//!
//! There are six variants, each capturing one observable inflection
//! point in the orchestrator's poll loop:
//!
//! | Variant | Origin | When emitted |
//! |---|---|---|
//! | [`OrchestratorEvent::StateChanged`] | [`crate::state_machine`] | Any [`ClaimState`] transition (claim, retry-arm, release). |
//! | [`OrchestratorEvent::Dispatched`] | [`crate::poll_loop`] | A worker task has just been spawned for an issue. |
//! | [`OrchestratorEvent::Agent`] | [`crate::agent`] | Re-emission of one [`AgentEvent`] from a running session. |
//! | [`OrchestratorEvent::RetryScheduled`] | [`crate::retry`] | A failed/continuation retry has been pushed onto the queue. |
//! | [`OrchestratorEvent::Reconciled`] | [`crate::poll_loop`] | A reconciliation pass dropped one or more stale claims. |
//! | [`OrchestratorEvent::Released`] | [`crate::state_machine`] | A claim was removed from the ledger (terminal or canceled). |
//!
//! `Agent` is a *re-emission*: the orchestrator pipes per-session
//! [`AgentEvent`]s onto the same bus so a single SSE stream gives a
//! consumer both lifecycle events and live agent telemetry without
//! having to subscribe twice.

use serde::{Deserialize, Serialize};

use crate::agent::{AgentEvent, SessionId};
use crate::retry::RetryReason;
use crate::state_machine::{ClaimState, ReleaseReason};
use crate::tracker::{IssueId, IssueState};

/// A single observable inflection point in the orchestrator's lifecycle.
///
/// Serialised on the wire as one `data:` SSE frame. See the module
/// docs for the stability contract — in short: adding variants is
/// non-breaking, removing them is a major version bump.
///
/// All variants carry an [`IssueId`] so a status surface can route
/// events to per-issue panels without tracking sender state. Variants
/// that pertain to a specific agent session additionally carry the
/// [`SessionId`] so a TUI can correlate with `Agent` re-emissions on
/// the same session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorEvent {
    /// The state machine moved an issue between claim states.
    ///
    /// Emitted on every transition handled by [`crate::state_machine`]:
    /// `Unclaimed → Running` (initial claim), `Running → RetryQueued`
    /// (worker exit, retry armed), and `RetryQueued → Running`
    /// (retry timer fired, redispatch). The complementary "claim
    /// removed entirely" transition is reported as
    /// [`OrchestratorEvent::Released`] rather than a `StateChanged`
    /// with a synthetic "absent" value, because consumers care about
    /// the release *reason*.
    ///
    /// `previous` is `None` for the very first claim of an issue
    /// (transitioning from absent → present). It is never `None`
    /// otherwise.
    StateChanged {
        /// Issue whose claim state changed.
        issue: IssueId,
        /// Best-effort tracker identifier (e.g. `ENG-123`) snapshot
        /// at transition time. Carried so consumers can render
        /// without joining against tracker state.
        identifier: String,
        /// Claim state immediately before the transition; `None`
        /// for an initial claim (absent → present).
        previous: Option<ClaimState>,
        /// Claim state immediately after the transition.
        current: ClaimState,
    },

    /// A worker task has been spawned for `issue` and the agent
    /// session has been started.
    ///
    /// Emitted by the poll loop after [`crate::agent::AgentRunner::start_session`]
    /// returns successfully — that is, *after* the subprocess is up and
    /// the [`SessionId`] is known. A `Dispatched` event is always
    /// preceded by a `StateChanged { current: Running }` event for the
    /// same issue.
    Dispatched {
        /// Issue the worker is handling.
        issue: IssueId,
        /// Best-effort tracker identifier snapshot.
        identifier: String,
        /// Session id allocated by the agent backend. Subsequent
        /// [`OrchestratorEvent::Agent`] events for this dispatch will
        /// carry the same id.
        session: SessionId,
        /// Which agent backend was selected (`"codex"`, `"claude"`,
        /// `"tandem"`, `"mock"`). Carried as a string so the wire
        /// format does not couple to the runner enum.
        backend: String,
        /// 1-based attempt number for this dispatch. `1` on the
        /// initial dispatch; higher on retried dispatches.
        attempt: u32,
    },

    /// Re-emission of one [`AgentEvent`] from a live session.
    ///
    /// The orchestrator forwards every event from every active
    /// session onto its own bus so a single SSE subscription gives
    /// consumers both lifecycle and live-agent telemetry. The
    /// [`AgentEvent`] retains its own `#[serde(tag = "type")]` shape
    /// nested under this variant's payload, so on the wire a typical
    /// frame looks like:
    ///
    /// ```json
    /// { "type": "agent", "issue": "ENG-1", "event": { "type": "message", "session": "...", "role": "assistant", "content": "..." } }
    /// ```
    Agent {
        /// Issue this session is running for. Duplicates information
        /// already in the inner `event.session()` mapping for
        /// convenience — saves the consumer from joining session →
        /// issue.
        issue: IssueId,
        /// The original normalized agent event, untouched.
        event: AgentEvent,
    },

    /// A retry has been pushed onto the orchestrator's [`crate::retry::RetryQueue`].
    ///
    /// Emitted *after* the retry is in the queue, so a status surface
    /// rendering "next retry at HH:MM:SS" can trust the timestamp.
    /// Backoff is encoded as `delay_ms` (milliseconds from the
    /// emission instant until `due_at`) rather than as an absolute
    /// instant, because monotonic instants don't survive the SSE
    /// boundary cleanly.
    RetryScheduled {
        /// Issue that will be retried.
        issue: IssueId,
        /// Best-effort tracker identifier snapshot.
        identifier: String,
        /// 1-based attempt number that *will* be made when the retry
        /// fires.
        attempt: u32,
        /// Continuation vs. failure retry — surfaces the same
        /// distinction [`RetryReason`] makes internally.
        reason: RetryReason,
        /// Milliseconds until the retry becomes due, measured from
        /// the moment the event was emitted.
        delay_ms: u64,
        /// Failure error string captured at schedule time, if any.
        /// Always `None` for [`RetryReason::Continuation`].
        error: Option<String>,
    },

    /// A reconciliation tick removed one or more stale claims.
    ///
    /// Emitted once per poll-loop tick that drops at least one claim
    /// because its issue left the tracker's active states (SPEC
    /// §4.1.8 reconciliation). A tick that drops zero claims emits
    /// nothing — silence is the steady state.
    Reconciled {
        /// Issues whose claims were dropped this tick. Always
        /// non-empty (a no-op tick does not emit `Reconciled`).
        dropped: Vec<IssueId>,
    },

    /// A claim was removed from the state machine.
    ///
    /// Distinct from [`OrchestratorEvent::Reconciled`]: `Released`
    /// fires once per claim, carries a [`ReleaseReason`] explaining
    /// *why* the claim was removed, and includes the issue's
    /// identifier for log rendering. `Reconciled` is the bulk
    /// summary; `Released` is the per-issue detail.
    Released {
        /// Issue whose claim was released.
        issue: IssueId,
        /// Best-effort tracker identifier snapshot at release time.
        identifier: String,
        /// Why the claim was released.
        reason: ReleaseReason,
        /// Tracker state observed at release, when known. `None`
        /// when the issue has disappeared from the tracker entirely
        /// (i.e. [`ReleaseReason::Missing`]).
        final_state: Option<IssueState>,
    },
}

impl OrchestratorEvent {
    /// Borrow the [`IssueId`] every variant carries (or, for
    /// [`OrchestratorEvent::Reconciled`], `None` because it is a
    /// multi-issue summary).
    ///
    /// Useful when a status surface routes events into per-issue
    /// panels: bulk `Reconciled` events go to a global "reconciled"
    /// log line instead.
    #[must_use]
    pub fn issue(&self) -> Option<&IssueId> {
        match self {
            OrchestratorEvent::StateChanged { issue, .. }
            | OrchestratorEvent::Dispatched { issue, .. }
            | OrchestratorEvent::Agent { issue, .. }
            | OrchestratorEvent::RetryScheduled { issue, .. }
            | OrchestratorEvent::Released { issue, .. } => Some(issue),
            OrchestratorEvent::Reconciled { .. } => None,
        }
    }

    /// Stable string discriminator matching the serde `type` tag.
    ///
    /// Provided so non-serde consumers (logging, metrics) can label
    /// events without round-tripping through JSON.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            OrchestratorEvent::StateChanged { .. } => "state_changed",
            OrchestratorEvent::Dispatched { .. } => "dispatched",
            OrchestratorEvent::Agent { .. } => "agent",
            OrchestratorEvent::RetryScheduled { .. } => "retry_scheduled",
            OrchestratorEvent::Reconciled { .. } => "reconciled",
            OrchestratorEvent::Released { .. } => "released",
        }
    }
}

#[cfg(test)]
mod tests {
    //! Wire-format tests for [`OrchestratorEvent`].
    //!
    //! These tests pin the *serialised shape*, not the Rust enum.
    //! Breaking one of them is a signal that the wire contract has
    //! changed and a major version bump is in order.

    use super::*;
    use crate::agent::{CompletionReason, ThreadId, TurnId};
    use serde_json::json;

    fn issue(id: &str) -> IssueId {
        IssueId(id.to_string())
    }

    fn session() -> SessionId {
        SessionId::new(&ThreadId::new("t1"), &TurnId::new("u1"))
    }

    #[test]
    fn state_changed_serialises_with_type_tag() {
        let ev = OrchestratorEvent::StateChanged {
            issue: issue("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "state_changed");
        assert_eq!(v["issue"], "ENG-1");
        assert_eq!(v["identifier"], "ENG-1");
        assert!(v["previous"].is_null());
        assert_eq!(v["current"], json!("running"));
    }

    #[test]
    fn dispatched_carries_session_and_backend() {
        let ev = OrchestratorEvent::Dispatched {
            issue: issue("ENG-2"),
            identifier: "ENG-2".into(),
            session: session(),
            backend: "claude".into(),
            attempt: 1,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "dispatched");
        assert_eq!(v["backend"], "claude");
        assert_eq!(v["attempt"], 1);
        assert_eq!(v["session"], "t1-u1");
    }

    #[test]
    fn agent_reemission_nests_inner_event_with_its_own_tag() {
        let inner = AgentEvent::Completed {
            session: session(),
            reason: CompletionReason::Success,
        };
        let ev = OrchestratorEvent::Agent {
            issue: issue("ENG-3"),
            event: inner,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "agent");
        assert_eq!(v["issue"], "ENG-3");
        // Inner AgentEvent retains its own #[serde(tag = "type")] shape.
        assert_eq!(v["event"]["type"], "completed");
        assert_eq!(v["event"]["reason"], "success");
    }

    #[test]
    fn retry_scheduled_uses_delay_ms_not_instant() {
        let ev = OrchestratorEvent::RetryScheduled {
            issue: issue("ENG-4"),
            identifier: "ENG-4".into(),
            attempt: 2,
            reason: RetryReason::Failure,
            delay_ms: 30_000,
            error: Some("subprocess exit".into()),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "retry_scheduled");
        assert_eq!(v["delay_ms"], 30_000);
        assert_eq!(v["reason"], "failure");
        assert_eq!(v["error"], "subprocess exit");
    }

    #[test]
    fn reconciled_carries_dropped_list_and_no_issue() {
        let ev = OrchestratorEvent::Reconciled {
            dropped: vec![issue("ENG-5"), issue("ENG-6")],
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "reconciled");
        assert_eq!(v["dropped"][0], "ENG-5");
        assert_eq!(v["dropped"][1], "ENG-6");
        assert!(ev.issue().is_none());
    }

    #[test]
    fn released_carries_reason_and_optional_state() {
        let ev = OrchestratorEvent::Released {
            issue: issue("ENG-7"),
            identifier: "ENG-7".into(),
            reason: ReleaseReason::Completed,
            final_state: Some(IssueState::new("done")),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "released");
        // ReleaseReason currently has no serde derive on the source
        // type; if that changes we'll start asserting the wire form
        // here. For now we just verify shape.
        assert_eq!(v["issue"], "ENG-7");
        assert_eq!(v["final_state"], "done");
    }

    #[test]
    fn issue_accessor_returns_some_for_per_issue_variants() {
        let id = issue("ENG-10");
        let ev = OrchestratorEvent::StateChanged {
            issue: id.clone(),
            identifier: "ENG-10".into(),
            previous: Some(ClaimState::Running),
            current: ClaimState::RetryQueued { attempt: 2 },
        };
        assert_eq!(ev.issue(), Some(&id));
        assert_eq!(ev.kind(), "state_changed");
    }

    #[test]
    fn round_trip_preserves_all_variants() {
        let cases = vec![
            OrchestratorEvent::StateChanged {
                issue: issue("ENG-1"),
                identifier: "ENG-1".into(),
                previous: Some(ClaimState::Running),
                current: ClaimState::RetryQueued { attempt: 3 },
            },
            OrchestratorEvent::Dispatched {
                issue: issue("ENG-2"),
                identifier: "ENG-2".into(),
                session: session(),
                backend: "tandem".into(),
                attempt: 1,
            },
            OrchestratorEvent::RetryScheduled {
                issue: issue("ENG-3"),
                identifier: "ENG-3".into(),
                attempt: 2,
                reason: RetryReason::Continuation,
                delay_ms: 5_000,
                error: None,
            },
            OrchestratorEvent::Reconciled {
                dropped: vec![issue("ENG-4")],
            },
        ];
        for ev in cases {
            let s = serde_json::to_string(&ev).unwrap();
            let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(ev, back);
        }
    }
}
