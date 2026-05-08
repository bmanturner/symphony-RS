//! Tandem telemetry — per-agent token counts, agreement rate, cost-per-turn.
//!
//! This module is the Phase 6 closing deliverable. Strategies in
//! `draft_review`, `split_implement`, and `consensus` already capture the
//! per-role event vectors during execution; this module turns those
//! vectors into the structured aggregates that the Phase 8 status surface
//! and any external billing/observability path can consume.
//!
//! # Why pure functions
//!
//! Telemetry is a *read-only projection* of an event sequence. Keeping it
//! out of the strategy hot path means:
//!
//! - Strategies remain focused on event synthesis; they don't have to
//!   thread an aggregator through their async plumbing.
//! - Tests can assemble synthetic event vectors and assert exact
//!   aggregates without spinning up runners.
//! - A future caller (e.g. the orchestrator's per-turn audit log) can
//!   compute telemetry from a recording of the stream after the fact.
//!
//! # Agreement rate
//!
//! "Agreement" between two coding agents on the same task is fuzzy by
//! nature, so we pick a single, honest, cheaply-computable definition and
//! commit to it: the [Jaccard similarity] of the bag of lower-cased
//! alphanumeric tokens drawn from the two agents' [`AgentEvent::Message`]
//! contents.
//!
//! - 1.0 means identical token sets. A clean overlap score; not a claim
//!   that the two agents made the same *decision*.
//! - 0.0 means disjoint vocabularies — they were talking about different
//!   things, which is a strong negative signal regardless of strategy.
//! - Either side empty → 0.0 (we can't agree about nothing).
//!
//! Strategy-specific notions of "did the follower accept the lead's
//! draft?" or "did the consensus scorer pick a clear winner?" are *not*
//! folded in here — those are decisions the strategy already made and
//! recorded in the synthesised stream. Telemetry stays a content-level
//! projection so callers can layer their own strategy-aware
//! interpretation on top without us pre-mixing them.
//!
//! [Jaccard similarity]: https://en.wikipedia.org/wiki/Jaccard_index
//!
//! # Cost
//!
//! [`CostModel`] holds per-million-token rates for input, cached input,
//! and output. Defaults are deliberately set to zero so a misconfigured
//! orchestrator reports `$0.00` rather than a fabricated number — adapters
//! and operators must opt in to billing by supplying real rates.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use symphony_core::agent::{AgentEvent, TokenUsage};

use super::TandemStrategy;

// ---------------------------------------------------------------------------
// Cost model
// ---------------------------------------------------------------------------

/// Per-million-token pricing applied to a [`TokenUsage`] snapshot to derive
/// a dollar figure.
///
/// All rates are USD per 1,000,000 tokens — matching how every public
/// model card publishes its prices, and keeping the stored numbers in a
/// human-readable range (single- or low-double-digit dollars rather than
/// micro-dollar fractions per token).
///
/// Zero defaults are intentional: they make `cost_usd` a no-op until an
/// operator has chosen to wire pricing through `WORKFLOW.md`. We never
/// guess a price.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Price for fresh (un-cached) input tokens, USD per 1M.
    pub input_per_million_usd: f64,
    /// Price for input tokens served from prompt cache, USD per 1M.
    /// Backends that don't distinguish cached input (e.g. Codex today)
    /// should leave this equal to `input_per_million_usd`; if the adapter
    /// reports `cached_input = 0` it falls out of the math regardless.
    pub cached_input_per_million_usd: f64,
    /// Price for output tokens, USD per 1M.
    pub output_per_million_usd: f64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            input_per_million_usd: 0.0,
            cached_input_per_million_usd: 0.0,
            output_per_million_usd: 0.0,
        }
    }
}

impl CostModel {
    /// Apply this pricing to a token snapshot. Cached input is billed at
    /// the cached rate; the remaining `input - cached_input` is billed at
    /// the full input rate. We saturate at 0 if `cached_input` exceeds
    /// `input` (some adapters double-count when the model serves an
    /// entirely-cached turn).
    pub fn cost_usd(&self, tokens: &TokenUsage) -> f64 {
        let cached = tokens.cached_input.min(tokens.input);
        let fresh = tokens.input - cached;
        let per = 1_000_000.0;
        (fresh as f64) * self.input_per_million_usd / per
            + (cached as f64) * self.cached_input_per_million_usd / per
            + (tokens.output as f64) * self.output_per_million_usd / per
    }
}

// ---------------------------------------------------------------------------
// Per-agent aggregate
// ---------------------------------------------------------------------------

