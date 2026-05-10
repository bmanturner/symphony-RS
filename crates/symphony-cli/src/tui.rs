//! TUI scaffold for `symphony watch`.
//!
//! # Why a separate module
//!
//! [`crate::watch`] already gives us a [`crate::watch::WatchEvent`]
//! [`futures::Stream`] that survives daemon restarts. The TUI's job is
//! purely *presentation*: turn that stream into frames the operator
//! reads, plus a key-press loop that knows how to quit. Keeping the
//! transport layer (SSE + reconnect) and the presentation layer
//! (ratatui) in different modules means each can evolve independently —
//! a panel layout change does not touch SSE parsing, and an SSE reconnect
//! tweak does not touch render code.
//!
//! # Iteration scope
//!
//! This iteration is the **scaffold** only: alternate screen, raw mode,
//! a placeholder layout (header / body / footer), terminal-resize
//! safety, and a `q` hotkey to quit. Subsequent checklist items layer
//! the active-issues table, cost summary, recent-events log, and
//! tandem-activity panel on top of the [`AppState`] defined here.
//! Putting the state struct in place now gives those iterations a stable
//! seam to plug into.
//!
//! # Testing
//!
//! Rendering is split from terminal I/O so unit tests can drive the
//! pure [`render`] function against ratatui's `TestBackend`. The
//! async [`run`] function (which owns `stdout` and the input task) is
//! only exercised end-to-end via the existing [`crate::watch`]
//! integration tests once the panels and snapshot tests land in later
//! iterations — running a real raw-mode terminal in `cargo test` is
//! flaky and not worth the maintenance.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table};
use symphony_agent::tandem::{CostModel, TandemRole};
use symphony_core::agent::{AgentEvent, SessionId, TokenUsage};
use symphony_core::events::OrchestratorEvent;
use symphony_core::state_machine::ClaimState;
use symphony_core::tracker::IssueId;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::watch::WatchEvent;

/// Connection state surfaced in the header panel. Mirrors the lifecycle
/// arms of [`WatchEvent`] one-to-one so [`AppState::apply`] is a flat
/// `match`. We keep this enum public-but-not-exhaustive: future panels
/// will read it directly when they need to grey out content during a
/// reconnect, and a `Default` of [`ConnectionStatus::Pending`] gives
/// the very first frame (rendered before any SSE bytes arrive) a
/// sensible label.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// First frame, before the SSE client has reported anything.
    #[default]
    Pending,
    /// Stream is live and reading from `url`.
    Connected {
        /// Endpoint we are reading from (echoed back from
        /// [`WatchEvent::Connected`]).
        url: String,
    },
    /// Stream ended; the watch loop will retry shortly.
    Disconnected {
        /// Human-readable reason from [`WatchEvent::Disconnected`].
        reason: String,
    },
    /// Backoff sleep before the next dial.
    Reconnecting {
        /// 1-based attempt counter.
        attempt: u32,
        /// Wall-clock delay until the next dial.
        delay: Duration,
    },
}

/// One row of the active-issues table.
///
/// An "active" issue is one the orchestrator currently holds a claim
/// for. Rows are inserted on the first
/// [`OrchestratorEvent::StateChanged`] for an issue, mutated by
/// subsequent `StateChanged` and [`OrchestratorEvent::Dispatched`]
/// events, and removed on [`OrchestratorEvent::Released`]. We
/// deliberately do **not** drop rows on `Reconciled` because the
/// orchestrator follows up with one `Released` per dropped claim — and
/// listening to both would double-remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveIssue {
    /// Best-effort tracker identifier (`ENG-123`) snapshot from the
    /// most recent event. Carried so the table can render without
    /// joining against the tracker.
    pub identifier: String,
    /// Latest [`ClaimState`] observed for this issue.
    pub state: ClaimState,
    /// Wall-clock instant of the most recent
    /// [`OrchestratorEvent::Dispatched`] for this issue. Used to render
    /// the *elapsed* column. `None` until the first dispatch — between
    /// initial claim and first dispatch the row exists but elapsed is
    /// blank, which is correct: nothing has been "running" yet.
    pub dispatched_at: Option<Instant>,
    /// Agent backend reported by the most recent
    /// [`OrchestratorEvent::Dispatched`] (`"claude"`, `"codex"`,
    /// `"tandem"`, `"mock"`). `None` until the first dispatch.
    pub backend: Option<String>,
    /// 1-based attempt number from the most recent dispatch. `1` until
    /// proven otherwise; updated by `Dispatched`.
    pub attempt: u32,
}

/// Running cost-summary aggregate.
///
/// `AgentEvent::TokenUsage` is *cumulative-per-session*: both the Claude
/// and Codex runners merge each backend snapshot into a per-session
/// running total and re-emit the full total on every update (see
/// `crates/symphony-agent/src/claude/runner.rs` `merge_usage` and
/// `codex/runner.rs` `merge_tokens`). That means a naive "sum every
/// `TokenUsage` event" approach over-counts by the number of intermediate
/// emissions. We instead remember the **latest** snapshot keyed by
/// [`SessionId`] and treat the table's sum as the cumulative total — that
/// way an adapter that only ever reports a final cumulative is identical
/// to one that streams progressive cumulatives.
///
/// Dollars are derived per-backend via [`CostModel::cost_usd`]. Pricing
/// lives in [`CostState::pricing`] and **defaults to empty** — we never
/// fabricate a price (mirrors the tandem-telemetry contract). Operators
/// supply real rates via configuration; until then the panel honestly
/// renders `$0.00` while still showing accurate token counts.
#[derive(Debug, Clone, Default)]
pub struct CostState {
    /// Latest cumulative [`TokenUsage`] per session. Replaced (not
    /// summed) on every `AgentEvent::TokenUsage` because backend
    /// adapters emit cumulative totals.
    pub session_tokens: HashMap<SessionId, TokenUsage>,
    /// Session → backend label, populated on
    /// [`OrchestratorEvent::Dispatched`]. Required so cost can pick the
    /// right `CostModel` per session — token events do not carry the
    /// backend label themselves.
    pub session_backend: HashMap<SessionId, String>,
    /// Per-backend pricing. Empty by default; an operator/config-loader
    /// populates this from `WORKFLOW.md`. Unknown backends fall through
    /// to a zero model (no fabrication).
    pub pricing: HashMap<String, CostModel>,
}

impl CostState {
    /// Sum of every session's latest cumulative token snapshot. Each
    /// scalar is a straight `u64::saturating_add` so the panel never
    /// silently wraps under absurd inputs.
    pub fn aggregate_tokens(&self) -> TokenUsage {
        let mut acc = TokenUsage::default();
        for u in self.session_tokens.values() {
            acc.input = acc.input.saturating_add(u.input);
            acc.output = acc.output.saturating_add(u.output);
            acc.cached_input = acc.cached_input.saturating_add(u.cached_input);
            acc.total = acc.total.saturating_add(u.total);
        }
        acc
    }

    /// Cumulative dollars across all sessions, applying each session's
    /// per-backend pricing. Sessions whose backend has no entry in
    /// [`CostState::pricing`] contribute `$0` rather than guessing.
    pub fn aggregate_dollars(&self) -> f64 {
        let mut total = 0.0;
        for (sid, tokens) in &self.session_tokens {
            let backend = match self.session_backend.get(sid) {
                Some(b) => b,
                None => continue,
            };
            if let Some(model) = self.pricing.get(backend) {
                total += model.cost_usd(tokens);
            }
        }
        total
    }
}

/// Maximum recent events the log keeps in memory.
///
/// Sized to a few screens of scroll-back: at 24 rows per terminal that's
/// roughly eight viewport-fulls of history, which is enough to catch a
/// burst of activity without unbounded growth on a long-lived TUI. Old
/// events are dropped FIFO; consumers who need archival history should
/// subscribe to the SSE feed directly.
pub const RECENT_EVENTS_CAP: usize = 200;

/// One captured orchestrator event, materialised for the recent-events
/// log panel.
///
/// We snapshot the *display data* rather than re-walking the original
/// [`OrchestratorEvent`] at render time so the panel renders in O(rows
/// drawn) regardless of how chatty the SSE stream is. The `kind` field
/// is the serde tag of the originating variant (used for both the
/// colour-coding and a one-token label); `summary` is a one-line
/// human-readable description; `identifier` is captured so the `f`
/// substring filter can match without joining against tracker state at
/// render time.
#[derive(Debug, Clone)]
pub struct RecentEvent {
    /// Wall-clock instant the event was applied to the TUI state. Used
    /// by the future `r` toggle to switch absolute ↔ relative time
    /// rendering across panels.
    pub at: Instant,
    /// Stable single-token label drawn from the variant's serde tag
    /// (`"state_changed"`, `"dispatched"`, `"agent"`, …) plus a synthetic
    /// `"lagged"` for transport-level drops. Drives [`event_kind_colour`]
    /// so the log is colour-coded without inspecting the payload twice.
    pub kind: &'static str,
    /// One-line human description. Adapter-specific: a `Message` carries
    /// the (truncated) text, a `Dispatched` names the backend, etc. Plain
    /// ASCII so terminals without UTF-8 still render cleanly.
    pub summary: String,
    /// Best-effort tracker identifier (e.g. `ENG-123`) when the event
    /// pertains to one specific issue. `None` for `Reconciled` (which
    /// carries only [`IssueId`]s and no human-readable identifier
    /// snapshots) and for `Lagged` (a transport-layer drop, not
    /// per-issue). The `f` filter only matches events with `Some`.
    pub identifier: Option<String>,
}

/// Stable wire label the orchestrator stamps on tandem dispatches. Lives
/// here as a constant so the apply logic and the tests both reference the
/// same string — a typo would silently make the tandem panel disappear
/// from production while still passing tests written against the typo.
pub const TANDEM_BACKEND_LABEL: &str = "tandem";

