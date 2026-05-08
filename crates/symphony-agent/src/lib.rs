//! Agent runner abstraction for Symphony-RS.
//!
//! This crate is one of two deliberate deviations from the upstream
//! Symphony spec (the other is the tracker abstraction). Where SPEC §3.1
//! names a single "Agent Runner" coupled to Codex's app-server protocol,
//! we expose:
//!
//! 1. The `AgentRunner` trait, which yields a normalized stream of
//!    `AgentEvent`s (Started / Message / ToolUse / TokenUsage /
//!    RateLimit / Completed / Failed) regardless of backend.
//! 2. A `CodexRunner` that speaks Codex's JSONL `app-server` protocol.
//! 3. A `ClaudeRunner` that spawns `claude -p --output-format
//!    stream-json --permission-mode bypassPermissions` and parses
//!    Claude Code's stream-json events.
//! 4. A `TandemRunner` that runs two inner runners concurrently with
//!    one of three lead/follower strategies (`draft-review`,
//!    `split-implement`, `consensus`).
//!
//! The orchestrator only ever sees `AgentEvent` values. All backend
//! protocol differences are absorbed here.
