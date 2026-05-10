//! Implementation of the `symphony validate` subcommand.
//!
//! Loads a `WORKFLOW.md` through [`symphony_config::LayeredLoader`] —
//! the same loader the orchestrator uses on startup — and runs the
//! semantic [`symphony_config::WorkflowConfig::validate`] pass on top. The two stages
//! are kept separate in error reporting so an operator can tell
//! "your YAML is malformed" apart from "your YAML parses but a value
//! is out of range" without grepping the message body.
//!
//! Exit codes are deliberately small and stable so CI can branch on
//! them:
//!
//! - `0` — file loaded and config is valid.
//! - `1` — the loader rejected the file (missing, unreadable, malformed
//!   YAML, env override that does not satisfy the typed schema).
//! - `2` — the loader succeeded but a semantic constraint failed (e.g.
//!   `agent.max_turns = 0`, missing `tracker.repository` for GitHub).
//!
//! The summary printed on success is intentionally compact — one line
//! per top-level section — so a test or `grep` can pattern-match it
//! without parsing structured output.

use std::path::Path;

use symphony_config::{ConfigValidationError, LayeredLoadError, LayeredLoader, LoadedWorkflow};

/// Outcome of a `symphony validate` run, kept distinct from the binary's
/// exit code so the function is easy to unit-test without spawning a
/// process.
///
/// `LoadFailed` and `Invalid` both indicate the user needs to fix
/// something; the split exists purely so the CLI can map them to
/// different exit codes (see module docs).
#[derive(Debug)]
pub enum ValidateOutcome {
    /// The workflow loaded and `validate()` passed.
    Ok(Box<LoadedWorkflow>),
    /// The loader rejected the file or the layered merge.
    LoadFailed(LayeredLoadError),
    /// The loader succeeded but a semantic constraint failed.
    Invalid(Box<LoadedWorkflow>, ConfigValidationError),
}

impl ValidateOutcome {
    /// Map an outcome to its stable exit code (see module docs).
    pub fn exit_code(&self) -> i32 {
        match self {
            ValidateOutcome::Ok(_) => 0,
            ValidateOutcome::LoadFailed(_) => 1,
            ValidateOutcome::Invalid(_, _) => 2,
        }
    }
}

/// Run the `validate` subcommand against `path` and return the outcome.
///
/// Splitting "compute outcome" from "render to stdout/stderr" lets the
/// integration tests assert on either side independently.
pub fn run(path: &Path) -> ValidateOutcome {
    match LayeredLoader::from_path(path) {
        Err(err) => ValidateOutcome::LoadFailed(err),
        Ok(loaded) => match loaded.config.validate() {
            Ok(()) => ValidateOutcome::Ok(Box::new(loaded)),
            Err(err) => ValidateOutcome::Invalid(Box::new(loaded), err),
        },
    }
}

/// Render `outcome` to stdout (success) or stderr (failure). Returns
/// the exit code the caller should propagate.
///
/// Output format is intentionally human-first: a one-line OK header
/// followed by short, key:value lines for the most operationally
/// relevant fields. Machine-readable output is a future subcommand
/// (`symphony validate --format=json`) and not in scope for this
/// iteration.
pub fn render(outcome: &ValidateOutcome) -> i32 {
    match outcome {
        ValidateOutcome::Ok(loaded) => {
            let cfg = &loaded.config;
            println!("OK: {} parses and validates", loaded.source_path.display());
            println!("  tracker.kind         = {:?}", cfg.tracker.kind);
            // active_states / terminal_states join with ", " — a single
            // line keeps the summary scannable for the common case where
            // the lists are short.
            println!(
                "  tracker.active       = [{}]",
                cfg.tracker.active_states.join(", ")
            );
            println!(
                "  tracker.terminal     = [{}]",
                cfg.tracker.terminal_states.join(", ")
            );
            println!("  polling.interval_ms  = {}", cfg.polling.interval_ms);
            println!("  agent.kind           = {:?}", cfg.agent.kind);
            println!("  agent.max_turns      = {}", cfg.agent.max_turns);
            println!(
                "  agent.max_concurrent = {}",
                cfg.agent.max_concurrent_agents
            );
            println!(
                "  prompt.body_chars    = {}",
                loaded.prompt_template.chars().count()
            );
            for warning in cfg.validation_warnings() {
                eprintln!("{warning}");
            }
        }
        ValidateOutcome::LoadFailed(err) => {
            // `eprintln!` so a CI step capturing stdout for the success
            // summary does not accidentally swallow the failure reason.
            eprintln!("error: failed to load workflow: {err}");
        }
        ValidateOutcome::Invalid(loaded, err) => {
            eprintln!(
                "error: workflow at {} is semantically invalid: {err}",
                loaded.source_path.display()
            );
        }
    }
    outcome.exit_code()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write `body` to a temp `WORKFLOW.md` and return the
    /// `(tempdir, path)` pair. Holding the tempdir keeps the file alive
    /// for the duration of the test.
    fn workflow_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, path)
    }

    #[test]
    fn ok_outcome_for_valid_workflow() {
        let (_dir, path) =
            workflow_file("---\ntracker:\n  kind: linear\n  project_slug: ENG\n---\nbody\n");
        let outcome = run(&path);
        assert!(matches!(outcome, ValidateOutcome::Ok(_)));
        assert_eq!(outcome.exit_code(), 0);
    }

    #[test]
    fn load_failed_when_file_missing() {
        let outcome = run(Path::new("/no/such/path/WORKFLOW.md"));
        assert!(matches!(outcome, ValidateOutcome::LoadFailed(_)));
        assert_eq!(outcome.exit_code(), 1);
    }

    #[test]
    fn load_failed_for_malformed_yaml() {
        // Unknown nested key — figment surfaces this through the typed
        // extract step, which we report as `LoadFailed` (exit 1).
        let (_dir, path) = workflow_file("---\npolling:\n  interva_ms: 100\n---\nbody\n");
        let outcome = run(&path);
        assert!(matches!(outcome, ValidateOutcome::LoadFailed(_)));
        assert_eq!(outcome.exit_code(), 1);
    }

    #[test]
    fn invalid_when_linear_missing_project_slug() {
        // tracker.kind = linear but no project_slug → parses fine but
        // `validate()` rejects it → exit 2.
        let (_dir, path) = workflow_file("---\ntracker:\n  kind: linear\n---\nbody\n");
        let outcome = run(&path);
        match &outcome {
            ValidateOutcome::Invalid(_, ConfigValidationError::LinearMissingProjectSlug) => {}
            other => panic!("expected Invalid(LinearMissingProjectSlug), got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }
}
