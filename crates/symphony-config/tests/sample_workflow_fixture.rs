//! Integration test for the canonical `tests/fixtures/sample-workflow/WORKFLOW.md`.
//!
//! This fixture is the worked example shipped with the repo. It is also
//! the target of the `symphony validate` smoke test that lands in Phase 7.
//! Anchoring a config-layer test on it now guarantees CI fails the moment
//! the fixture drifts out of step with the typed [`WorkflowConfig`] schema
//! — long before the CLI smoke test runs against a stale file.
//!
//! The fixture lives at the workspace root rather than inside a crate so
//! every layer (config, CLI, future end-to-end runners) can point at the
//! same canonical example without duplicating it.
//!
//! ## What this test asserts
//!
//! - The fixture loads cleanly via [`WorkflowLoader::from_path`].
//! - Front-matter values match what the file declares (catches accidental
//!   reformatting that changes semantics).
//! - The typed [`WorkflowConfig::validate`] check passes — the same gate
//!   `symphony validate` will run.
//! - The prompt body is non-empty and survives trimming.

use std::path::PathBuf;

use symphony_config::{AgentKind, TrackerKind, WorkflowLoader};

/// Resolve the fixture path from the crate's manifest dir up to the
/// workspace root. Done at runtime (not via `include_str!`) so the
/// loader's real on-disk read path is exercised, and so a missing or
/// mis-located fixture surfaces as a `Read` error with the exact path
/// rather than a compile-time include failure.
fn fixture_path() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir)
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("sample-workflow")
        .join("WORKFLOW.md")
}

#[test]
fn sample_fixture_loads_and_validates() {
    let path = fixture_path();
    let loaded = WorkflowLoader::from_path(&path)
        .unwrap_or_else(|e| panic!("failed to load sample fixture at {}: {e}", path.display()));

    // Tracker section reflects the SPEC default (Linear) plus an explicit
    // project_slug so `validate()` is satisfied.
    assert_eq!(loaded.config.tracker.kind, TrackerKind::Linear);
    assert_eq!(loaded.config.tracker.project_slug.as_deref(), Some("ENG"));
    // Active states intentionally include the canonical "In Progress"
    // mixed-case spelling — the conformance suite later asserts adapters
    // lowercase before comparing.
    assert_eq!(
        loaded.config.tracker.active_states,
        vec!["Todo".to_string(), "In Progress".to_string()]
    );

    // Polling, agent, and codex sections carry the SPEC defaults so this
    // file doubles as a worked example of "what the defaults look like
    // when written out explicitly."
    assert_eq!(loaded.config.polling.interval_ms, 30_000);
    assert_eq!(loaded.config.agent.kind, AgentKind::Codex);
    assert_eq!(loaded.config.agent.max_concurrent_agents, 4);
    assert_eq!(loaded.config.agent.max_turns, 20);
    assert_eq!(
        loaded
            .config
            .agent
            .max_concurrent_agents_by_state
            .get("in_progress")
            .copied(),
        Some(2)
    );
    assert_eq!(loaded.config.codex.command, "codex app-server");

    // The prompt body is not empty and has been trimmed (no leading or
    // trailing whitespace). The `symphony validate` smoke test will rely
    // on a non-empty body to assert the CLI surfaces it.
    assert!(
        !loaded.prompt_template.is_empty(),
        "prompt body should be non-empty"
    );
    assert_eq!(
        loaded.prompt_template,
        loaded.prompt_template.trim(),
        "prompt body must come back trimmed"
    );
    assert!(
        loaded
            .prompt_template
            .starts_with("# Symphony Sample Workflow"),
        "prompt body should start with the documented heading; got: {:?}",
        &loaded.prompt_template[..loaded.prompt_template.len().min(80)]
    );

    // Final gate: the same validation `symphony validate` will run.
    loaded
        .config
        .validate()
        .expect("sample fixture must satisfy WorkflowConfig::validate");
}
