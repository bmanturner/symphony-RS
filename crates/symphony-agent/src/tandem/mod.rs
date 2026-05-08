//! Tandem runner — wraps two inner [`AgentRunner`]s and exposes them as a
//! single composite runner.
//!
//! This module is SPEC-deviation #1: where upstream Symphony assumes one
//! coding agent per run, Symphony-RS lets a single dispatch drive two
//! agents with a configurable lead/follower relationship. This file
//! delivers the *shape* — the [`TandemStrategy`] / [`TandemRole`] enums
//! and the [`TandemRunner`] composition root — and dispatches into one
//! of three submodules for the actual behaviour.
//!
//! # Strategy dispatch
//!
//! - [`TandemStrategy::DraftReview`]: lead drafts, follower reviews
//!   (`draft_review` submodule). Sequential — follower starts only
//!   after the lead's draft is captured.
//! - [`TandemStrategy::SplitImplement`]: lead plans, follower executes
//!   subtasks it claims via tool-use (`split_implement` submodule).
//!   Also sequential.
//! - [`TandemStrategy::Consensus`]: both run in parallel; the
//!   orchestrator picks the output with the greater test-pass delta
//!   (`consensus` submodule). The scoring function is pluggable via
//!   [`TandemRunner::with_consensus_scorer`] with a heuristic default.
//!
//! All three strategies expose the same `(lead, follower, params) ->
//! AgentSession` shape, suppress per-stream terminal events, and emit
//! exactly one synthesised terminal tagged with the lead session id. The
//! orchestrator therefore sees a Tandem session as just another agent.

mod consensus;
mod draft_review;
mod split_implement;

pub use consensus::{ConsensusScorer, default_consensus_scorer};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use symphony_core::agent::{AgentResult, AgentRunner, AgentSession, StartSessionParams};

// ---------------------------------------------------------------------------
// Configuration enums
// ---------------------------------------------------------------------------

/// Which of the three SPEC-deviation strategies the runner should use.
///
/// Serialized as kebab-case to match the planned `WORKFLOW.md` syntax
/// (`tandem.strategy = "draft-review"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TandemStrategy {
    /// Lead drafts the turn, follower reviews it; the orchestrator either
    /// accepts the draft or runs a second turn with the merged feedback.
    DraftReview,
    /// Lead plans, follower executes specific subtasks it claims via the
    /// tool-use channel.
    SplitImplement,
    /// Both run in parallel; the orchestrator picks the output with the
    /// greater test-pass delta. The scoring function is pluggable via
    /// [`TandemRunner::with_consensus_scorer`] (default counts test-pass
    /// phrasing and test-tool invocations). Failure on either side is a
    /// forfeit; ties break toward the lead.
    Consensus,
}

/// Which inner runner played which role in a given turn.
///
/// Carried in telemetry (Phase 6's per-agent token counts and agreement
/// rate) and used by the Phase 8 status surface to label a tandem panel.
/// Not currently embedded in `AgentEvent` — strategy-specific commits
/// may add a tandem-tagged variant later if and when the orchestrator
/// needs to demultiplex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TandemRole {
    /// First-mover for the strategy: drafts in `draft-review`, plans in
    /// `split-implement`, parallel candidate A in `consensus`.
    Lead,
    /// Second seat: reviews / executes / parallel candidate B.
    Follower,
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Composite [`AgentRunner`] holding a lead and a follower runner plus a
/// [`TandemStrategy`].
///
/// `Arc<dyn AgentRunner>` is the storage type so the same backend can be
/// used in both seats (e.g. `claude` lead + `claude` follower with
/// different system prompts) and so the orchestrator can keep its
/// `Arc<dyn AgentRunner>` composition root unchanged.
pub struct TandemRunner {
    lead: Arc<dyn AgentRunner>,
    follower: Arc<dyn AgentRunner>,
    strategy: TandemStrategy,
    /// Scorer used by [`TandemStrategy::Consensus`] to break ties between
    /// the two candidate transcripts. Lazily defaulted via
    /// [`default_consensus_scorer`] when `None`. Stored as an `Option`
    /// rather than always-eager-defaulting so that constructing a
    /// `TandemRunner` for a non-consensus strategy doesn't allocate a
    /// scoring closure that will never be called.
    consensus_scorer: Option<ConsensusScorer>,
}

impl TandemRunner {
    /// Construct from two inner runners and a strategy. The first
    /// argument is the lead by convention — strategies that distinguish
    /// the seats branch on which `Arc` was passed where.
    pub fn new(
        lead: Arc<dyn AgentRunner>,
        follower: Arc<dyn AgentRunner>,
        strategy: TandemStrategy,
    ) -> Self {
        Self {
            lead,
            follower,
            strategy,
            consensus_scorer: None,
        }
    }