/// One row of the tandem-activity panel.
///
/// We intentionally derive everything in this struct from data that is
/// **already on the SSE wire today** — `Dispatched { backend, session }`
/// and the session ids on subsequent [`AgentEvent`]s. Fields that *would*
/// require new wire surface (concrete [`symphony_agent::tandem::TandemStrategy`]
/// label, "drafting / reviewing / executing" phase tag, lead/follower
/// backend names) are deliberately left off rather than fabricated:
/// surfacing a placeholder strategy is worse than admitting the wire
/// doesn't carry one yet, because operators trust what the panel shows.
/// A follow-up checklist item can extend [`crate::watch`] / the
/// `OrchestratorEvent::Dispatched` payload with a `tandem` descriptor and
/// upgrade this struct in lockstep.
///
/// What we *do* render:
///
/// - Lead [`SessionId`] from the original `Dispatched` event.
/// - Follower [`SessionId`] inferred when an [`OrchestratorEvent::Agent`]
///   event arrives with a session id different from the lead's.
/// - Per-side message and tool-use counts, so the operator can see at a
///   glance which side is currently doing the work.
/// - A live `AgentTelemetry`-style Jaccard agreement rate computed
///   from accumulated [`AgentEvent::Message`] contents. Computation
///   matches `aggregate_agent` in
///   `crates/symphony-agent/src/tandem/telemetry.rs` so the
///   panel's number agrees with the canonical post-turn telemetry.
/// - `last_active_role` — `Lead` or `Follower` depending on which side
///   most recently emitted an event. This is the closest we can get to
///   "current phase" without strategy on the wire; the panel labels it
///   honestly as "active" rather than "drafting".
#[derive(Debug, Clone, Default)]
pub struct TandemSession {
    /// Best-effort tracker identifier (`ENG-123`) snapshot from the
    /// `Dispatched` event. Carried so the row renders without joining
    /// against [`AppState::identifier_cache`].
    pub identifier: String,
    /// Session id stamped on the originating `Dispatched`. Treated as
    /// the lead by convention — `TandemRunner` constructs its
    /// `AgentSession` with `initial_session = lead.start_session(...).initial_session`,
    /// so the orchestrator's `Dispatched` always carries the lead's id.
    pub lead_session: Option<SessionId>,
    /// First *other* session id observed on an `Agent` event for this
    /// issue. `None` until the follower starts emitting; the panel
    /// renders "—" in that case.
    pub follower_session: Option<SessionId>,
    /// Count of [`AgentEvent::Message`] events attributed to the lead.
    pub lead_messages: u32,
    /// Count of [`AgentEvent::Message`] events attributed to the
    /// follower.
    pub follower_messages: u32,
    /// Count of [`AgentEvent::ToolUse`] events attributed to the lead.
    pub lead_tools: u32,
    /// Count of [`AgentEvent::ToolUse`] events attributed to the
    /// follower.
    pub follower_tools: u32,
    /// Bag of lowercase alphanumeric word tokens drawn from every lead
    /// `Message.content` seen so far. Drives the Jaccard agreement
    /// computation. Bounded growth: each unique word is stored once.
    pub lead_word_bag: HashSet<String>,
    /// Bag of lowercase alphanumeric word tokens drawn from every
    /// follower `Message.content` seen so far. See `lead_word_bag`.
    pub follower_word_bag: HashSet<String>,
    /// Which side most recently emitted an event; rendered as the
    /// `active` column. `None` until the first event after dispatch.
    pub last_active_role: Option<TandemRole>,
}

impl TandemSession {
    /// Live agreement rate, identical in spirit to
    /// `tandem::telemetry::compute`'s post-turn computation: Jaccard
    /// similarity over the bag-of-tokens. Returns `0.0` if either side
    /// has yet to emit any message tokens — no honest agreement can be
    /// claimed when one side is silent.
    pub fn agreement_rate(&self) -> f64 {
        if self.lead_word_bag.is_empty() || self.follower_word_bag.is_empty() {
            return 0.0;
        }
        let intersection = self
            .lead_word_bag
            .intersection(&self.follower_word_bag)
            .count();
        let union = self.lead_word_bag.union(&self.follower_word_bag).count();
        if union == 0 {
            return 0.0;
        }
        intersection as f64 / union as f64
    }
}

/// Extract lowercase alphanumeric word tokens from `content`.
///
/// Pulled into a free function so the bag-update path in
/// [`AppState::apply_orchestrator_event`] and the agreement test fixtures
/// share one definition. Kept *intentionally simple*: anything outside
/// `[a-z0-9]` is a separator. This matches the spirit of
/// `tandem::telemetry::collect_message_tokens` while staying inline so
/// the TUI doesn't need to depend on private helpers.
fn extract_word_tokens(content: &str) -> impl Iterator<Item = String> + '_ {
    content
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
}

/// All state the TUI needs to render a frame.
///
/// Future panels (tandem activity) push their own fields onto this
/// struct. Keeping a single owning struct (rather than a tangle of
/// `Arc<Mutex<…>>` per panel) means the render pipeline is
/// `(&AppState, Instant) → Frame`, which is trivial to snapshot-test.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// Current SSE connection status, drives the header banner.
    pub connection: ConnectionStatus,
    /// Issues the orchestrator currently has claimed, keyed by the
    /// tracker's stable [`IssueId`]. We use a [`HashMap`] for O(1)
    /// lookup on `apply` and sort by `identifier` at render time so
    /// the visual order is deterministic regardless of insertion
    /// order — important for ratatui snapshot tests.
    pub active_issues: HashMap<IssueId, ActiveIssue>,
    /// Token + dollar accumulator surfaced in the cost-summary panel.
    pub cost: CostState,
    /// FIFO ring buffer of the most recent events, capped at
    /// [`RECENT_EVENTS_CAP`]. Newest events are pushed to the back; the
    /// recent-events panel renders the tail (newest-first) so a
    /// reconnecting TUI shows the most relevant context first.
    pub recent_events: VecDeque<RecentEvent>,
    /// Best-known human identifier per [`IssueId`], populated from any
    /// event that carries one. Lets us label `Agent` events (which
    /// carry only the raw [`IssueId`]) with the same `ENG-123` string
    /// the operator sees in the active-issues table — and lets the `f`
    /// filter match those events too. Never purged: a release that
    /// removes the row from `active_issues` does not remove its
    /// identifier from history.
    pub identifier_cache: HashMap<IssueId, String>,
    /// Current substring filter applied to the recent-events panel.
    /// Empty string disables the filter entirely.
    pub event_filter: String,
    /// `true` while the user is typing into the filter input via the
    /// `f` hotkey. While set, character keys append to
    /// [`AppState::event_filter`] instead of routing as commands; `Esc`
    /// cancels (clears + exits) and `Enter` commits (keeps + exits).
    pub filter_input_mode: bool,
    /// Live tandem-mode dispatches keyed by [`IssueId`]. Populated only
    /// when an [`OrchestratorEvent::Dispatched`] arrives with
    /// `backend == TANDEM_BACKEND_LABEL`; cleared on `Released`. The
    /// tandem-activity panel renders **only** while this map is
    /// non-empty, so a non-tandem workflow never sees the panel chrome.
    pub tandem_sessions: HashMap<IssueId, TandemSession>,
    /// Set true when the user pressed `q` (or `Ctrl+C`); the run loop
    /// terminates on the next iteration. Tests inspect this directly.
    pub should_quit: bool,
}

impl AppState {
    /// Fold a [`WatchEvent`] into the state. Pure function — no I/O —
    /// so unit tests can build a state up event-by-event without a
    /// running tokio runtime.
    pub fn apply(&mut self, ev: &WatchEvent) {
        match ev {
            WatchEvent::Connected { url } => {
                self.connection = ConnectionStatus::Connected { url: url.clone() };
            }
            WatchEvent::Disconnected { reason } => {
                self.connection = ConnectionStatus::Disconnected {
                    reason: reason.clone(),
                };
            }
            WatchEvent::Reconnecting { attempt, delay } => {
                self.connection = ConnectionStatus::Reconnecting {
                    attempt: *attempt,
                    delay: *delay,
                };
            }
            WatchEvent::Lagged { missed } => {
                // A `Lagged` is a transport-layer drop, not an
                // orchestrator event — push it onto the recent-events
                // log so the operator sees it (in red) without lying
                // about the orchestrator's state.
                self.push_recent("lagged", format!("missed {missed} events"), None);
            }
            WatchEvent::Event(orch) => {
                self.apply_orchestrator_event(orch);
            }
        }
    }

    /// Append one entry to the recent-events ring buffer, evicting the
    /// oldest if [`RECENT_EVENTS_CAP`] would be exceeded. Pure helper
    /// (no I/O, no clock dependency beyond [`Instant::now`]) so the
    /// call sites in [`AppState::apply_orchestrator_event`] stay flat.
    fn push_recent(&mut self, kind: &'static str, summary: String, identifier: Option<String>) {
        if self.recent_events.len() == RECENT_EVENTS_CAP {
            self.recent_events.pop_front();
        }
        self.recent_events.push_back(RecentEvent {
            at: Instant::now(),
            kind,
            summary,
            identifier,
        });
    }

