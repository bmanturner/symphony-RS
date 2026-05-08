//! Integration smoke tests for `symphony validate`.
//!
//! Spawns the real binary through `assert_cmd` so we exercise the same
//! clap parser + dispatch path a user hits at the shell. Checks the
//! three documented exit codes (0 / 1 / 2) and the shape of the success
//! summary.

use std::io::Write;
use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// Resolve the workspace's checked-in fixture WORKFLOW.md. Lives at
/// `<workspace>/tests/fixtures/sample-workflow/WORKFLOW.md`. Computing
/// this from `CARGO_MANIFEST_DIR` keeps the test robust to the shell's
/// CWD when `cargo test` is invoked from a subdirectory.
fn fixture_workflow() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("symphony-cli is two levels under the workspace root")
        .join("tests/fixtures/sample-workflow/WORKFLOW.md")
}

#[test]
fn validate_passes_against_fixture_workflow() {
    let path = fixture_workflow();
    assert!(path.exists(), "fixture missing at {}", path.display());
    Command::cargo_bin("symphony")
        .unwrap()
        .arg("validate")
        .arg(&path)
        .assert()
        .success()
        .stdout(predicate::str::contains("OK:"))
        .stdout(predicate::str::contains("tracker.kind"))
        .stdout(predicate::str::contains("agent.kind"));
}

#[test]
fn validate_exits_1_when_file_missing() {
    Command::cargo_bin("symphony")
        .unwrap()
        .arg("validate")
        .arg("/definitely/does/not/exist/WORKFLOW.md")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("failed to load workflow"));
}

#[test]
fn validate_exits_2_for_semantically_invalid_config() {
    // tracker.kind = linear with no project_slug parses but fails
    // semantic validation — documented as exit code 2.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("WORKFLOW.md");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"---\ntracker:\n  kind: linear\n---\nbody\n")
            .unwrap();
    }
    Command::cargo_bin("symphony")
        .unwrap()
        .arg("validate")
        .arg(&path)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("semantically invalid"));
}

#[test]
fn validate_help_lists_subcommand() {
    Command::cargo_bin("symphony")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("validate"));
}