    /// Override the consensus scorer. Only affects [`TandemStrategy::Consensus`];
    /// silently ignored under other strategies but stored verbatim so a
    /// caller can flip strategies without re-injecting the scorer.
    ///
    /// Builder-style for ergonomic composition at the orchestrator's
    /// composition root, where a `WORKFLOW.md`-driven scoring policy is
    /// looked up once and applied to every freshly constructed runner.
    pub fn with_consensus_scorer(mut self, scorer: ConsensusScorer) -> Self {
        self.consensus_scorer = Some(scorer);
        self
    }

    /// Borrow the configured strategy. Exposed for status-surface and
    /// telemetry consumers; the runner's own behaviour reads it
    /// directly.
    pub fn strategy(&self) -> TandemStrategy {
        self.strategy
    }
}

#[async_trait]
impl AgentRunner for TandemRunner {
    async fn start_session(&self, params: StartSessionParams) -> AgentResult<AgentSession> {
        // Each strategy lives in its own submodule. The orchestrator
        // doesn't know or care which one it's talking to — all three
        // expose the same `(lead, follower, params) -> AgentSession`
        // shape and emit normalized `AgentEvent`s.
        match self.strategy {
            TandemStrategy::DraftReview => {
                draft_review::start(self.lead.clone(), self.follower.clone(), params).await
            }
            TandemStrategy::SplitImplement => {
                split_implement::start(self.lead.clone(), self.follower.clone(), params).await
            }
            TandemStrategy::Consensus => {
                let scorer = self
                    .consensus_scorer
                    .clone()
                    .unwrap_or_else(default_consensus_scorer);
                consensus::start(self.lead.clone(), self.follower.clone(), params, scorer).await
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use symphony_core::agent::{
        AgentControl, AgentError, AgentEvent, AgentSession, CompletionReason, SessionId,
        StartSessionParams,
    };
    use symphony_core::tracker::IssueId;

    /// Programmable stub runner: returns a pre-seeded event vector and
    /// records `start_session` / `abort` / `continue_turn` calls.
    struct StubRunner {
        session_id: SessionId,
        events: Mutex<Option<Vec<AgentEvent>>>,
        start_calls: AtomicUsize,
        fail_start: bool,
    }

    impl StubRunner {
        fn new(session_id: &str, events: Vec<AgentEvent>) -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw(session_id),
                events: Mutex::new(Some(events)),
                start_calls: AtomicUsize::new(0),
                fail_start: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                session_id: SessionId::from_raw("never"),
                events: Mutex::new(Some(vec![])),
                start_calls: AtomicUsize::new(0),
                fail_start: true,
            })
        }
    }