    /// Update [`AppState::active_issues`] from one orchestrator event.
    ///
    /// Folded out of [`AppState::apply`] so unit tests can drive table
    /// state without constructing a [`WatchEvent`] wrapper. Pure: no
    /// I/O, no clock dependency — `dispatched_at` is stamped with
    /// `Instant::now()` only when a `Dispatched` event arrives,
    /// matching the wall-clock semantics of the SSE feed.
    ///
    /// # Routing rules
    ///
    /// - `StateChanged` upserts the row's `identifier` and `state`.
    ///   For the [`ClaimState::RetryQueued`] branch we lift the
    ///   `attempt` onto the row so the table can show "retry(3)" even
    ///   before the next `Dispatched` lands.
    /// - `Dispatched` upserts the row (in case the table missed the
    ///   prior `StateChanged`, which can happen when the TUI starts
    ///   mid-flight) and stamps `dispatched_at`, `backend`, `attempt`.
    /// - `Released` removes the row. We trust `Released` rather than
    ///   the bulk `Reconciled` summary because the orchestrator emits
    ///   one `Released` per dropped claim (see [`symphony_core::events`]),
    ///   and reacting to both would double-remove.
    /// - All other variants are presentation-only here (the table
    ///   panel does not care about agent re-emissions or retry
    ///   schedules — the cost-summary and retry panels in later
    ///   iterations do).
    pub fn apply_orchestrator_event(&mut self, ev: &OrchestratorEvent) {
        match ev {
            OrchestratorEvent::StateChanged {
                issue,
                identifier,
                previous,
                current,
            } => {
                self.identifier_cache
                    .insert(issue.clone(), identifier.clone());
                let row = self
                    .active_issues
                    .entry(issue.clone())
                    .or_insert_with(|| ActiveIssue {
                        identifier: identifier.clone(),
                        state: *current,
                        dispatched_at: None,
                        backend: None,
                        attempt: 1,
                    });
                row.identifier = identifier.clone();
                row.state = *current;
                if let ClaimState::RetryQueued { attempt } = current {
                    row.attempt = *attempt;
                }
                let summary = match previous {
                    Some(p) => format!(
                        "{identifier} {} → {}",
                        format_state(p),
                        format_state(current)
                    ),
                    None => format!("{identifier} → {}", format_state(current)),
                };
                self.push_recent("state_changed", summary, Some(identifier.clone()));
            }
            OrchestratorEvent::Dispatched {
                issue,
                identifier,
                session,
                backend,
                attempt,
            } => {
                self.identifier_cache
                    .insert(issue.clone(), identifier.clone());
                let row = self
                    .active_issues
                    .entry(issue.clone())
                    .or_insert_with(|| ActiveIssue {
                        identifier: identifier.clone(),
                        state: ClaimState::Running,
                        dispatched_at: None,
                        backend: None,
                        attempt: *attempt,
                    });
                row.identifier = identifier.clone();
                row.state = ClaimState::Running;
                row.backend = Some(backend.clone());
                row.attempt = *attempt;
                row.dispatched_at = Some(Instant::now());
                // Capture the session→backend mapping so the cost panel
                // can apply per-backend pricing to later TokenUsage
                // events. Token events carry only the SessionId.
                self.cost
                    .session_backend
                    .insert(session.clone(), backend.clone());
                if backend == TANDEM_BACKEND_LABEL {
                    // Establish a tandem-panel row keyed by the issue.
                    // The dispatch's session id is the lead by convention
                    // (see `TandemSession::lead_session` doc). We
                    // *replace* any prior entry rather than merging:
                    // a re-dispatch (retry) starts a fresh tandem turn,
                    // and stale message/tool counts from the previous
                    // turn would mislead the operator.
                    self.tandem_sessions.insert(
                        issue.clone(),
                        TandemSession {
                            identifier: identifier.clone(),
                            lead_session: Some(session.clone()),
                            ..Default::default()
                        },
                    );
                }
                self.push_recent(
                    "dispatched",
                    format!("{identifier} dispatched [{backend}] attempt {attempt}"),
                    Some(identifier.clone()),
                );
            }
            OrchestratorEvent::Released {
                issue,
                identifier,
                reason,
                ..
            } => {
                self.identifier_cache
                    .insert(issue.clone(), identifier.clone());
                self.active_issues.remove(issue);
                // Drop the tandem-panel row in lockstep with the active
                // issue. Cumulative cost survives release (see CostState
                // docs), but the tandem panel is *live activity only* —
                // a released issue is no longer doing tandem work and
                // keeping the row would imply otherwise.
                self.tandem_sessions.remove(issue);
                // Cost totals are intentionally *not* purged on
                // release: the panel renders a cumulative-since-launch
                // figure, and dropping a session's contribution when
                // its issue completes would make the dollar count run
                // backwards. Sessions are pruned only when the TUI
                // restarts.
                self.push_recent(
                    "released",
                    format!("{identifier} released ({reason:?})"),
                    Some(identifier.clone()),
                );
            }
            OrchestratorEvent::Agent { issue, event } => {
                if let AgentEvent::TokenUsage { session, usage } = event {
                    // Replace, don't sum: backend adapters emit
                    // cumulative totals on every update, so the latest
                    // snapshot already includes everything that came
                    // before. See [`CostState`] docs.
                    self.cost
                        .session_tokens
                        .insert(session.clone(), usage.clone());
                }
                // Route into the tandem panel if we're tracking this
                // issue as a tandem dispatch. Done before
                // identifier-cache lookup so the lead/follower
                // attribution is independent of any other panel state.
                if let Some(t) = self.tandem_sessions.get_mut(issue) {
                    apply_tandem_agent_event(t, event);
                }
                let identifier = self.identifier_cache.get(issue).cloned();
                let label = identifier.clone().unwrap_or_else(|| "?".into());
                let summary = format_agent_event(&label, event);
                self.push_recent("agent", summary, identifier);
            }
            OrchestratorEvent::RetryScheduled {
                issue,
                identifier,
                attempt,
                reason,
                delay_ms,
                ..
            } => {
                self.identifier_cache
                    .insert(issue.clone(), identifier.clone());
                self.push_recent(
                    "retry_scheduled",
                    format!("{identifier} retry attempt {attempt} in {delay_ms}ms ({reason:?})"),
                    Some(identifier.clone()),
                );
            }
            OrchestratorEvent::Reconciled { dropped } => {
                self.push_recent(
                    "reconciled",
                    format!("reconciled: dropped {} issue(s)", dropped.len()),
                    None,
                );
            }
            OrchestratorEvent::ScopeCapReached {
                run_id,
                identifier,
                scope_kind,
                scope_key,
                in_flight,
                cap,
            } => {
                let label = identifier
                    .clone()
                    .or_else(|| run_id.map(|id| format!("run#{id}")))
                    .unwrap_or_else(|| "<unknown>".into());
                let scope_label = if scope_key.is_empty() {
                    format!("{scope_kind:?}")
                } else {
                    format!("{scope_kind:?}({scope_key})")
                };
                self.push_recent(
                    "scope_cap_reached",
                    format!("{label} blocked on {scope_label} {in_flight}/{cap}"),
                    identifier.clone(),
                );
            }
            OrchestratorEvent::BudgetExceeded {
                run_id,
                identifier,
                budget_kind,
                observed,
                cap,
            } => {
                let label = if identifier.is_empty() {
                    run_id
                        .map(|id| format!("run#{id}"))
                        .unwrap_or_else(|| "<unknown>".into())
                } else {
                    identifier.clone()
                };
                self.push_recent(
                    "budget_exceeded",
                    format!("{label} hit {budget_kind} budget {observed}/{cap}"),
                    Some(identifier.clone()),
                );
            }
            OrchestratorEvent::RunCancelled {
                run_id,
                identifier,
                reason,
                requested_by,
                ..
            } => {
                let label = identifier
                    .clone()
                    .unwrap_or_else(|| format!("run#{run_id}"));
                self.push_recent(
                    "run_cancelled",
                    format!("{label} cancelled by {requested_by}: {reason}"),
                    identifier.clone(),
                );
            }
        }
    }

    /// Translate a key press into a state mutation. Returns `true` if
    /// the key was consumed. Encapsulating this here (rather than in
    /// the run loop's `match`) means tests can drive shortcut behaviour
    /// without a real terminal.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Crossterm fires `KeyEventKind::Release` on some platforms.
        // We only react to presses so a held `q` doesn't double-fire,
        // and so a key-up from a previous foreground process doesn't
        // immediately quit on launch.
        if key.kind != KeyEventKind::Press {
            return false;
        }

        // Filter input mode swallows most keys: it is a tiny in-line
        // text field, not a shortcut surface. We deliberately do *not*
        // treat `q` as quit while typing — an operator filtering by
        // `queue` would otherwise quit halfway through their search.
        if self.filter_input_mode {
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) => {
                    // Cancel: clear the filter and exit input mode so
                    // the panel returns to showing every event.
                    self.event_filter.clear();
                    self.filter_input_mode = false;
                    true
                }
                (KeyCode::Enter, _) => {
                    // Commit: keep the filter, exit input mode. The
                    // panel header continues to show the active filter
                    // so the operator never forgets it is on.
                    self.filter_input_mode = false;
                    true
                }
                (KeyCode::Backspace, _) => {
                    self.event_filter.pop();
                    true
                }
                // Ctrl+C is an emergency exit even from input mode.
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                    true
                }
                (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                    // Cap the filter at a sane upper bound: the input
                    // line is one terminal row and identifiers in the
                    // wild are short. 64 chars is comfortably more than
                    // any real tracker key.
                    if self.event_filter.len() < 64 {
                        self.event_filter.push(c);
                    }
                    true
                }
                _ => false,
            }
        } else {
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                    self.should_quit = true;
                    true
                }
                // Ctrl+C is an explicit quit signal: SIGINT does not
                // reach a process running in raw mode, so we have to
                // translate the keystroke ourselves.
                (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                    true
                }
                (KeyCode::Char('f'), _) => {
                    // Enter filter input mode. We do *not* clear the
                    // existing filter so an operator can refine it
                    // (`f`, edit, Enter) without retyping from scratch.
                    self.filter_input_mode = true;
                    true
                }
                _ => false,
            }
        }
    }
}

/// Build a one-line summary of an [`AgentEvent`] for the recent-events
/// log. Pulled out so the routing in [`AppState::apply_orchestrator_event`]
/// stays a flat match rather than a nested one. The `label` argument is
/// the issue identifier (or `"?"` if the cache has not seen one yet).
///
/// Long message bodies are truncated to 60 characters with an ellipsis;
/// the panel is one row per event and full transcripts belong in the
/// SSE stream itself, not the log surface.
fn format_agent_event(label: &str, event: &AgentEvent) -> String {
    match event {
        AgentEvent::Started { .. } => format!("{label} session started"),
        AgentEvent::Message { role, content, .. } => {
            format!("{label} {role}: {}", truncate_one_line(content, 60))
        }
        AgentEvent::ToolUse { tool, .. } => format!("{label} tool: {tool}"),
        AgentEvent::TokenUsage { usage, .. } => format!(
            "{label} tokens in {} out {} total {}",
            usage.input, usage.output, usage.total
        ),
        AgentEvent::RateLimit { .. } => format!("{label} rate limited"),
        AgentEvent::Completed { reason, .. } => format!("{label} completed: {reason:?}"),
        AgentEvent::Failed { error, .. } => format!("{label} failed: {error}"),
    }
}