/// Tokens, message/tool counts, and cost for one of the two agents.
///
/// Built by [`aggregate_agent`] and assembled into a [`TandemTelemetry`]
/// by [`compute`]. Kept publicly constructible because the Phase 8 status
/// surface might want to assemble synthetic snapshots in tests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTelemetry {
    /// Sum of every [`AgentEvent::TokenUsage`] this agent emitted. The
    /// last snapshot in a turn typically carries the cumulative total
    /// already, but we sum to be robust against adapters that emit
    /// per-step deltas instead.
    pub tokens: TokenUsage,
    /// Number of [`AgentEvent::Message`] events. Cheap proxy for "how
    /// chatty was this side."
    pub messages: u32,
    /// Number of [`AgentEvent::ToolUse`] events. Cheap proxy for "how
    /// much work did this side dispatch."
    pub tool_calls: u32,
    /// Dollars spent on this agent for the captured window, computed via
    /// [`CostModel::cost_usd`] applied to `tokens`.
    pub cost_usd: f64,
}

/// Reduce one agent's event slice into an [`AgentTelemetry`] aggregate.
///
/// Robust to multiple `TokenUsage` events in the same window: adapters
/// emitting deltas accumulate naturally, while adapters emitting a single
/// cumulative total at completion produce the same answer (the sum has
/// only one term).
pub fn aggregate_agent(events: &[AgentEvent], model: &CostModel) -> AgentTelemetry {
    let mut acc = AgentTelemetry::default();
    for ev in events {
        match ev {
            AgentEvent::Message { .. } => acc.messages += 1,
            AgentEvent::ToolUse { .. } => acc.tool_calls += 1,
            AgentEvent::TokenUsage { usage, .. } => {
                acc.tokens.input += usage.input;
                acc.tokens.output += usage.output;
                acc.tokens.cached_input += usage.cached_input;
                acc.tokens.total += usage.total;
            }
            _ => {}
        }
    }
    acc.cost_usd = model.cost_usd(&acc.tokens);
    acc
}

// ---------------------------------------------------------------------------
// Composite telemetry
// ---------------------------------------------------------------------------

/// Tandem-level telemetry: both agents plus the strategy and an
/// agreement-rate scalar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TandemTelemetry {
    /// Which strategy produced these aggregates. Carried so a downstream
    /// renderer can format e.g. "lead drafted, follower reviewed" copy.
    pub strategy: TandemStrategy,
    /// Aggregate for the lead agent.
    pub lead: AgentTelemetry,
    /// Aggregate for the follower agent.
    pub follower: AgentTelemetry,
    /// Bag-of-tokens Jaccard similarity over the two agents' message
    /// contents — see the module-level docs for the exact definition.
    /// Range `[0.0, 1.0]`. NaN-free: empty intersection on empty union
    /// short-circuits to 0.0.
    pub agreement_rate: f64,
    /// `lead.cost_usd + follower.cost_usd`. Stored explicitly rather than
    /// asking every consumer to recompute it; cheap and avoids drift.
    pub cost_per_turn_usd: f64,
}

/// Build a [`TandemTelemetry`] from the two captured event slices.
pub fn compute(
    lead_events: &[AgentEvent],
    follower_events: &[AgentEvent],
    strategy: TandemStrategy,
    model: &CostModel,
) -> TandemTelemetry {
    let lead = aggregate_agent(lead_events, model);
    let follower = aggregate_agent(follower_events, model);
    let agreement_rate = jaccard_message_similarity(lead_events, follower_events);
    let cost_per_turn_usd = lead.cost_usd + follower.cost_usd;
    TandemTelemetry {
        strategy,
        lead,
        follower,
        agreement_rate,
        cost_per_turn_usd,
    }
}

// ---------------------------------------------------------------------------
// Agreement-rate computation
// ---------------------------------------------------------------------------

/// Jaccard similarity over the bag of lowercase alphanumeric tokens drawn
/// from `Message.content` events on each side. See module docs for why
/// this definition.
fn jaccard_message_similarity(lhs: &[AgentEvent], rhs: &[AgentEvent]) -> f64 {
    let lhs_tokens = collect_message_tokens(lhs);
    let rhs_tokens = collect_message_tokens(rhs);
    if lhs_tokens.is_empty() || rhs_tokens.is_empty() {
        // No way to "agree about nothing." Returning 0.0 instead of 1.0
        // (the empty-set convention) so an empty side reads as "we have
        // no signal" rather than "perfect agreement," which would be
        // dangerously misleading on a status panel.
        return 0.0;
    }
    let intersection = lhs_tokens.intersection(&rhs_tokens).count() as f64;
    let union = lhs_tokens.union(&rhs_tokens).count() as f64;
    intersection / union
}