    struct StubControl {
        continue_calls: Arc<AtomicUsize>,
        abort_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentControl for StubControl {
        async fn continue_turn(&self, _guidance: &str) -> AgentResult<()> {
            self.continue_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn abort(&self) -> AgentResult<()> {
            self.abort_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl AgentRunner for StubRunner {
        async fn start_session(&self, _params: StartSessionParams) -> AgentResult<AgentSession> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_start {
                return Err(AgentError::Spawn("stubbed failure".into()));
            }
            let evs = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(AgentSession {
                initial_session: self.session_id.clone(),
                events: Box::pin(stream::iter(evs)),
                control: Box::new(StubControl {
                    continue_calls: Arc::new(AtomicUsize::new(0)),
                    abort_calls: Arc::new(AtomicUsize::new(0)),
                }),
            })
        }
    }

    fn msg(session: &str, content: &str) -> AgentEvent {
        AgentEvent::Message {
            session: SessionId::from_raw(session),
            role: "assistant".into(),
            content: content.into(),
        }
    }
    fn done(session: &str) -> AgentEvent {
        AgentEvent::Completed {
            session: SessionId::from_raw(session),
            reason: CompletionReason::Success,
        }
    }

    fn params() -> StartSessionParams {
        StartSessionParams {
            issue: IssueId::new("X-1"),
            workspace: std::path::PathBuf::from("/tmp/ws"),
            initial_prompt: "go".into(),
            issue_label: None,
        }
    }

    #[test]
    fn strategy_round_trips_kebab_case() {
        for (variant, expected) in [
            (TandemStrategy::DraftReview, "\"draft-review\""),
            (TandemStrategy::SplitImplement, "\"split-implement\""),
            (TandemStrategy::Consensus, "\"consensus\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected);
            let back: TandemStrategy = serde_json::from_str(&s).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn role_round_trips_lowercase() {
        assert_eq!(
            serde_json::to_string(&TandemRole::Lead).unwrap(),
            "\"lead\""
        );
        assert_eq!(
            serde_json::to_string(&TandemRole::Follower).unwrap(),
            "\"follower\""
        );
    }

    #[tokio::test]
    async fn starts_both_inner_runners_in_order() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::new("foll-1", vec![done("foll-1")]);
        let runner = TandemRunner::new(
            lead.clone() as Arc<dyn AgentRunner>,
            follower.clone() as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let session = runner.start_session(params()).await.expect("start ok");
        assert_eq!(session.initial_session.as_str(), "lead-1");
        assert_eq!(lead.start_calls.load(Ordering::SeqCst), 1);
        assert_eq!(follower.start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn consensus_dispatch_emits_single_terminal_tagged_with_lead_session() {
        // Whichever side wins the consensus vote, the orchestrator must
        // see exactly one terminal tagged with the lead session id. Both
        // sides score 0 here (no test-pass phrasing) → lead wins by the
        // tie-break rule, so its messages replay and the follower's are
        // discarded. The detailed scoring/forfeit/replay assertions live
        // in the `consensus` submodule's own tests; this is a smoke test
        // of the dispatch wiring through `TandemRunner`.
        let lead = StubRunner::new(
            "lead-1",
            vec![msg("lead-1", "L1"), msg("lead-1", "L2"), done("lead-1")],
        );
        let follower = StubRunner::new("foll-1", vec![msg("foll-1", "F1"), done("foll-1")]);
        let runner = TandemRunner::new(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let session = runner.start_session(params()).await.unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;

        let terminals: Vec<&AgentEvent> = collected.iter().filter(|e| e.is_terminal()).collect();
        assert_eq!(terminals.len(), 1, "exactly one synthetic terminal");
        match terminals[0] {
            AgentEvent::Completed { session, reason } => {
                assert_eq!(session.as_str(), "lead-1");
                assert_eq!(*reason, CompletionReason::Success);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
        assert!(collected.last().unwrap().is_terminal());
    }

    #[tokio::test]
    async fn with_consensus_scorer_overrides_default() {
        // Custom scorer that always favours the follower regardless of
        // content. Verifies the builder hook actually feeds through into
        // `consensus::start`'s scoring stage.
        let lead = StubRunner::new("lead-1", vec![msg("lead-1", "lead text"), done("lead-1")]);
        let follower = StubRunner::new(
            "foll-1",
            vec![msg("foll-1", "follower text"), done("foll-1")],
        );
        let scorer: ConsensusScorer = Arc::new(|events: &[AgentEvent]| {
            // Any side whose first message starts with "follower" wins.
            for ev in events {
                if let AgentEvent::Message { content, .. } = ev
                    && content.starts_with("follower")
                {
                    return 100;
                }
            }
            0
        });
        let runner = TandemRunner::new(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        )
        .with_consensus_scorer(scorer);
        let session = runner.start_session(params()).await.unwrap();
        let collected: Vec<AgentEvent> = session.events.collect().await;
        let first_msg = collected
            .iter()
            .find(|e| matches!(e, AgentEvent::Message { .. }))
            .unwrap();
        match first_msg {
            AgentEvent::Message {
                content, session, ..
            } => {
                assert_eq!(content, "follower text");
                assert_eq!(session.as_str(), "foll-1");
            }
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn follower_failure_aborts_lead_and_propagates() {
        // Lead succeeds but follower's start_session fails. The composite
        // must surface the follower error and abort the lead. We can't
        // observe the lead's abort directly here without a richer stub,
        // so this test covers the error-propagation path; lead-abort
        // wiring is exercised by the lead-side abort assertions.
        //
        // Pinned to `Consensus` because the baseline parallel-start path
        // is what surfaces follower-spawn errors synchronously.
        // Draft-review is sequential — it starts the follower only after
        // the lead's draft completes, so its follower-failure path is
        // covered by the in-stream `Failed` test in `draft_review`.
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::failing();
        let runner = TandemRunner::new(
            lead.clone() as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::Consensus,
        );

        let err = match runner.start_session(params()).await {
            Ok(_) => panic!("expected failure"),
            Err(e) => e,
        };
        assert!(matches!(err, AgentError::Spawn(_)));
        assert_eq!(lead.start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn strategy_accessor_returns_configured_value() {
        let lead = StubRunner::new("lead-1", vec![done("lead-1")]);
        let follower = StubRunner::new("foll-1", vec![done("foll-1")]);
        let runner = TandemRunner::new(
            lead as Arc<dyn AgentRunner>,
            follower as Arc<dyn AgentRunner>,
            TandemStrategy::SplitImplement,
        );
        assert_eq!(runner.strategy(), TandemStrategy::SplitImplement);
    }
}