/// Fold one [`AgentEvent`] into a tandem-session row.
///
/// Routing rule: an event whose session id matches the lead is attributed
/// to the lead seat; any other session id is attributed to the follower.
/// The first non-lead session id we observe is *adopted* as the follower
/// (the wire does not carry an explicit follower-session announcement, so
/// we rely on this "first other session wins" heuristic — `TandemRunner`
/// only ever spawns two inner runners, so this is unambiguous in
/// practice). Once adopted, a third unrecognised session id is silently
/// ignored rather than overwriting the follower; that path should never
/// fire in production but is the safe choice if a future strategy ever
/// emits stray events.
///
/// We deliberately ignore non-message / non-tool events here so the bag
/// of tokens powering the agreement rate stays focused on substantive
/// content; `Started` / `Completed` carry no semantically interesting
/// text.
fn apply_tandem_agent_event(t: &mut TandemSession, event: &AgentEvent) {
    let session = event.session();
    let role = match (&t.lead_session, &t.follower_session) {
        (Some(lead), _) if lead == session => TandemRole::Lead,
        (_, Some(follower)) if follower == session => TandemRole::Follower,
        (Some(_), None) => {
            // Adopt this session as the follower on first sighting.
            t.follower_session = Some(session.clone());
            TandemRole::Follower
        }
        // Pre-dispatch state (no lead recorded yet). Should not happen
        // because tandem rows are only inserted on `Dispatched`, which
        // sets `lead_session`; defend with a no-op.
        _ => return,
    };
    t.last_active_role = Some(role);
    match event {
        AgentEvent::Message { content, .. } => match role {
            TandemRole::Lead => {
                t.lead_messages = t.lead_messages.saturating_add(1);
                t.lead_word_bag.extend(extract_word_tokens(content));
            }
            TandemRole::Follower => {
                t.follower_messages = t.follower_messages.saturating_add(1);
                t.follower_word_bag.extend(extract_word_tokens(content));
            }
        },
        AgentEvent::ToolUse { .. } => match role {
            TandemRole::Lead => t.lead_tools = t.lead_tools.saturating_add(1),
            TandemRole::Follower => t.follower_tools = t.follower_tools.saturating_add(1),
        },
        _ => {}
    }
}

/// Trim a free-text string to a single line, truncating to `max` chars
/// with an ellipsis if needed. Newlines and tabs are collapsed to
/// spaces so multi-line message content does not break the panel
/// layout. Pure ASCII handling — we measure in chars to stay
/// codepoint-safe for non-ASCII content.
fn truncate_one_line(s: &str, max: usize) -> String {
    let collapsed: String = s
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if collapsed.chars().count() <= max {
        return collapsed;
    }
    let head: String = collapsed.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Render the TUI layout against any backend.
///
/// Layout, top to bottom:
///
/// 1. **Header** (3 lines, bordered) — connection status banner.
/// 2. **Active issues table** (flex, bordered) — one row per
///    currently-claimed issue with identifier, state, elapsed,
///    backend.
/// 3. **Cost summary** (3 lines, bordered) — token + dollar totals.
/// 4. **Recent events log** (flex, bordered) — colour-coded ring
///    buffer of the last [`RECENT_EVENTS_CAP`] events, optionally
///    filtered by identifier substring (`f`).
/// 5. **Footer** (1 line, no border) — key bindings or filter input.
///
/// Taking `now: Instant` as a parameter (rather than calling
/// [`Instant::now`] internally) lets unit tests pin elapsed-time
/// rendering deterministically.
pub fn render(state: &AppState, now: Instant, frame: &mut Frame<'_>) {
    let area = frame.area();
    // The tandem panel is conditional: when no tandem sessions are
    // active we collapse it out of the layout entirely so non-tandem
    // workflows do not pay the chrome of an empty box. When present,
    // its height is sized to the number of rows (capped at 5) plus
    // header + 2 borders, so a single tandem session uses ~4 lines and
    // a busy fleet uses no more than ~8.
    let tandem_visible = !state.tandem_sessions.is_empty();
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ];
    if tandem_visible {
        let rows = state.tandem_sessions.len().min(5) as u16;
        constraints.push(Constraint::Length(rows + 3));
    }
    constraints.push(Constraint::Min(5));
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let header_text = match &state.connection {
        ConnectionStatus::Pending => "connecting…".to_string(),
        ConnectionStatus::Connected { url } => format!("connected: {url}"),
        ConnectionStatus::Disconnected { reason } => format!("disconnected: {reason}"),
        ConnectionStatus::Reconnecting { attempt, delay } => {
            format!("reconnecting (attempt {attempt}, in {delay:?})")
        }
    };
    frame.render_widget(
        Paragraph::new(header_text).block(
            Block::default()
                .borders(Borders::ALL)
                .title("symphony watch"),
        ),
        chunks[0],
    );

    render_active_issues(state, now, frame, chunks[1]);
    render_cost_summary(state, frame, chunks[2]);
    // Index bookkeeping: when the tandem panel is hidden, recent-events
    // and footer slide up by one. Computing the indices once keeps the
    // routing readable even as the layout grows.
    let (tandem_idx, recent_idx, footer_idx) = if tandem_visible {
        (Some(3usize), 4usize, 5usize)
    } else {
        (None, 3usize, 4usize)
    };
    if let Some(idx) = tandem_idx {
        render_tandem_activity(state, frame, chunks[idx]);
    }
    render_recent_events(state, now, frame, chunks[recent_idx]);
    render_footer(state, frame, chunks[footer_idx]);
}