/// Tokenize every `Message.content` in `events` into a set of
/// lowercase alphanumeric runs. Punctuation and whitespace are
/// separators; one-character tokens are kept (they discriminate file
/// names like `a.rs` after we split on `.`).
fn collect_message_tokens(events: &[AgentEvent]) -> HashSet<String> {
    let mut set = HashSet::new();
    for ev in events {
        if let AgentEvent::Message { content, .. } = ev {
            for raw in content.split(|c: char| !c.is_alphanumeric()) {
                if raw.is_empty() {
                    continue;
                }
                set.insert(raw.to_lowercase());
            }
        }
    }
    set
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_core::agent::{CompletionReason, SessionId};

    fn msg(session: &str, content: &str) -> AgentEvent {
        AgentEvent::Message {
            session: SessionId::from_raw(session),
            role: "assistant".into(),
            content: content.into(),
        }
    }

    fn tool(session: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolUse {
            session: SessionId::from_raw(session),
            tool: name.into(),
            input: serde_json::json!({}),
        }
    }

    fn tokens(session: &str, input: u64, output: u64, cached_input: u64, total: u64) -> AgentEvent {
        AgentEvent::TokenUsage {
            session: SessionId::from_raw(session),
            usage: TokenUsage {
                input,
                output,
                cached_input,
                total,
            },
        }
    }

    fn done(session: &str) -> AgentEvent {
        AgentEvent::Completed {
            session: SessionId::from_raw(session),
            reason: CompletionReason::Success,
        }
    }

    fn priced() -> CostModel {
        CostModel {
            input_per_million_usd: 3.0,
            cached_input_per_million_usd: 0.30,
            output_per_million_usd: 15.0,
        }
    }

    // ----- CostModel -----

    #[test]
    fn cost_model_default_is_zero() {
        let model = CostModel::default();
        let tu = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cached_input: 1_000_000,
            total: 2_000_000,
        };
        assert_eq!(model.cost_usd(&tu), 0.0);
    }

    #[test]
    fn cost_model_separates_fresh_from_cached_input() {
        let model = priced();
        // 1M total input, 250k served from cache → 750k fresh × $3 +
        // 250k cached × $0.30 + 500k output × $15.
        let tu = TokenUsage {
            input: 1_000_000,
            output: 500_000,
            cached_input: 250_000,
            total: 1_500_000,
        };
        let expected = 0.75 * 3.0 + 0.25 * 0.30 + 0.5 * 15.0;
        assert!((model.cost_usd(&tu) - expected).abs() < 1e-9);
    }

    #[test]
    fn cost_model_clamps_overlarge_cached_input() {
        // Some adapters double-count; cached must not exceed input. We
        // saturate rather than producing a negative fresh count.
        let model = priced();
        let tu = TokenUsage {
            input: 100,
            output: 0,
            cached_input: 1_000,
            total: 100,
        };
        let expected = 100.0 * 0.30 / 1_000_000.0;
        assert!((model.cost_usd(&tu) - expected).abs() < 1e-12);
    }

    // ----- aggregate_agent -----

    #[test]
    fn aggregate_counts_messages_tools_and_sums_tokens() {
        let events = vec![
            msg("s", "hello"),
            tool("s", "Read"),
            tool("s", "Edit"),
            tokens("s", 100, 50, 20, 150),
            tokens("s", 200, 100, 30, 300),
            done("s"),
        ];
        let agg = aggregate_agent(&events, &priced());
        assert_eq!(agg.messages, 1);
        assert_eq!(agg.tool_calls, 2);
        assert_eq!(
            agg.tokens,
            TokenUsage {
                input: 300,
                output: 150,
                cached_input: 50,
                total: 450
            }
        );
        // 250 fresh × $3 + 50 cached × $0.30 + 150 × $15, all per million.
        let expected = 250.0 * 3.0 / 1e6 + 50.0 * 0.30 / 1e6 + 150.0 * 15.0 / 1e6;
        assert!((agg.cost_usd - expected).abs() < 1e-12);
    }

    #[test]
    fn aggregate_handles_empty_events() {
        let agg = aggregate_agent(&[], &priced());
        assert_eq!(agg, AgentTelemetry::default());
    }

    // ----- jaccard / agreement_rate -----

    #[test]
    fn agreement_rate_is_one_for_identical_messages() {
        let lead = vec![msg("l", "ship the migration on tuesday")];
        let foll = vec![msg("f", "Ship the migration on TUESDAY!")];
        let r = jaccard_message_similarity(&lead, &foll);
        assert!((r - 1.0).abs() < 1e-12);
    }

    #[test]
    fn agreement_rate_is_zero_for_disjoint_messages() {
        let lead = vec![msg("l", "alpha beta gamma")];
        let foll = vec![msg("f", "delta epsilon zeta")];
        assert_eq!(jaccard_message_similarity(&lead, &foll), 0.0);
    }

    #[test]
    fn agreement_rate_partial_overlap() {
        // Lead: {a, b, c, d}; Follower: {c, d, e, f}. Intersection = 2,
        // union = 6 → 1/3.
        let lead = vec![msg("l", "a b c d")];
        let foll = vec![msg("f", "c d e f")];
        let r = jaccard_message_similarity(&lead, &foll);
        assert!((r - (2.0 / 6.0)).abs() < 1e-12);
    }

    #[test]
    fn agreement_rate_zero_when_either_side_empty() {
        let lead = vec![msg("l", "anything")];
        assert_eq!(jaccard_message_similarity(&lead, &[]), 0.0);
        assert_eq!(jaccard_message_similarity(&[], &lead), 0.0);
        assert_eq!(jaccard_message_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn agreement_rate_ignores_non_message_events() {
        // Tool calls and token snapshots must not contribute to the
        // vocabulary even though they may carry textual fields elsewhere.
        let lead = vec![msg("l", "shared word"), tool("l", "Read")];
        let foll = vec![msg("f", "shared word"), tokens("f", 1, 1, 0, 2)];
        assert_eq!(jaccard_message_similarity(&lead, &foll), 1.0);
    }

    // ----- compute -----

    #[test]
    fn compute_assembles_full_telemetry() {
        let lead = vec![
            msg("lead", "plan the refactor"),
            tool("lead", "Grep"),
            tokens("lead", 1_000, 500, 0, 1_500),
            done("lead"),
        ];
        let foll = vec![
            msg("foll", "execute the refactor"),
            tool("foll", "Edit"),
            tool("foll", "Edit"),
            tokens("foll", 800, 400, 100, 1_200),
            done("foll"),
        ];
        let t = compute(&lead, &foll, TandemStrategy::SplitImplement, &priced());
        assert_eq!(t.strategy, TandemStrategy::SplitImplement);
        assert_eq!(t.lead.messages, 1);
        assert_eq!(t.lead.tool_calls, 1);
        assert_eq!(t.follower.tool_calls, 2);
        // Both messages share "the" and "refactor"; vocabulary union
        // is {plan, the, refactor, execute} → 2/4 = 0.5.
        assert!((t.agreement_rate - 0.5).abs() < 1e-12);
        let expected_cost = t.lead.cost_usd + t.follower.cost_usd;
        assert!((t.cost_per_turn_usd - expected_cost).abs() < 1e-12);
    }

    #[test]
    fn compute_round_trips_through_serde_json() {
        // Telemetry is destined for the Phase 8 SSE wire format and any
        // billing dump; lock the stable shape with a round-trip.
        let t = compute(
            &[msg("l", "alpha"), tokens("l", 10, 5, 0, 15), done("l")],
            &[msg("f", "alpha"), tokens("f", 10, 5, 0, 15), done("f")],
            TandemStrategy::Consensus,
            &priced(),
        );
        let json = serde_json::to_string(&t).unwrap();
        let back: TandemTelemetry = serde_json::from_str(&json).unwrap();
        // f64 round-trip through JSON can drift in the last ULP, so
        // compare integer fields exactly and floats with a tiny epsilon.
        assert_eq!(back.strategy, t.strategy);
        assert_eq!(back.lead.tokens, t.lead.tokens);
        assert_eq!(back.lead.messages, t.lead.messages);
        assert_eq!(back.lead.tool_calls, t.lead.tool_calls);
        assert_eq!(back.follower.tokens, t.follower.tokens);
        assert!((back.lead.cost_usd - t.lead.cost_usd).abs() < 1e-9);
        assert!((back.follower.cost_usd - t.follower.cost_usd).abs() < 1e-9);
        assert!((back.agreement_rate - t.agreement_rate).abs() < 1e-9);
        assert!((back.cost_per_turn_usd - t.cost_per_turn_usd).abs() < 1e-9);
    }

    #[test]
    fn compute_zero_cost_when_no_tokens_emitted() {
        let lead = vec![msg("l", "no usage emitted"), done("l")];
        let foll = vec![msg("f", "still nothing"), done("f")];
        let t = compute(&lead, &foll, TandemStrategy::DraftReview, &priced());
        assert_eq!(t.cost_per_turn_usd, 0.0);
        assert_eq!(t.lead.tokens, TokenUsage::default());
        assert_eq!(t.follower.tokens, TokenUsage::default());
    }
}
