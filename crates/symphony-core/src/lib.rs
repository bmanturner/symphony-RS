//! Symphony orchestrator core.
//!
//! This crate defines the *composition root* of Symphony-RS: the poll loop,
//! the per-issue state machine (SPEC §4.1.8), and the abstract trait surface
//! that decouples the orchestrator from any concrete tracker or agent
//! backend. Concrete adapters live in sibling crates (`symphony-tracker`,
//! `symphony-agent`, `symphony-workspace`).
//!
//! Architectural invariant: nothing in this crate may depend on
//! Linear/Codex/Claude protocol details. The orchestrator only sees the
//! normalized `Issue` model and the `AgentEvent` enum (defined in later
//! phases).

pub mod advisory;
pub mod agent;
pub mod blocker;
pub mod blocker_gate;
pub mod budget_exceeded;
pub mod budget_pause_runner;
pub mod budget_pause_tick;
pub mod cancellation;
pub mod cancellation_observer;
pub mod concurrency_gate;
pub mod decomposition;
pub mod decomposition_applier;
pub mod decomposition_runner;
pub mod event_bus;
pub mod events;
pub mod followup;
pub mod followup_approval_runner;
pub mod followup_approval_tick;
pub mod followup_policy;
pub mod followup_request;
pub mod followup_routing;
pub mod handoff;
pub mod intake_tick;
pub mod integration;
pub mod integration_request;
pub mod integration_runner;
pub mod integration_tick;
pub mod lease;
pub mod logical_queue;
pub mod parent_close;
pub mod poll_loop;
pub mod prompt;
pub mod pull_request;
pub mod qa;
pub mod qa_blocker;
pub mod qa_blocker_policy;
pub mod qa_request;
pub mod qa_rework;
pub mod qa_runner;
pub mod qa_tick;
pub mod queue_tick;
pub mod recovery_runner;
pub mod recovery_tick;
pub mod retry;
pub mod role;
pub mod routing;
pub mod run_lease;
pub mod run_status;
pub mod scheduler_v2;
pub mod scope_contention_broadcaster;
pub mod scope_contention_log;
pub mod specialist_runner;
pub mod specialist_tick;
pub mod state_machine;
pub mod tracker;
pub mod tracker_trait;
pub mod work_item;