/// Render the tandem-activity panel: one row per active tandem dispatch
/// showing the lead/follower session ids, message and tool counts per
/// side, the side that most recently emitted an event, and the live
/// Jaccard agreement rate.
///
/// Layout note: this function is only invoked when the panel is visible
/// (see [`render`]) — it does not branch on emptiness internally because
/// the slot is collapsed out of the layout in the empty case.
///
/// Strategy and a strategy-aware phase tag (`drafting` / `reviewing` /
/// `executing`) are *intentionally* not shown: they are not on the
/// `OrchestratorEvent` wire today, and the project convention is to
/// admit a gap rather than fabricate a label. The "active" column —
/// `lead` or `follower` — is the closest honest substitute and matches
/// what the operator can verify by reading the recent-events log.
fn render_tandem_activity(state: &AppState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let title = format!("tandem activity ({})", state.tandem_sessions.len());
    let block = Block::default().borders(Borders::ALL).title(title);

    // Sort by identifier so two equal AppStates render byte-identical
    // frames — the foundation for snapshot tests and a property the
    // active-issues panel already relies on.
    let mut rows: Vec<(&IssueId, &TandemSession)> = state.tandem_sessions.iter().collect();
    rows.sort_by(|a, b| a.1.identifier.cmp(&b.1.identifier));

    let header = Row::new(vec![
        Cell::from("identifier"),
        Cell::from("active"),
        Cell::from("lead"),
        Cell::from("follower"),
        Cell::from("agree"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let body_rows: Vec<Row> = rows
        .into_iter()
        .map(|(_, t)| {
            let active = match t.last_active_role {
                Some(TandemRole::Lead) => "lead".to_string(),
                Some(TandemRole::Follower) => "follower".to_string(),
                // No event yet — the row exists because the dispatch
                // arrived but neither side has spoken. "—" mirrors the
                // active-issues panel's pre-dispatch convention.
                None => "—".to_string(),
            };
            let active_style = match t.last_active_role {
                Some(TandemRole::Lead) => Style::default().fg(Color::Cyan),
                Some(TandemRole::Follower) => Style::default().fg(Color::Magenta),
                None => Style::default().add_modifier(Modifier::DIM),
            };
            // Render a session id as `(msgs/tools)` so the row stays
            // tight: full session ids are 36-char UUIDs and would
            // overflow the column. The session ids themselves are not
            // load-bearing for the operator — the recent-events log
            // already shows them.
            let lead = format!("msg {} tool {}", t.lead_messages, t.lead_tools);
            let follower = if t.follower_session.is_some() {
                format!("msg {} tool {}", t.follower_messages, t.follower_tools)
            } else {
                "—".to_string()
            };
            let agree = format!("{:.2}", t.agreement_rate());
            Row::new(vec![
                Cell::from(t.identifier.clone()),
                Cell::from(active).style(active_style),
                Cell::from(lead),
                Cell::from(follower),
                Cell::from(agree),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(6),
    ];
    let table = Table::new(body_rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

/// Render the footer line: shortcut help when idle, an inline filter
/// editor when [`AppState::filter_input_mode`] is on. We deliberately
/// keep this to a single row so the body panels do not jitter when the
/// user opens or closes the input.
fn render_footer(state: &AppState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let line = if state.filter_input_mode {
        // The trailing `_` is a fake caret — crossterm does not draw
        // one in a Paragraph and a blinking cursor would require
        // routing terminal control through ratatui, which is more
        // ceremony than this footer is worth.
        Line::from(vec![
            Span::styled(
                "filter: ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(state.event_filter.clone()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            Span::raw("  "),
            Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" apply  "),
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ])
    } else {
        let mut spans = vec![
            Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" quit  "),
            Span::styled("f", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" filter"),
        ];
        if !state.event_filter.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("[filter: {}]", state.event_filter),
                Style::default().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Foreground colour used to tint a recent-event row by its variant.
///
/// Picks distinct hues across the six orchestrator variants so a busy
/// feed is scannable at a glance. `lagged` is red because it is the
/// only line that signals data loss.
fn event_kind_colour(kind: &str) -> Color {
    match kind {
        "state_changed" => Color::Cyan,
        "dispatched" => Color::Green,
        "agent" => Color::Gray,
        "retry_scheduled" => Color::Yellow,
        "reconciled" => Color::Magenta,
        "released" => Color::Blue,
        "lagged" => Color::Red,
        // A future variant added on the wire side will land here until
        // the TUI catches up; default to no tint rather than panic so
        // the panel keeps rendering.
        _ => Color::White,
    }
}

/// Render the recent-events log: newest at the top, colour-coded by
/// variant, optionally filtered by identifier substring.
///
/// Filter rules:
/// - Empty filter renders every event.
/// - Non-empty filter keeps only events whose `identifier` contains the
///   filter substring (case-sensitive — tracker identifiers are
///   conventionally fixed case, so a permissive `to_lowercase` would
///   only mask real mismatches).
/// - Events without an identifier (`reconciled`, `lagged`) are hidden
///   when a filter is active. They are not "about" any one issue, so
///   showing them under a per-issue filter would be misleading.
fn render_recent_events(
    state: &AppState,
    now: Instant,
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
) {
    let title = if state.event_filter.is_empty() {
        format!("recent events ({})", state.recent_events.len())
    } else {
        let visible = state
            .recent_events
            .iter()
            .filter(|e| {
                e.identifier
                    .as_deref()
                    .is_some_and(|id| id.contains(&state.event_filter))
            })
            .count();
        format!(
            "recent events ({}/{}) [filter: {}]",
            visible,
            state.recent_events.len(),
            state.event_filter
        )
    };
    let block = Block::default().borders(Borders::ALL).title(title);

    if state.recent_events.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no events yet)",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    }

    // Newest-first: iterate the deque in reverse so the most recent
    // line sits at the top of the viewport. Operators read the panel
    // top-to-bottom and the freshest information is what they care
    // about.
    let items: Vec<ListItem> = state
        .recent_events
        .iter()
        .rev()
        .filter(|e| {
            if state.event_filter.is_empty() {
                true
            } else {
                e.identifier
                    .as_deref()
                    .is_some_and(|id| id.contains(&state.event_filter))
            }
        })
        .map(|e| {
            let elapsed = format_elapsed(now.saturating_duration_since(e.at));
            let line = Line::from(vec![
                Span::styled(
                    format!("{elapsed:>6} "),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("{:<16} ", e.kind),
                    Style::default()
                        .fg(event_kind_colour(e.kind))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(e.summary.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();

    frame.render_widget(List::new(items).block(block), area);
}

/// Render the single-line cost-summary panel.
///
/// Shows the four token buckets (`in`, `out`, `cached`, `total`) followed
/// by cumulative dollars. Numbers are thousand-separated so a six-figure
/// token count is still legible in an 80-column terminal. When pricing
/// is unconfigured (zero models for every backend) the dollar field
/// renders as `$0.00` — accurate to the configuration, never fabricated.
fn render_cost_summary(state: &AppState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let agg = state.cost.aggregate_tokens();
    let dollars = state.cost.aggregate_dollars();
    let line = Line::from(vec![
        Span::styled("tok ", Style::default().add_modifier(Modifier::DIM)),
        Span::raw(format!("in {}", thousands(agg.input))),
        Span::raw("  "),
        Span::raw(format!("out {}", thousands(agg.output))),
        Span::raw("  "),
        Span::raw(format!("cached {}", thousands(agg.cached_input))),
        Span::raw("  "),
        Span::raw(format!("total {}", thousands(agg.total))),
        Span::raw("    "),
        Span::styled(
            format!("${dollars:.2}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title("cost")),
        area,
    );
}

/// Thousand-separator format for token counts.
///
/// `1234567` → `"1,234,567"`. Pure ASCII so it renders identically across
/// terminals and snapshot tests; no `num_format` dep needed.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        // Insert a comma before every group of three digits except the
        // first. `len - i` is the number of digits *remaining*, so a
        // multiple-of-three boundary marks a separator point.
        let remaining = bytes.len() - i;
        if i > 0 && remaining.is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Render the active-issues table into `area`.
///
/// Sorted by tracker `identifier` so the visual order is deterministic
/// independent of HashMap iteration order — required for ratatui
/// `TestBackend` snapshot tests in later iterations. When the table is
/// empty we render a placeholder line rather than collapsing the
/// border, so a quiet system still looks like the right kind of UI
/// (an empty table) instead of a missing panel.
fn render_active_issues(
    state: &AppState,
    now: Instant,
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("active issues ({})", state.active_issues.len()));

    if state.active_issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no active issues)",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    }

    // Sort rows by identifier so two AppStates with identical content
    // render identical frames — the foundation for snapshot testing.
    let mut rows: Vec<(&IssueId, &ActiveIssue)> = state.active_issues.iter().collect();
    rows.sort_by(|a, b| a.1.identifier.cmp(&b.1.identifier));

    let header = Row::new(vec![
        Cell::from("identifier"),
        Cell::from("state"),
        Cell::from("elapsed"),
        Cell::from("backend"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let body_rows: Vec<Row> = rows
        .into_iter()
        .map(|(_, ai)| {
            let state_label = format_state(&ai.state);
            let state_cell = Cell::from(state_label).style(state_colour(&ai.state));
            let elapsed = ai
                .dispatched_at
                .map(|d| format_elapsed(now.saturating_duration_since(d)))
                .unwrap_or_else(|| "—".to_string());
            let backend = ai.backend.clone().unwrap_or_else(|| "—".to_string());
            Row::new(vec![
                Cell::from(ai.identifier.clone()),
                state_cell,
                Cell::from(elapsed),
                Cell::from(backend),
            ])
        })
        .collect();

    // Column widths chosen to match the longest realistic value:
    // identifiers are typically `ABC-1234` (≤ 12 chars), states fit
    // in 12 chars including `retry(99)`, elapsed renders as `99h59m`
    // at most (we cap formatting), and backend is one of four short
    // tokens. The remainder flexes via `Min`.
    let widths = [
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Min(8),
    ];
    let table = Table::new(body_rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

/// Stringify a [`ClaimState`] for the state column.
///
/// Kept separate from `ClaimState`'s serde tag so the *display* form
/// can diverge from the wire form without breaking the SSE contract:
/// today they happen to agree (`"running"`, `"retry(N)"`), but a
/// future variant might serialise differently from how it should
/// render in a 12-column cell.
fn format_state(state: &ClaimState) -> String {
    match state {
        ClaimState::Running => "running".to_string(),
        ClaimState::RetryQueued { attempt } => format!("retry({attempt})"),
    }
}

/// Foreground colour for a state cell.
///
/// Running is green (steady-state happy path), retries are yellow
/// (something went wrong but the orchestrator is handling it). Future
/// states (e.g. a `Stuck` variant) get a colour at the same time they
/// land so the table never silently downgrades to default white.
fn state_colour(state: &ClaimState) -> Style {
    match state {
        ClaimState::Running => Style::default().fg(Color::Green),
        ClaimState::RetryQueued { .. } => Style::default().fg(Color::Yellow),
    }
}

/// Format a [`Duration`] into a tight 1–6-char label.
///
/// We render `Ns`, `NmMs`, `NhMm` rather than `H:MM:SS` because the
/// elapsed column is 8 characters wide and many sessions run into
/// hours. Above 99h we clamp to `99h+` so the column never overflows.
fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        return format!("{total}s");
    }
    if total < 3_600 {
        let m = total / 60;
        let s = total % 60;
        return format!("{m}m{s:02}s");
    }
    let h = total / 3_600;
    if h >= 100 {
        return "99h+".to_string();
    }
    let m = (total % 3_600) / 60;
    format!("{h}h{m:02}m")
}

/// Drive the TUI: own stdout in alternate-screen + raw mode, render on
/// every state change, quit on `q` / `Esc` / `Ctrl+C` / external
/// cancel. Returns when the user quits or `cancel` fires.
///
/// The function is structured so that any `?` after [`ratatui::init()`]
/// must run through the `restore_terminal` helper — leaving the user's
/// terminal in raw mode is the worst possible failure mode for a TUI.
/// We use a guard struct rather than `try {}` because `ratatui::init`'s
/// `DefaultTerminal` already does most of the work, but we still wrap
/// the run body in a closure so any error path drops the terminal
/// before propagating.
pub async fn run<S>(stream: S, cancel: CancellationToken) -> anyhow::Result<()>
where
    S: Stream<Item = WatchEvent> + Send + 'static,
{
    let mut terminal = ratatui::init();
    let outcome = run_inner(&mut terminal, stream, cancel).await;
    // `ratatui::restore` swallows any error printing the terminal back
    // to a sane state — a TUI that exits is not a place to surface
    // I/O errors, and the stderr would land in a now-restored cooked
    // shell anyway.
    ratatui::restore();
    outcome
}

/// The TUI body, separated from `run` so the alt-screen restore
/// always runs. Tests do not exercise this directly (we'd need a real
/// pty); state-update paths are covered via [`AppState::apply`] and
/// [`AppState::handle_key`].
async fn run_inner<S>(
    terminal: &mut ratatui::DefaultTerminal,
    stream: S,
    cancel: CancellationToken,
) -> anyhow::Result<()>
where
    S: Stream<Item = WatchEvent> + Send + 'static,
{
    let mut state = AppState::default();
    let mut stream = Box::pin(stream);
    let mut keys = spawn_key_reader(cancel.clone());

    // Initial frame so the operator sees *something* before the first
    // SSE byte arrives — the SSE dial may take a few hundred ms over
    // a slow network and a blank terminal looks broken.
    terminal.draw(|f| render(&state, Instant::now(), f))?;

    loop {
        tokio::select! {
            // Bias toward input so a `q` press during a flood of events
            // still feels responsive: tokio::select! is random by
            // default, but a key press should never wait behind an
            // unbounded SSE stream.
            biased;

            _ = cancel.cancelled() => {
                debug!("tui: external cancel");
                break;
            }
            maybe_input = keys.recv() => {
                match maybe_input {
                    Some(Event::Key(k)) => {
                        state.handle_key(k);
                    }
                    Some(Event::Resize(_, _)) => {
                        // Ratatui auto-detects the new size on the next
                        // `draw`; we just need to make sure a draw
                        // happens. Falling through to the redraw at the
                        // bottom of the loop accomplishes that.
                    }
                    Some(_) => {
                        // Mouse / paste / focus events are not bound in
                        // the scaffold. Future iterations may add panel
                        // hover; for now ignore so they don't show up
                        // as random redraws.
                    }
                    None => {
                        // Reader task exited (cancelled or terminal
                        // closed). Treat as a quit so we don't spin.
                        debug!("tui: input reader closed");
                        state.should_quit = true;
                    }
                }
            }
            maybe_event = stream.next() => {
                match maybe_event {
                    Some(ev) => state.apply(&ev),
                    None => {
                        // The watch stream is supposed to be
                        // infinite, so seeing `None` here means cancel
                        // already fired in the SSE side. Quit cleanly.
                        debug!("tui: watch stream ended");
                        state.should_quit = true;
                    }
                }
            }
        }

        if state.should_quit {
            break;
        }
        terminal.draw(|f| render(&state, Instant::now(), f))?;
    }
    Ok(())
}

/// Spawn a blocking task that pumps crossterm input events onto an mpsc
/// channel. Crossterm's `event::read` is synchronous and blocks the
/// whole thread; running it on the tokio runtime would starve every
/// other task.
fn spawn_key_reader(cancel: CancellationToken) -> tokio::sync::mpsc::Receiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::task::spawn_blocking(move || {
        // 100ms polling is a compromise: low enough that `q` feels
        // instant, high enough that an idle TUI uses near-zero CPU.
        let tick = Duration::from_millis(100);
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match crossterm::event::poll(tick) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "tui: crossterm read failed");
                        break;
                    }
                },
                Ok(false) => continue,
                Err(err) => {
                    warn!(error = %err, "tui: crossterm poll failed");
                    break;
                }
            }
        }
        // Best-effort drop: the channel may already be closed if the
        // run loop exited first.
        drop(tx);
        io::Result::<()>::Ok(())
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use symphony_core::agent::{SessionId, ThreadId, TurnId};
    use symphony_core::events::OrchestratorEvent;
    use symphony_core::state_machine::{ClaimState, ReleaseReason};
    use symphony_core::tracker::IssueId;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn apply_connected_sets_status() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Connected {
            url: "http://x/events".into(),
        });
        assert_eq!(
            s.connection,
            ConnectionStatus::Connected {
                url: "http://x/events".into()
            }
        );
    }

    #[test]
    fn apply_reconnecting_carries_attempt_and_delay() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Reconnecting {
            attempt: 4,
            delay: Duration::from_millis(250),
        });
        assert_eq!(
            s.connection,
            ConnectionStatus::Reconnecting {
                attempt: 4,
                delay: Duration::from_millis(250)
            }
        );
    }

    #[test]
    fn apply_event_pushes_recent_event() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Event(OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        }));
        assert_eq!(s.recent_events.len(), 1);
        let ev = s.recent_events.back().unwrap();
        assert_eq!(ev.kind, "state_changed");
        assert_eq!(ev.identifier.as_deref(), Some("ENG-1"));
        assert!(ev.summary.contains("ENG-1"), "summary: {}", ev.summary);
        assert!(ev.summary.contains("running"), "summary: {}", ev.summary);
    }

    #[test]
    fn apply_lagged_pushes_recent_event_without_identifier() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Lagged { missed: 7 });
        let ev = s.recent_events.back().unwrap();
        assert_eq!(ev.kind, "lagged");
        assert!(ev.identifier.is_none());
        assert!(ev.summary.contains("7"));
    }

    #[test]
    fn handle_key_q_quits() {
        let mut s = AppState::default();
        assert!(s.handle_key(key(KeyCode::Char('q'))));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_esc_quits() {
        let mut s = AppState::default();
        assert!(s.handle_key(key(KeyCode::Esc)));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_ctrl_c_quits() {
        let mut s = AppState::default();
        let k = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(s.handle_key(k));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_ignores_release_events() {
        // A release of `q` from a previous foreground app must not quit
        // the TUI on launch.
        let mut s = AppState::default();
        let release = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(!s.handle_key(release));
        assert!(!s.should_quit);
    }

    #[test]
    fn handle_key_ignores_unbound_keys() {
        let mut s = AppState::default();
        assert!(!s.handle_key(key(KeyCode::Char('x'))));
        assert!(!s.should_quit);
    }

    #[test]
    fn render_pending_state_shows_connecting_label() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState::default();
        terminal
            .draw(|f| render(&state, Instant::now(), f))
            .unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("connecting"), "buffer was: {buf}");
        assert!(buf.contains("symphony watch"), "buffer was: {buf}");
        assert!(buf.contains("q quit"), "footer missing in: {buf}");
    }

    #[test]
    fn render_connected_state_shows_url() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        state.apply(&WatchEvent::Connected {
            url: "http://h/e".into(),
        });
        terminal
            .draw(|f| render(&state, Instant::now(), f))
            .unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("http://h/e"), "buffer: {buf}");
    }

    #[test]
    fn render_handles_resize() {
        // Successive draws against differently-sized backends model a
        // terminal resize. Both must succeed without panicking — that
        // is the entirety of the resize-safety contract for the
        // scaffold (no fixed-size assumptions in the layout).
        let mut t1 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut t2 = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let state = AppState::default();
        t1.draw(|f| render(&state, Instant::now(), f)).unwrap();
        t2.draw(|f| render(&state, Instant::now(), f)).unwrap();
    }

    fn session(t: &str, u: &str) -> SessionId {
        SessionId::new(&ThreadId::new(t), &TurnId::new(u))
    }

    #[test]
    fn state_changed_inserts_active_issue_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });
        let row = s.active_issues.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(row.identifier, "ENG-1");
        assert_eq!(row.state, ClaimState::Running);
        assert!(row.dispatched_at.is_none());
        assert!(row.backend.is_none());
    }

    #[test]
    fn dispatched_stamps_backend_and_attempt() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-2"),
            identifier: "ENG-2".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-2"),
            identifier: "ENG-2".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 2,
        });
        let row = s.active_issues.get(&IssueId::new("ENG-2")).unwrap();
        assert_eq!(row.backend.as_deref(), Some("claude"));
        assert_eq!(row.attempt, 2);
        assert!(row.dispatched_at.is_some());
        assert_eq!(row.state, ClaimState::Running);
    }

    #[test]
    fn dispatched_without_prior_state_change_still_inserts_row() {
        // The TUI may attach mid-flight, after the orchestrator has
        // already moved past the initial StateChanged. The table still
        // needs a row so the operator sees the work-in-progress.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-7"),
            identifier: "ENG-7".into(),
            session: session("t", "u"),
            backend: "tandem".into(),
            attempt: 1,
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-7")));
    }

    #[test]
    fn retry_queued_state_change_lifts_attempt() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-3"),
            identifier: "ENG-3".into(),
            previous: Some(ClaimState::Running),
            current: ClaimState::RetryQueued { attempt: 4 },
        });
        let row = s.active_issues.get(&IssueId::new("ENG-3")).unwrap();
        assert_eq!(row.attempt, 4);
        assert_eq!(row.state, ClaimState::RetryQueued { attempt: 4 });
    }

    #[test]
    fn released_removes_active_issue_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-4"),
            identifier: "ENG-4".into(),
            previous: None,
            current: ClaimState::Running,
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-4")));
        s.apply_orchestrator_event(&OrchestratorEvent::Released {
            issue: IssueId::new("ENG-4"),
            identifier: "ENG-4".into(),
            reason: ReleaseReason::Completed,
            final_state: None,
        });
        assert!(!s.active_issues.contains_key(&IssueId::new("ENG-4")));
    }

    #[test]
    fn reconciled_does_not_remove_rows() {
        // Released does the per-issue cleanup; if Reconciled also did,
        // the orchestrator's emission of both would double-remove.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-5"),
            identifier: "ENG-5".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Reconciled {
            dropped: vec![IssueId::new("ENG-5")],
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-5")));
    }

    #[test]
    fn format_elapsed_compact_branches() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3_600)), "1h00m");
        assert_eq!(format_elapsed(Duration::from_secs(3_660)), "1h01m");
        // Cap so an 8-char elapsed column never overflows.
        assert_eq!(format_elapsed(Duration::from_secs(100 * 3_600)), "99h+");
    }

    #[test]
    fn format_state_renders_running_and_retry() {
        assert_eq!(format_state(&ClaimState::Running), "running");
        assert_eq!(
            format_state(&ClaimState::RetryQueued { attempt: 7 }),
            "retry(7)"
        );
    }

    #[test]
    fn render_active_issues_panel_shows_rows_sorted_by_identifier() {
        // Insert ENG-2 first so HashMap iteration order is unlikely to
        // match identifier order without our explicit sort.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-2"),
            identifier: "ENG-2".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 1,
        });

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let now = Instant::now();
        terminal.draw(|f| render(&s, now, f)).unwrap();
        let buf = terminal.backend().to_string();

        assert!(buf.contains("active issues (2)"), "buf: {buf}");
        assert!(buf.contains("identifier"), "buf: {buf}");
        assert!(buf.contains("ENG-1"), "buf: {buf}");
        assert!(buf.contains("ENG-2"), "buf: {buf}");
        assert!(buf.contains("running"), "buf: {buf}");
        assert!(buf.contains("claude"), "buf: {buf}");
        // Sorted: ENG-1 must appear earlier in the buffer than ENG-2.
        let p1 = buf.find("ENG-1").unwrap();
        let p2 = buf.find("ENG-2").unwrap();
        assert!(p1 < p2, "rows not sorted by identifier: {buf}");
    }

    #[test]
    fn render_active_issues_panel_shows_placeholder_when_empty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let s = AppState::default();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("active issues (0)"), "buf: {buf}");
        assert!(buf.contains("no active issues"), "buf: {buf}");
    }

    #[test]
    fn thousands_formats_grouped_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(123), "123");
        assert_eq!(thousands(1_234), "1,234");
        assert_eq!(thousands(12_345), "12,345");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn dispatched_records_session_backend_for_cost_lookup() {
        let mut s = AppState::default();
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "claude".into(),
            attempt: 1,
        });
        assert_eq!(
            s.cost.session_backend.get(&sid).map(String::as_str),
            Some("claude")
        );
    }

    #[test]
    fn token_usage_replaces_not_sums_for_cumulative_emissions() {
        // Backend adapters emit cumulative TokenUsage on every update.
        // Two emissions on the same session must leave the panel at the
        // *latest* total, not the sum of both.
        let mut s = AppState::default();
        let sid = session("t", "u");
        let emit = |s: &mut AppState, input: u64, output: u64, total: u64| {
            s.apply_orchestrator_event(&OrchestratorEvent::Agent {
                issue: IssueId::new("ENG-1"),
                event: AgentEvent::TokenUsage {
                    session: sid.clone(),
                    usage: TokenUsage {
                        input,
                        output,
                        cached_input: 0,
                        total,
                    },
                },
            });
        };
        emit(&mut s, 100, 50, 150);
        emit(&mut s, 300, 200, 500);
        let agg = s.cost.aggregate_tokens();
        assert_eq!(agg.input, 300);
        assert_eq!(agg.output, 200);
        assert_eq!(agg.total, 500);
    }

    #[test]
    fn token_usage_sums_across_sessions() {
        let mut s = AppState::default();
        for (sid, input, output) in [
            (session("t", "u1"), 100u64, 50u64),
            (session("t", "u2"), 200, 75),
        ] {
            s.apply_orchestrator_event(&OrchestratorEvent::Agent {
                issue: IssueId::new("ENG-1"),
                event: AgentEvent::TokenUsage {
                    session: sid,
                    usage: TokenUsage {
                        input,
                        output,
                        cached_input: 0,
                        total: input + output,
                    },
                },
            });
        }
        let agg = s.cost.aggregate_tokens();
        assert_eq!(agg.input, 300);
        assert_eq!(agg.output, 125);
        assert_eq!(agg.total, 425);
    }

    #[test]
    fn aggregate_dollars_is_zero_without_pricing() {
        // No fabrication: an unconfigured pricing table yields $0.00
        // even with non-zero tokens.
        let mut s = AppState::default();
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "claude".into(),
            attempt: 1,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::TokenUsage {
                session: sid,
                usage: TokenUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    cached_input: 0,
                    total: 2_000_000,
                },
            },
        });
        assert_eq!(s.cost.aggregate_dollars(), 0.0);
    }

    #[test]
    fn aggregate_dollars_applies_per_backend_pricing() {
        let mut s = AppState::default();
        s.cost.pricing.insert(
            "claude".to_string(),
            CostModel {
                input_per_million_usd: 3.0,
                cached_input_per_million_usd: 0.30,
                output_per_million_usd: 15.0,
            },
        );
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "claude".into(),
            attempt: 1,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::TokenUsage {
                session: sid,
                usage: TokenUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    cached_input: 0,
                    total: 2_000_000,
                },
            },
        });
        // 1M input @ $3 + 1M output @ $15 = $18.00.
        let dollars = s.cost.aggregate_dollars();
        assert!(
            (dollars - 18.0).abs() < 1e-9,
            "expected ~18.00, got {dollars}"
        );
    }

    #[test]
    fn aggregate_dollars_unknown_backend_contributes_zero() {
        let mut s = AppState::default();
        s.cost.pricing.insert(
            "claude".to_string(),
            CostModel {
                input_per_million_usd: 3.0,
                cached_input_per_million_usd: 0.30,
                output_per_million_usd: 15.0,
            },
        );
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "exotic-backend".into(),
            attempt: 1,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::TokenUsage {
                session: sid,
                usage: TokenUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    cached_input: 0,
                    total: 2_000_000,
                },
            },
        });
        assert_eq!(s.cost.aggregate_dollars(), 0.0);
    }

    #[test]
    fn release_does_not_purge_session_tokens() {
        // Cost panel renders cumulative-since-launch; releasing a
        // completed issue must not rewind the dollar counter.
        let mut s = AppState::default();
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "claude".into(),
            attempt: 1,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::TokenUsage {
                session: sid,
                usage: TokenUsage {
                    input: 100,
                    output: 50,
                    cached_input: 0,
                    total: 150,
                },
            },
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Released {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            reason: ReleaseReason::Completed,
            final_state: None,
        });
        assert_eq!(s.cost.aggregate_tokens().total, 150);
    }

    #[test]
    fn render_cost_panel_shows_zero_dollars_when_unconfigured() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let s = AppState::default();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("cost"), "buf: {buf}");
        assert!(buf.contains("$0.00"), "buf: {buf}");
        assert!(buf.contains("in 0"), "buf: {buf}");
    }

    #[test]
    fn render_cost_panel_shows_aggregated_tokens_and_dollars() {
        let mut s = AppState::default();
        s.cost.pricing.insert(
            "claude".to_string(),
            CostModel {
                input_per_million_usd: 3.0,
                cached_input_per_million_usd: 0.30,
                output_per_million_usd: 15.0,
            },
        );
        let sid = session("t", "u");
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: sid.clone(),
            backend: "claude".into(),
            attempt: 1,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::TokenUsage {
                session: sid,
                usage: TokenUsage {
                    input: 12_345,
                    output: 6_789,
                    cached_input: 1_000,
                    total: 19_134,
                },
            },
        });
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("12,345"), "in tokens missing: {buf}");
        assert!(buf.contains("6,789"), "out tokens missing: {buf}");
        assert!(buf.contains("1,000"), "cached missing: {buf}");
        assert!(buf.contains("19,134"), "total missing: {buf}");
        // 12_345 @ $3 + 1_000 @ $0.30 + 6_789 @ $15 = $0.139... ≈ $0.14
        // (12345 - 1000)*3/1e6 + 1000*0.30/1e6 + 6789*15/1e6
        // = 0.034035 + 0.0003 + 0.101835 = 0.13617
        assert!(buf.contains("$0.14"), "dollars missing: {buf}");
    }

    #[test]
    fn render_active_issues_shows_elapsed_for_dispatched_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-9"),
            identifier: "ENG-9".into(),
            session: session("t", "u"),
            backend: "codex".into(),
            attempt: 1,
        });
        // Simulate a 90-second-old dispatch by rewinding the row's
        // timestamp before we render against a fixed `now`.
        let dispatched = Instant::now();
        s.active_issues
            .get_mut(&IssueId::new("ENG-9"))
            .unwrap()
            .dispatched_at = Some(dispatched);
        let now = dispatched + Duration::from_secs(90);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, now, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("1m30s"), "elapsed missing in buf: {buf}");
    }

    // ---- recent-events log panel ---------------------------------------

    #[test]
    fn dispatched_pushes_recent_event_with_backend_label() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 2,
        });
        let ev = s.recent_events.back().unwrap();
        assert_eq!(ev.kind, "dispatched");
        assert!(ev.summary.contains("claude"), "summary: {}", ev.summary);
        assert!(ev.summary.contains("attempt 2"), "summary: {}", ev.summary);
    }

    #[test]
    fn agent_event_uses_identifier_cache_for_label() {
        let mut s = AppState::default();
        // Seed the cache via a StateChanged so the Agent event below
        // resolves to the human identifier rather than "?".
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-1"),
            identifier: "ENG-42".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("id-1"),
            event: AgentEvent::Message {
                session: session("t", "u"),
                role: "assistant".into(),
                content: "hello world".into(),
            },
        });
        let ev = s.recent_events.back().unwrap();
        assert_eq!(ev.kind, "agent");
        assert_eq!(ev.identifier.as_deref(), Some("ENG-42"));
        assert!(ev.summary.starts_with("ENG-42"), "summary: {}", ev.summary);
        assert!(ev.summary.contains("assistant"), "summary: {}", ev.summary);
    }

    #[test]
    fn agent_event_without_cached_identifier_uses_placeholder() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("id-x"),
            event: AgentEvent::Started {
                session: session("t", "u"),
                issue: IssueId::new("id-x"),
            },
        });
        let ev = s.recent_events.back().unwrap();
        assert!(ev.summary.starts_with("? "), "summary: {}", ev.summary);
        assert!(ev.identifier.is_none());
    }

    #[test]
    fn ring_buffer_caps_at_recent_events_cap() {
        let mut s = AppState::default();
        for i in 0..(RECENT_EVENTS_CAP + 25) {
            s.push_recent("agent", format!("evt {i}"), Some(format!("ENG-{i}")));
        }
        assert_eq!(s.recent_events.len(), RECENT_EVENTS_CAP);
        // Oldest 25 entries should have been evicted; the head is now
        // the 25th-pushed event.
        assert_eq!(s.recent_events.front().unwrap().summary, "evt 25");
        assert_eq!(
            s.recent_events.back().unwrap().summary,
            format!("evt {}", RECENT_EVENTS_CAP + 24)
        );
    }

    #[test]
    fn truncate_one_line_caps_long_content_and_collapses_newlines() {
        assert_eq!(truncate_one_line("hi", 10), "hi");
        assert_eq!(truncate_one_line("a\nb\tc", 10), "a b c");
        let long = "x".repeat(100);
        let out = truncate_one_line(&long, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn handle_key_f_enters_filter_input_mode() {
        let mut s = AppState::default();
        assert!(s.handle_key(key(KeyCode::Char('f'))));
        assert!(s.filter_input_mode);
        assert!(!s.should_quit);
    }

    #[test]
    fn handle_key_in_filter_mode_appends_chars_and_does_not_quit_on_q() {
        let mut s = AppState {
            filter_input_mode: true,
            ..Default::default()
        };
        s.handle_key(key(KeyCode::Char('q')));
        s.handle_key(key(KeyCode::Char('u')));
        s.handle_key(key(KeyCode::Char('e')));
        assert_eq!(s.event_filter, "que");
        assert!(!s.should_quit, "q while filtering must not quit");
    }

    #[test]
    fn handle_key_filter_backspace_pops_char() {
        let mut s = AppState {
            filter_input_mode: true,
            event_filter: "ENG".into(),
            ..Default::default()
        };
        s.handle_key(key(KeyCode::Backspace));
        assert_eq!(s.event_filter, "EN");
    }

    #[test]
    fn handle_key_filter_enter_commits_and_exits_input_mode() {
        let mut s = AppState {
            filter_input_mode: true,
            event_filter: "ENG-1".into(),
            ..Default::default()
        };
        s.handle_key(key(KeyCode::Enter));
        assert!(!s.filter_input_mode);
        assert_eq!(s.event_filter, "ENG-1");
    }

    #[test]
    fn handle_key_filter_esc_cancels_and_clears_filter() {
        let mut s = AppState {
            filter_input_mode: true,
            event_filter: "ENG".into(),
            ..Default::default()
        };
        s.handle_key(key(KeyCode::Esc));
        assert!(!s.filter_input_mode);
        assert!(s.event_filter.is_empty());
        assert!(!s.should_quit);
    }

    #[test]
    fn handle_key_ctrl_c_quits_even_in_filter_mode() {
        let mut s = AppState {
            filter_input_mode: true,
            ..Default::default()
        };
        let k = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(s.handle_key(k));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_filter_caps_input_length() {
        let mut s = AppState {
            filter_input_mode: true,
            ..Default::default()
        };
        for _ in 0..200 {
            s.handle_key(key(KeyCode::Char('x')));
        }
        assert_eq!(s.event_filter.len(), 64);
    }

    #[test]
    fn render_recent_events_panel_shows_summaries() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            session: session("t", "u"),
            backend: "codex".into(),
            attempt: 1,
        });
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("recent events (1)"), "buf: {buf}");
        assert!(buf.contains("dispatched"), "buf: {buf}");
        assert!(buf.contains("ENG-1"), "buf: {buf}");
        assert!(buf.contains("codex"), "buf: {buf}");
    }

    #[test]
    fn render_recent_events_filter_hides_non_matching_rows() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-2"),
            identifier: "ENG-2".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.event_filter = "ENG-1".into();

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();

        // Title shows visible/total + active filter.
        assert!(buf.contains("recent events (1/2)"), "buf: {buf}");
        assert!(buf.contains("[filter: ENG-1]"), "buf: {buf}");
        // The non-matching event-row text must not appear in the
        // recent-events region. (`ENG-2` still appears in the
        // active-issues table — so we only assert no `ENG-2 → running`
        // *summary* line shows up in the events panel by checking the
        // summary string is absent.)
        let summary_for_eng2 = format!("ENG-2 → {}", format_state(&ClaimState::Running));
        assert!(
            !buf.contains(&summary_for_eng2),
            "filter leaked ENG-2 summary into events panel: {buf}"
        );
    }

    #[test]
    fn render_recent_events_panel_empty_state() {
        let s = AppState::default();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("recent events (0)"), "buf: {buf}");
        assert!(buf.contains("no events yet"), "buf: {buf}");
    }

    #[test]
    fn render_footer_shows_filter_indicator_when_set() {
        let s = AppState {
            event_filter: "ENG-1".into(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("q quit"), "buf: {buf}");
        assert!(buf.contains("f filter"), "buf: {buf}");
        assert!(buf.contains("[filter: ENG-1]"), "buf: {buf}");
    }

    #[test]
    fn render_footer_in_filter_mode_shows_input_line() {
        let s = AppState {
            filter_input_mode: true,
            event_filter: "EN".into(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("filter:"), "buf: {buf}");
        assert!(buf.contains("EN"), "buf: {buf}");
        assert!(buf.contains("Enter"), "buf: {buf}");
        assert!(buf.contains("Esc"), "buf: {buf}");
    }

    #[test]
    fn agent_token_usage_summary_includes_totals() {
        let summary = format_agent_event(
            "ENG-9",
            &AgentEvent::TokenUsage {
                session: session("t", "u"),
                usage: TokenUsage {
                    input: 100,
                    output: 50,
                    cached_input: 0,
                    total: 150,
                },
            },
        );
        assert!(summary.contains("ENG-9"));
        assert!(summary.contains("100"));
        assert!(summary.contains("50"));
        assert!(summary.contains("150"));
    }

    // ---- tandem-activity panel ----------------------------------------

    fn dispatch_tandem(state: &mut AppState, issue: &str, session_str: &str) {
        state.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new(issue),
            identifier: issue.into(),
            session: session(session_str, "u"),
            backend: TANDEM_BACKEND_LABEL.into(),
            attempt: 1,
        });
    }

    fn agent_msg(state: &mut AppState, issue: &str, session_str: &str, content: &str) {
        state.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new(issue),
            event: AgentEvent::Message {
                session: session(session_str, "u"),
                role: "assistant".into(),
                content: content.into(),
            },
        });
    }

    #[test]
    fn tandem_dispatch_creates_tandem_session() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(t.identifier, "ENG-1");
        assert!(t.lead_session.is_some());
        assert!(t.follower_session.is_none());
        assert!(t.last_active_role.is_none());
    }

    #[test]
    fn non_tandem_dispatch_does_not_create_tandem_session() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 1,
        });
        assert!(s.tandem_sessions.is_empty());
    }

    #[test]
    fn release_drops_tandem_session() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        assert!(!s.tandem_sessions.is_empty());
        s.apply_orchestrator_event(&OrchestratorEvent::Released {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            reason: ReleaseReason::Completed,
            final_state: None,
        });
        assert!(s.tandem_sessions.is_empty());
    }

    #[test]
    fn agent_event_attributes_lead_to_dispatch_session() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "lead-thread", "lead speaks");
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(t.lead_messages, 1);
        assert_eq!(t.follower_messages, 0);
        assert_eq!(t.last_active_role, Some(TandemRole::Lead));
    }

    #[test]
    fn first_other_session_is_adopted_as_follower() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "follower-thread", "review");
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert!(t.follower_session.is_some());
        assert_eq!(t.follower_messages, 1);
        assert_eq!(t.last_active_role, Some(TandemRole::Follower));
    }

    #[test]
    fn tool_use_counts_per_side() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        // Adopt follower first.
        agent_msg(&mut s, "ENG-1", "follower-thread", "hi");
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::ToolUse {
                session: session("lead-thread", "u"),
                tool: "rg".into(),
                input: serde_json::Value::Null,
            },
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Agent {
            issue: IssueId::new("ENG-1"),
            event: AgentEvent::ToolUse {
                session: session("follower-thread", "u"),
                tool: "patch".into(),
                input: serde_json::Value::Null,
            },
        });
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(t.lead_tools, 1);
        assert_eq!(t.follower_tools, 1);
    }

    #[test]
    fn agreement_rate_zero_when_one_side_silent() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "lead-thread", "alpha beta gamma");
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(t.agreement_rate(), 0.0);
    }

    #[test]
    fn agreement_rate_jaccard_over_word_bags() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "lead-thread", "alpha beta gamma");
        agent_msg(&mut s, "ENG-1", "follower-thread", "beta gamma delta");
        // bags: {alpha,beta,gamma} and {beta,gamma,delta}
        // intersection = 2 (beta, gamma); union = 4 → 0.5
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert!(
            (t.agreement_rate() - 0.5).abs() < 1e-9,
            "rate was {}",
            t.agreement_rate()
        );
    }

    #[test]
    fn redispatch_resets_tandem_session_counts() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "lead-thread", "foo");
        // A retry produces a new Dispatched. Stale counts must not carry.
        dispatch_tandem(&mut s, "ENG-1", "lead-thread-2");
        let t = s.tandem_sessions.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(t.lead_messages, 0);
        assert_eq!(t.follower_messages, 0);
    }

    #[test]
    fn render_hides_tandem_panel_when_no_tandem_sessions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let s = AppState::default();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(
            !buf.contains("tandem activity"),
            "tandem panel leaked into non-tandem UI: {buf}"
        );
    }

    #[test]
    fn render_shows_tandem_panel_with_rows() {
        let mut s = AppState::default();
        dispatch_tandem(&mut s, "ENG-1", "lead-thread");
        agent_msg(&mut s, "ENG-1", "lead-thread", "draft text here");
        agent_msg(&mut s, "ENG-1", "follower-thread", "draft text approved");

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("tandem activity (1)"), "buf: {buf}");
        assert!(buf.contains("ENG-1"), "buf: {buf}");
        assert!(buf.contains("follower"), "buf: {buf}");
        assert!(buf.contains("msg 1"), "lead msg count missing: {buf}");
    }

    #[test]
    fn extract_word_tokens_lowercases_and_splits() {
        let v: Vec<String> = extract_word_tokens("Hello, World! 123-foo").collect();
        assert_eq!(v, vec!["hello", "world", "123", "foo"]);
    }

    #[test]
    fn event_kind_colour_distinct_per_variant() {
        // Spot-check that the six variants do not collapse to the same
        // colour — a regression here would make the panel unscannable.
        let palette = [
            event_kind_colour("state_changed"),
            event_kind_colour("dispatched"),
            event_kind_colour("agent"),
            event_kind_colour("retry_scheduled"),
            event_kind_colour("reconciled"),
            event_kind_colour("released"),
            event_kind_colour("lagged"),
        ];
        let mut seen = std::collections::HashSet::new();
        for c in palette {
            assert!(seen.insert(format!("{c:?}")), "colour collision: {c:?}");
        }
    }
}
