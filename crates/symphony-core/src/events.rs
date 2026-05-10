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
use crate::concurrency_gate::ScopeKind;
use crate::retry::RetryReason;
use crate::state_machine::{ClaimState, ReleaseReason};
use crate::tracker::{IssueId, IssueState};
use crate::work_item::{WorkItemId, WorkItemStatusClass};

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

    /// A dispatch could not run because at least one
    /// [`crate::concurrency_gate::ConcurrencyGate`] scope was at cap.
    ///
    /// Emitted exactly once per *contention episode* — defined as the
    /// span over which a given dispatch keeps re-deferring against the
    /// same `(scope_kind, scope_key)` tuple. A request that contends on
    /// the same scope across ten consecutive scheduler ticks emits one
    /// `ScopeCapReached`, not ten. When the request finally dispatches
    /// (or the scope key changes) the episode ends; a fresh contention
    /// against the same scope after the episode ends is a new episode
    /// and emits again.
    ///
    /// Persisted by the orchestrator alongside broadcast so an operator
    /// can answer "why is this issue not running" from event history.
    /// The dedup primitive lives in
    /// [`crate::scope_contention_log::ScopeContentionEventLog`].
    ScopeCapReached {
        /// Durable run row the contended dispatch is associated with,
        /// when the runner has reserved one. `None` for runners whose
        /// dispatch identity is not a `runs.id` (today: follow-up
        /// approval, budget pause).
        #[serde(default)]
        run_id: Option<i64>,
        /// Tracker-facing identifier of the parked dispatch, when the
        /// runner carries one. `None` when the runner identifies
        /// dispatches purely by an internal id (e.g. `FollowupId`).
        #[serde(default)]
        identifier: Option<String>,
        /// Which scope kind hit cap.
        scope_kind: ScopeKind,
        /// Key of the contended scope (role / agent profile / repo
        /// slug, or `""` for [`ScopeKind::Global`]).
        scope_key: String,
        /// In-flight permits on the contended scope at rejection.
        in_flight: u32,
        /// Configured cap on the contended scope.
        cap: u32,
    },

    /// A budget cap was exceeded — the orchestrator must pause work
    /// rather than dispatch.
    ///
    /// Today the only emitter is the retry-cap policy
    /// ([`crate::retry`]): when an issue's `attempt` exceeds the
    /// configured `budgets.max_retries`, the scheduler emits this
    /// event instead of inserting another [`crate::retry::RetryEntry`]
    /// and enqueues a durable [`crate::budget_pause_runner`] dispatch.
    /// Future budget kinds (cost-per-issue, turns-per-run, etc.) reuse
    /// the same variant with a different `budget_kind` discriminator.
    ///
    /// Like `ScopeCapReached` this is a *policy* event, not a
    /// dispatched run, so it carries no agent session and is exempt
    /// from any future `expects_lease()` predicate. Per-episode dedup
    /// (so a long stream of cap-exceeded ticks emits one event, not
    /// one per tick) is the broadcaster's responsibility.
    BudgetExceeded {
        /// Durable run row that tripped the cap, when the failure
        /// path reserved one. `None` for continuation-driven caps and
        /// for callers that key purely on `identifier`. Wire shape
        /// matches `ScopeCapReached.run_id` (transparent integer);
        /// see [`crate::blocker::RunRef`].
        #[serde(default)]
        run_id: Option<i64>,
        /// Tracker-facing identifier of the work item the cap pertains
        /// to (e.g. an issue id). Always present — budget pauses are
        /// keyed on `(budget_kind, identifier)` in the budget-pause
        /// queue, so the wire mirrors that key.
        identifier: String,
        /// Stable string discriminator for the cap that tripped (e.g.
        /// `"max_retries"`, `"max_cost_per_issue_usd"`,
        /// `"max_turns_per_run"`). Free-form so adding a budget kind
        /// is non-breaking; consumers route on this field.
        budget_kind: String,
        /// Observed value at the moment of exceedance, in the cap's
        /// natural units (attempts for `max_retries`, USD for
        /// `max_cost_per_issue_usd`, turns for `max_turns_per_run`).
        /// Encoded as `f64` so heterogeneous units share one shape.
        observed: f64,
        /// Configured cap, in the same units as `observed`.
        cap: f64,
    },

    /// A run was cancelled — emitted once per `runs.id` lifecycle.
    ///
    /// Fired on the leading edge of a run's cancellation: the kernel
    /// observes a pending [`crate::cancellation::CancelRequest`] for the
    /// run (or cascades one in via
    /// [`crate::cancellation_propagator::CancellationPropagator`]) and
    /// transitions the run toward [`crate::run_status::RunStatus::Cancelled`].
    /// Per-run dedup lives in
    /// [`crate::cancellation_event_log::CancellationEventLog`]: a run
    /// cannot cancel twice, so the log emits the first observation and
    /// suppresses repeats.
    ///
    /// The wire fields mirror the shape of `CancelRequest` plus the
    /// durable run row id so consumers can correlate without joining
    /// against the `cancel_requests` table. `identifier` is best-effort
    /// — operator-issued cancels at the CLI carry a tracker id, while a
    /// cascade from a parent work item may not. The `subject` field
    /// preserves whether the cancel originated at the run level or
    /// cascaded from a work-item-level request.
    /// A v2 work item moved between [`WorkItemStatusClass`] values.
    ///
    /// This is the **spine event** for Phase 13 scenario consumers:
    /// every deterministic v2 scenario asserts that a work item moved
    /// through a specific status sequence (e.g. `intake → integration →
    /// qa → done` for the happy path, or `qa → rework → qa → done` for
    /// the QA-blocker rework path). Test consumers filter on
    /// `kind() == "work_item_status_changed"` and pattern-match on the
    /// `previous` / `current` pair to drive their assertions, rather
    /// than polling the durable `work_items` table.
    ///
    /// Distinct from [`OrchestratorEvent::StateChanged`]: that variant
    /// carries [`ClaimState`] (in-flight claim ledger transitions for the
    /// v1 poll loop). This variant carries the *normalized* v2 work-item
    /// status class derived from tracker state per workflow config.
    /// Both can fire for the same wall-clock event when a v2 work item
    /// is also represented as a v1 claim — they observe different
    /// projections of the same underlying lifecycle.
    ///
    /// `previous` is `None` for the initial observation of a work item
    /// (e.g. when intake first imports it from the tracker). The
    /// `identifier` is best-effort — it is `Some` whenever the kernel
    /// has a tracker-facing id snapshot, and `None` for synthetic work
    /// items that exist only in durable state.
    ///
    /// `raw_state` is the tracker's untranslated status string (e.g.
    /// `"In Progress"`, `"Ready For Review"`). It is preserved on the
    /// wire so a status surface can render the operator-facing label
    /// without round-tripping back through workflow config.
    WorkItemStatusChanged {
        /// Durable work-item row id whose status class changed.
        work_item_id: WorkItemId,
        /// Tracker-facing identifier snapshot, when known.
        #[serde(default)]
        identifier: Option<String>,
        /// Status class immediately before the transition; `None` for
        /// the very first observation of a work item.
        #[serde(default)]
        previous: Option<WorkItemStatusClass>,
        /// Status class immediately after the transition.
        current: WorkItemStatusClass,
        /// Raw tracker state string the `current` class was derived
        /// from, when known.
        #[serde(default)]
        raw_state: Option<String>,
    },

    RunCancelled {
        /// Durable run row that cancelled.
        run_id: i64,
        /// Tracker-facing identifier of the work item the run is
        /// against, when known. `None` when the cancel originated from
        /// a propagation step that did not snapshot the identifier.
        #[serde(default)]
        identifier: Option<String>,
        /// Originating subject of the cancel (run-targeted vs.
        /// work-item-targeted cascade). Mirrors the durable
        /// `cancel_requests.subject_kind` so consumers can distinguish
        /// "operator cancelled this dispatch" from "operator cancelled
        /// the whole work item and this child got swept along".
        subject: crate::cancellation::CancelSubject,
        /// Operator-supplied reason from the originating
        /// [`crate::cancellation::CancelRequest`].
        reason: String,
        /// Identity that requested the cancel (e.g. `$USER` from the
        /// CLI, or `"cascade:work_item:42"` for a propagated child
        /// cancel).
        requested_by: String,
        /// RFC3339-style timestamp the cancel was first requested at.
        /// Opaque text — the kernel does not parse it.
        requested_at: String,
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
            OrchestratorEvent::Reconciled { .. }
            | OrchestratorEvent::ScopeCapReached { .. }
            | OrchestratorEvent::BudgetExceeded { .. }
            | OrchestratorEvent::RunCancelled { .. }
            | OrchestratorEvent::WorkItemStatusChanged { .. } => None,
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
            OrchestratorEvent::ScopeCapReached { .. } => "scope_cap_reached",
            OrchestratorEvent::BudgetExceeded { .. } => "budget_exceeded",
            OrchestratorEvent::RunCancelled { .. } => "run_cancelled",
            OrchestratorEvent::WorkItemStatusChanged { .. } => "work_item_status_changed",
        }
    }

    /// Borrow the durable [`WorkItemId`] for variants that carry one.
    ///
    /// Used by test consumers (Phase 13) to route observed events into
    /// per-work-item assertion lists without inspecting nested fields.
    /// Returns `None` for variants that pertain to a v1 claim, a run,
    /// or a multi-issue summary.
    #[must_use]
    pub fn work_item_id(&self) -> Option<WorkItemId> {
        match self {
            OrchestratorEvent::WorkItemStatusChanged { work_item_id, .. } => Some(*work_item_id),
            _ => None,
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
    fn scope_cap_reached_serialises_with_type_tag_and_snake_case_kind() {
        let ev = OrchestratorEvent::ScopeCapReached {
            run_id: Some(42),
            identifier: Some("ENG-9".into()),
            scope_kind: ScopeKind::AgentProfile,
            scope_key: "claude-default".into(),
            in_flight: 3,
            cap: 3,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "scope_cap_reached");
        assert_eq!(v["run_id"], 42);
        assert_eq!(v["identifier"], "ENG-9");
        assert_eq!(v["scope_kind"], "agent_profile");
        assert_eq!(v["scope_key"], "claude-default");
        assert_eq!(v["in_flight"], 3);
        assert_eq!(v["cap"], 3);
        assert!(ev.issue().is_none());
        assert_eq!(ev.kind(), "scope_cap_reached");
    }

    #[test]
    fn scope_cap_reached_carries_optional_run_id_and_identifier() {
        // Followup-approval / budget-pause runners don't carry a
        // run_id+identifier pair; the wire format must accept absences.
        let ev = OrchestratorEvent::ScopeCapReached {
            run_id: None,
            identifier: None,
            scope_kind: ScopeKind::Global,
            scope_key: "".into(),
            in_flight: 5,
            cap: 5,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn scope_cap_reached_deserialises_with_default_optional_fields() {
        // Future emitters may omit run_id / identifier entirely. The
        // serde defaults must accept that without breaking decode.
        let raw = r#"{"type":"scope_cap_reached","scope_kind":"role","scope_key":"qa","in_flight":2,"cap":2}"#;
        let ev: OrchestratorEvent = serde_json::from_str(raw).unwrap();
        match ev {
            OrchestratorEvent::ScopeCapReached {
                run_id,
                identifier,
                scope_kind,
                scope_key,
                in_flight,
                cap,
            } => {
                assert!(run_id.is_none());
                assert!(identifier.is_none());
                assert_eq!(scope_kind, ScopeKind::Role);
                assert_eq!(scope_key, "qa");
                assert_eq!(in_flight, 2);
                assert_eq!(cap, 2);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
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
            OrchestratorEvent::ScopeCapReached {
                run_id: Some(7),
                identifier: Some("ENG-5".into()),
                scope_kind: ScopeKind::Repository,
                scope_key: "foglet/symphony".into(),
                in_flight: 1,
                cap: 1,
            },
            OrchestratorEvent::BudgetExceeded {
                run_id: Some(11),
                identifier: "ENG-6".into(),
                budget_kind: "max_retries".into(),
                observed: 4.0,
                cap: 3.0,
            },
            OrchestratorEvent::BudgetExceeded {
                run_id: None,
                identifier: "ENG-7".into(),
                budget_kind: "max_cost_per_issue_usd".into(),
                observed: 12.5,
                cap: 10.0,
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: WorkItemId::new(8),
                identifier: Some("ENG-8".into()),
                previous: Some(WorkItemStatusClass::Qa),
                current: WorkItemStatusClass::Rework,
                raw_state: Some("Rework".into()),
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: WorkItemId::new(9),
                identifier: None,
                previous: None,
                current: WorkItemStatusClass::Intake,
                raw_state: None,
            },
        ];
        for ev in cases {
            let s = serde_json::to_string(&ev).unwrap();
            let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
            assert_eq!(ev, back);
        }
    }

    #[test]
    fn budget_exceeded_serialises_with_type_tag_and_snake_case_kind() {
        // Failure-driven cap: a specific run row tripped the cap.
        let ev = OrchestratorEvent::BudgetExceeded {
            run_id: Some(42),
            identifier: "ENG-9".into(),
            budget_kind: "max_retries".into(),
            observed: 4.0,
            cap: 3.0,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "budget_exceeded");
        assert_eq!(v["run_id"], 42);
        assert_eq!(v["identifier"], "ENG-9");
        assert_eq!(v["budget_kind"], "max_retries");
        assert_eq!(v["observed"], 4.0);
        assert_eq!(v["cap"], 3.0);
        assert!(ev.issue().is_none());
        assert_eq!(ev.kind(), "budget_exceeded");
    }

    #[test]
    fn budget_exceeded_carries_optional_run_id_for_continuation_caps() {
        // Continuation caps (e.g. cost-per-issue, turns-per-run) are
        // not associated with a specific failed run row, so run_id is
        // absent. Identifier remains required.
        let ev = OrchestratorEvent::BudgetExceeded {
            run_id: None,
            identifier: "ENG-10".into(),
            budget_kind: "max_cost_per_issue_usd".into(),
            observed: 12.50,
            cap: 10.00,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn run_cancelled_serialises_with_type_tag_and_subject_discriminator() {
        use crate::blocker::RunRef;
        use crate::cancellation::CancelSubject;

        let ev = OrchestratorEvent::RunCancelled {
            run_id: 42,
            identifier: Some("ENG-9".into()),
            subject: CancelSubject::run(RunRef::new(42)),
            reason: "operator cancel".into(),
            requested_by: "alice".into(),
            requested_at: "2026-05-09T00:00:00Z".into(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "run_cancelled");
        assert_eq!(v["run_id"], 42);
        assert_eq!(v["identifier"], "ENG-9");
        assert_eq!(v["subject"]["kind"], "run");
        assert_eq!(v["subject"]["run_id"], 42);
        assert_eq!(v["reason"], "operator cancel");
        assert_eq!(v["requested_by"], "alice");
        assert_eq!(v["requested_at"], "2026-05-09T00:00:00Z");
        assert!(ev.issue().is_none());
        assert_eq!(ev.kind(), "run_cancelled");
    }

    #[test]
    fn run_cancelled_round_trips_for_work_item_cascade_subject() {
        use crate::cancellation::CancelSubject;
        use crate::work_item::WorkItemId;

        let ev = OrchestratorEvent::RunCancelled {
            run_id: 7,
            identifier: None,
            subject: CancelSubject::work_item(WorkItemId::new(11)),
            reason: "parent cancelled".into(),
            requested_by: "cascade:work_item:11".into(),
            requested_at: "2026-05-09T00:00:00Z".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn run_cancelled_deserialises_with_default_identifier() {
        let raw = r#"{"type":"run_cancelled","run_id":3,"subject":{"kind":"run","run_id":3},"reason":"r","requested_by":"u","requested_at":"2026-05-09T00:00:00Z"}"#;
        let ev: OrchestratorEvent = serde_json::from_str(raw).unwrap();
        match ev {
            OrchestratorEvent::RunCancelled {
                run_id,
                identifier,
                reason,
                requested_by,
                ..
            } => {
                assert_eq!(run_id, 3);
                assert!(identifier.is_none());
                assert_eq!(reason, "r");
                assert_eq!(requested_by, "u");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn work_item_status_changed_serialises_with_type_tag_and_snake_case_classes() {
        let ev = OrchestratorEvent::WorkItemStatusChanged {
            work_item_id: WorkItemId::new(42),
            identifier: Some("ENG-42".into()),
            previous: Some(WorkItemStatusClass::Intake),
            current: WorkItemStatusClass::Integration,
            raw_state: Some("Ready for Review".into()),
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "work_item_status_changed");
        assert_eq!(v["work_item_id"], 42);
        assert_eq!(v["identifier"], "ENG-42");
        assert_eq!(v["previous"], "intake");
        assert_eq!(v["current"], "integration");
        assert_eq!(v["raw_state"], "Ready for Review");
        assert!(ev.issue().is_none());
        assert_eq!(ev.work_item_id(), Some(WorkItemId::new(42)));
        assert_eq!(ev.kind(), "work_item_status_changed");
    }

    #[test]
    fn work_item_status_changed_initial_observation_has_null_previous() {
        // First observation of a work item (intake import): no
        // previous status class to compare against.
        let ev = OrchestratorEvent::WorkItemStatusChanged {
            work_item_id: WorkItemId::new(7),
            identifier: None,
            previous: None,
            current: WorkItemStatusClass::Intake,
            raw_state: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v["previous"].is_null());
        assert!(v["identifier"].is_null());
        assert!(v["raw_state"].is_null());
        let s = serde_json::to_string(&ev).unwrap();
        let back: OrchestratorEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
    }

    #[test]
    fn work_item_status_changed_deserialises_with_default_optional_fields() {
        // Future emitters may omit identifier/previous/raw_state. The
        // serde defaults must accept that without breaking decode.
        let raw = r#"{"type":"work_item_status_changed","work_item_id":11,"current":"qa"}"#;
        let ev: OrchestratorEvent = serde_json::from_str(raw).unwrap();
        match ev {
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id,
                identifier,
                previous,
                current,
                raw_state,
            } => {
                assert_eq!(work_item_id, WorkItemId::new(11));
                assert!(identifier.is_none());
                assert!(previous.is_none());
                assert_eq!(current, WorkItemStatusClass::Qa);
                assert!(raw_state.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn work_item_status_changed_test_consumer_filters_per_item_transition_sequence() {
        // Demonstrates the Phase 13 "test consumer" pattern: a
        // deterministic scenario emits a stream of events; the consumer
        // filters by kind, joins by work_item_id(), and asserts the
        // transition sequence for each work item independently. The
        // variant exists *to make this assertion pattern feasible*;
        // pinning the pattern here keeps it from drifting when Phase 13
        // wires real scenarios.
        let parent = WorkItemId::new(100);
        let child_a = WorkItemId::new(101);
        let child_b = WorkItemId::new(102);

        let stream = vec![
            // Intake imports the parent.
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: parent,
                identifier: Some("ENG-100".into()),
                previous: None,
                current: WorkItemStatusClass::Intake,
                raw_state: Some("Triage".into()),
            },
            // Integration owner decomposes; children created at intake.
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: child_a,
                identifier: Some("ENG-101".into()),
                previous: None,
                current: WorkItemStatusClass::Ready,
                raw_state: Some("Ready".into()),
            },
            // An unrelated event we expect the consumer to ignore.
            OrchestratorEvent::Reconciled { dropped: vec![] },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: child_b,
                identifier: Some("ENG-102".into()),
                previous: None,
                current: WorkItemStatusClass::Ready,
                raw_state: Some("Ready".into()),
            },
            // Children complete; parent reaches integration → qa → done.
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: child_a,
                identifier: Some("ENG-101".into()),
                previous: Some(WorkItemStatusClass::Ready),
                current: WorkItemStatusClass::Done,
                raw_state: Some("Done".into()),
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: child_b,
                identifier: Some("ENG-102".into()),
                previous: Some(WorkItemStatusClass::Ready),
                current: WorkItemStatusClass::Done,
                raw_state: Some("Done".into()),
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: parent,
                identifier: Some("ENG-100".into()),
                previous: Some(WorkItemStatusClass::Intake),
                current: WorkItemStatusClass::Integration,
                raw_state: Some("Integrating".into()),
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: parent,
                identifier: Some("ENG-100".into()),
                previous: Some(WorkItemStatusClass::Integration),
                current: WorkItemStatusClass::Qa,
                raw_state: Some("In QA".into()),
            },
            OrchestratorEvent::WorkItemStatusChanged {
                work_item_id: parent,
                identifier: Some("ENG-100".into()),
                previous: Some(WorkItemStatusClass::Qa),
                current: WorkItemStatusClass::Done,
                raw_state: Some("Done".into()),
            },
        ];

        let transitions_for =
            |id: WorkItemId| -> Vec<(Option<WorkItemStatusClass>, WorkItemStatusClass)> {
                stream
                    .iter()
                    .filter(|ev| ev.kind() == "work_item_status_changed")
                    .filter(|ev| ev.work_item_id() == Some(id))
                    .map(|ev| match ev {
                        OrchestratorEvent::WorkItemStatusChanged {
                            previous, current, ..
                        } => (*previous, *current),
                        _ => unreachable!(),
                    })
                    .collect()
            };

        assert_eq!(
            transitions_for(parent),
            vec![
                (None, WorkItemStatusClass::Intake),
                (
                    Some(WorkItemStatusClass::Intake),
                    WorkItemStatusClass::Integration
                ),
                (
                    Some(WorkItemStatusClass::Integration),
                    WorkItemStatusClass::Qa
                ),
                (Some(WorkItemStatusClass::Qa), WorkItemStatusClass::Done),
            ]
        );
        assert_eq!(
            transitions_for(child_a),
            vec![
                (None, WorkItemStatusClass::Ready),
                (Some(WorkItemStatusClass::Ready), WorkItemStatusClass::Done),
            ]
        );
        assert_eq!(
            transitions_for(child_b),
            vec![
                (None, WorkItemStatusClass::Ready),
                (Some(WorkItemStatusClass::Ready), WorkItemStatusClass::Done),
            ]
        );
    }

    #[test]
    fn budget_exceeded_deserialises_with_default_run_id() {
        // Future emitters may omit run_id entirely. The serde default
        // must accept that without breaking decode.
        let raw = r#"{"type":"budget_exceeded","identifier":"ENG-11","budget_kind":"max_turns_per_run","observed":21.0,"cap":20.0}"#;
        let ev: OrchestratorEvent = serde_json::from_str(raw).unwrap();
        match ev {
            OrchestratorEvent::BudgetExceeded {
                run_id,
                identifier,
                budget_kind,
                observed,
                cap,
            } => {
                assert!(run_id.is_none());
                assert_eq!(identifier, "ENG-11");
                assert_eq!(budget_kind, "max_turns_per_run");
                assert!((observed - 21.0).abs() < f64::EPSILON);
                assert!((cap - 20.0).abs() < f64::EPSILON);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