pub use advisory::{AdvisoryMutations, AdvisoryRecord};
pub use agent::{
    AgentControl, AgentError, AgentEvent, AgentEventStream, AgentResult, AgentRunner, AgentSession,
    CompletionReason, RateLimitSnapshot, SessionId, StartSessionParams, ThreadId, TokenUsage,
    TurnId,
};
pub use blocker::{
    Blocker, BlockerError, BlockerId, BlockerOrigin, BlockerSeverity, BlockerStatus, RunRef,
};
pub use blocker_gate::{
    BlockerGateError, GateOperation, OpenBlockerSnapshot, check_no_open_blockers, collect_open,
};
pub use budget_exceeded::{
    BudgetExceededBridge, BudgetExceededEventLog, BudgetExceededEventSink,
    BudgetExceededObservation, BudgetExceededSinkError, MAX_RETRIES_BUDGET_KIND,
    RetryBudgetBridgeRequest,
};
pub use budget_pause_runner::{
    BudgetPauseDispatchOutcome, BudgetPauseDispatchReason, BudgetPauseDispatcher,
    BudgetPauseRunReport, BudgetPauseRunner,
};
pub use budget_pause_tick::{
    BudgetPauseCandidate, BudgetPauseDispatchQueue, BudgetPauseDispatchRequest, BudgetPauseId,
    BudgetPauseQueueError, BudgetPauseQueueSource, BudgetPauseQueueTick, BudgetPauseStatus,
};
pub use cancellation::{CancelRequest, CancelSubject, CancellationQueue};
pub use cancellation_observer::{CancellationDecision, observe_for_run, observe_for_run_or_parent};
pub use concurrency_gate::{
    ConcurrencyGate, DispatchTriple, RunnerScopes, Scope, ScopeAvailability, ScopeContended,
    ScopeKind, ScopePermitSet,
};
pub use decomposition::{
    ChildKey, ChildProposal, DecompositionError, DecompositionId, DecompositionProposal,
    DecompositionStatus,
};
pub use decomposition_applier::{
    AppliedChild, AppliedDecomposition, ApplyError, DecompositionApplier,
    DefaultDecompositionApplier, apply_decomposition,
};
pub use decomposition_runner::{
    ChildDraft, DecompositionContext, DecompositionDraft, DecompositionEligibility,
    DecompositionPolicy, DecompositionRunner, DecompositionTriggers, RunnerError, SkipReason,
    TriggerHit,
};
pub use event_bus::{DEFAULT_REPLAY_BUFFER, EventBus};
pub use events::OrchestratorEvent;
pub use followup::{
    FollowupError, FollowupId, FollowupIssueRequest, FollowupLink, FollowupPolicy, FollowupStatus,
};
pub use followup_approval_runner::{
    FollowupApprovalDispatchOutcome, FollowupApprovalDispatchReason, FollowupApprovalDispatcher,
    FollowupApprovalRunReport, FollowupApprovalRunner,
};
pub use followup_approval_tick::{
    FollowupApprovalCandidate, FollowupApprovalDispatchQueue, FollowupApprovalDispatchRequest,
    FollowupApprovalQueueError, FollowupApprovalQueueSource, FollowupApprovalQueueTick,
};
pub use followup_policy::{
    BlockingFollowupGateError, BlockingFollowupSnapshot, check_blocking_followups_at_parent_close,
};
pub use followup_request::{
    FollowupRequestError, FollowupRequestInput, FollowupSpec, derive_followups,
};
pub use followup_routing::{
    FollowupRouteDecision, FollowupRoutingError, route_followup, route_followups,
};
pub use handoff::{
    BranchOrWorkspace, Handoff, HandoffBlockerRequest, HandoffError, HandoffFollowupRequest,
    HandoffVerdictRequest, MalformedHandoff, MalformedHandoffDecision, MalformedHandoffPolicy,
    ReadyFor, ReadyForConsequence,
};
pub use intake_tick::{ActiveSetStore, IntakeQueueTick};
pub use integration::{
    IntegrationConflict, IntegrationError, IntegrationId, IntegrationMergeStrategy,
    IntegrationRecord, IntegrationStatus,
};
pub use integration_request::{
    IntegrationChild, IntegrationGates, IntegrationRequestCause, IntegrationRequestError,
    IntegrationRunRequest, IntegrationWorkspace,
};
pub use integration_runner::{
    IntegrationDispatchOutcome, IntegrationDispatchReason, IntegrationDispatchRunner,
    IntegrationDispatcher, IntegrationRunReport,
};
pub use integration_tick::{
    IntegrationCandidate, IntegrationDispatchQueue, IntegrationDispatchRequest,
    IntegrationQueueError, IntegrationQueueSource, IntegrationQueueTick,
};
pub use lease::{
    FixedLeaseClock, LeaseClock, LeaseConfig, LeaseOwner, LeaseOwnerParseError, LeaseTimestamps,
};
pub use logical_queue::{LogicalQueue, QueueTickOutcome};
pub use parent_close::{ChildSnapshot, ParentCloseError, check_parent_can_close};
pub use poll_loop::{Dispatcher, PollLoop, PollLoopConfig, TickReport};
pub use prompt::{
    PromptBlocker, PromptChild, PromptContext, PromptIssue, PromptParent, PromptWorkspace,
    RenderError, default_handoff_output_schema, render as render_prompt,
};
pub use pull_request::{
    PullRequestProvider, PullRequestRecord, PullRequestRecordError, PullRequestRecordId,
    PullRequestState,
};
pub use qa::{
    AcceptanceCriterionStatus, AcceptanceCriterionTrace, QaError, QaEvidence, QaOutcome, QaVerdict,
    QaVerdictId,
};
pub use qa_blocker::{QaBlockerError, QaBlockerSpec, derive_qa_blockers};
pub use qa_blocker_policy::{
    QaBlockerPolicy, QaBlockerSnapshot, QaParentCloseError, QaWaiverError,
    check_qa_blockers_at_parent_close, validate_qa_waiver,
};
pub use qa_request::{
    QaDraftPullRequest, QaRequestCause, QaRequestError, QaRunRequest, QaWorkspace,
};
pub use qa_rework::{
    BlockerReworkInput, BlockerReworkRoute, PriorRunSummary, QaReworkDecision, QaReworkError,
    derive_qa_rework_routing,
};
pub use qa_runner::{
    QaDispatchOutcome, QaDispatchReason, QaDispatchRunner, QaDispatcher, QaRunReport,
};
pub use qa_tick::{
    QaCandidate, QaDispatchQueue, QaDispatchRequest, QaGates, QaQueueError, QaQueueSource,
    QaQueueTick,
};
pub use queue_tick::{QueueTick, QueueTickCadence, ScriptedQueueTick, run_queue_tick_n};
pub use recovery_runner::{
    RecoveryDispatchOutcome, RecoveryDispatchReason, RecoveryDispatchTarget, RecoveryDispatcher,
    RecoveryRunReport, RecoveryRunner,
};
pub use recovery_tick::{
    ExpiredLeaseCandidate, OrphanedWorkspaceClaimCandidate, RecoveryDispatchQueue,
    RecoveryDispatchRequest, RecoveryQueueError, RecoveryQueueSource, RecoveryQueueTick,
    RecoveryRunId, RecoveryWorkspaceClaimId,
};
pub use retry::{RetryConfig, RetryEntry, RetryQueue, RetryReason, ScheduleRequest, backoff_for};
pub use role::{RoleAuthority, RoleAuthorityOverrides, RoleContext, RoleKind, RoleName};
pub use routing::{
    RoutingContext, RoutingDecision, RoutingEngine, RoutingError, RoutingMatch, RoutingMatchMode,
    RoutingRule, RoutingTable,
};
pub use run_lease::{
    InMemoryRunLeaseStore, LeaseAcquireOutcome, RunLeaseError, RunLeaseGuard, RunLeaseStore,
};
pub use run_status::RunStatus;
pub use scheduler_v2::{SchedulerTickReport, SchedulerV2, SchedulerV2Config};
pub use scope_contention_broadcaster::{
    ScopeContentionEventBroadcaster, ScopeContentionEventSink, ScopeContentionSinkError,
    ScopeFields,
};
pub use scope_contention_log::{ContentionObservation, ContentionSubject, ScopeContentionEventLog};
pub use specialist_runner::{SpecialistDispatchOutcome, SpecialistRunReport, SpecialistRunner};
pub use specialist_tick::{
    RoleKindLookup, SpecialistDispatchQueue, SpecialistDispatchRequest, SpecialistQueueTick,
};
pub use state_machine::{ClaimState, ReleaseReason, StateMachine, TransitionError};
pub use tracker::{BlockerRef, Issue, IssueId, IssueState};
pub use tracker_trait::{
    AddBlockerRequest, AddBlockerResponse, AddCommentRequest, AddCommentResponse, ArtifactKind,
    AttachArtifactRequest, AttachArtifactResponse, CreateIssueRequest, CreateIssueResponse,
    LinkParentChildRequest, LinkParentChildResponse, TrackerCapabilities, TrackerError,
    TrackerMutations, TrackerRead, TrackerResult, UpdateIssueRequest, UpdateIssueResponse,
};
pub use work_item::{
    ClassifyError, StatusClassifier, TrackerStatus, UnknownStatePolicy, WorkItem, WorkItemId,
    WorkItemStatusClass,
};
