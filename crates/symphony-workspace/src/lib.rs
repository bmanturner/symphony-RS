//! Per-issue workspace lifecycle for Symphony-RS.
//!
//! Implements SPEC §4.2: maps an issue identifier to a workspace path on
//! disk under a configured root, sanitizing per the spec's
//! `[A-Za-z0-9._-]` rule. Provides three lifecycle hooks:
//!
//! - `before_run`  — invoked before the agent session starts (e.g. git
//!   checkout, dependency install).
//! - `after_create` — invoked the first time a workspace is created.
//! - `after_release` — invoked when an issue reaches a terminal state and
//!   the workspace is being released.
//!
//! All path operations are containment-checked: a malicious issue id
//! must never resolve outside the configured root, even via `..` or
//! Unicode tricks.
