//! End-to-end smoke test for the Quickstart fixture.
//!
//! Spawns the real `symphony` binary against
//! `tests/fixtures/quickstart-workflow/`, which combines the in-memory
//! `MockTracker` (canned issues from `issues.yaml`) with the no-op
//! `MockAgentRunner` (scripted Started/Message/Completed). After ~one
//! poll tick we send SIGINT and assert two things:
//!
//! 1. **Dispatch happened.** The orchestrator's per-issue workspace dir
//!    (named after the sanitized issue identifier) appears under the
//!    workspace root we point it at via `SYMPHONY_WORKSPACE__ROOT`.
//!    `WorkspaceManager::ensure` is the very first dispatcher step, so a
//!    sub-directory named `QUICK-1` or `QUICK-2` is the smallest
//!    observable side-effect that proves an issue made it from
//!    "tracker says active" all the way to "dispatcher started".
//!
//! 2. **Clean SIGINT exit.** The child process drains and exits 0.
//!    `run.rs` logs `"symphony run exited cleanly"` on this path; we
//!    assert that line is present in the captured stderr.
//!
//! ## Why a real subprocess
//!
//! The dispatcher / poll-loop / signal-handler wiring lives only in
//! `main` + `run.rs`. A library-level test would have to re-do the
//! composition root, drift from production over time, and miss
//! signal-handling regressions entirely. The smoke test pays a
//! compile-the-binary cost in exchange for exercising the same code
//! path an operator hits at the shell.
//!
//! ## Unix-only
//!
//! Symphony's dispatcher spawns subprocesses (Codex/Claude in
//! production, the mock here), and SIGINT is the documented shutdown
//! mechanism. Windows uses CTRL_C_EVENT through a different API; rather
//! than maintain two graceful-shutdown paths in a single test, we gate
//! this whole file on `cfg(unix)`. Coverage on macOS + Linux is the
//! relevant signal for the v1 deployment target.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Resolve the checked-in fixture directory. Computed from
/// `CARGO_MANIFEST_DIR` so the test is robust to the shell's CWD when
/// `cargo test` is invoked from a subdirectory.
fn fixture_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("symphony-cli is two levels under the workspace root")
        .join("tests/fixtures/quickstart-workflow")
}

/// Wait for `child` to exit, polling every 50ms up to `deadline`. Returns
/// the captured `Output` on clean exit, or `None` on timeout. We can't
/// use `Child::wait_with_output` directly because we need to bound the
/// shutdown drain — a wedged child would hang the test forever.
fn wait_with_deadline(
    mut child: std::process::Child,
    deadline: Duration,
) -> Option<std::process::Output> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Send SIGINT to `pid` via `/bin/kill -INT`. Avoids a dependency on
/// `libc` / `nix` for a single FFI call. The shell's `kill` is in POSIX
/// and is the same mechanism an operator would use to ask the daemon to
/// shut down.
fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .arg("-INT")
        .arg(pid.to_string())
        .status()
        .expect("kill -INT must spawn");
    assert!(status.success(), "kill -INT returned {status}");
}

#[test]
#[ignore = "v2 grounding: quickstart fixture is now SPEC_v2 §5 shape; the v1 binary's loader rejects the new sections. Re-enabled when Phase 1 (typed schema) and Phase 11 (scheduler v2) land."]
fn quickstart_dispatches_an_issue_and_exits_cleanly_on_sigint() {
    let fixture = fixture_dir();
    let workflow = fixture.join("WORKFLOW.md");
    assert!(
        workflow.exists(),
        "missing fixture at {}",
        workflow.display()
    );

    // Per-test workspace root. The fixture deliberately omits
    // `workspace.root` so this env var (figment layer 2) flows through.
    let workspace_root = tempfile::tempdir().expect("tempdir for workspace root");
    let bin = assert_cmd::cargo::cargo_bin("symphony");

    let child = Command::new(&bin)
        .arg("run")
        .arg(&workflow)
        // Quiet log filter — info-level captures the lines we assert on
        // without the test depending on tracing-subscriber's default
        // formatting (which varies by feature flags).
        .env("RUST_LOG", "info")
        .env("SYMPHONY_WORKSPACE__ROOT", workspace_root.path())
        // Belt-and-braces: ensure no developer-shell credentials leak
        // into the env and accidentally drive a different adapter.
        .env_remove("LINEAR_API_KEY")
        .env_remove("GITHUB_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("symphony run must spawn");

    let pid = child.id();

    // Give the orchestrator long enough for at least one poll tick
    // (200ms in the fixture) plus the dispatcher's ensure() call. The
    // mock agent finishes its scripted sequence in <50ms, so 1.5s is
    // comfortably more than needed without making the test slow.
    let observed = poll_until(Duration::from_millis(2000), || {
        sub_dirs(workspace_root.path())
            .iter()
            .any(|d| d == "QUICK-1" || d == "QUICK-2")
    });

    send_sigint(pid);
    let output = wait_with_deadline(child, Duration::from_secs(10))
        .expect("child must exit within drain deadline");

    assert!(
        observed,
        "expected a per-issue workspace dir under {} after one poll tick; saw {:?}\nstderr:\n{}",
        workspace_root.path().display(),
        sub_dirs(workspace_root.path()),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected clean exit, got {:?}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("symphony run exited cleanly"),
        "expected graceful-shutdown log line; stderr was:\n{stderr}",
    );
}

/// List the immediate sub-directory names of `root`. Used as the
/// dispatch-evidence probe — `WorkspaceManager::ensure` creates exactly
/// one directory per dispatched issue.
fn sub_dirs(root: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(root)
        .map(|it| {
            it.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Sleep-poll helper. Returns true the first time `cond` returns true
/// before `deadline`, false if the deadline elapsed first. We poll
/// rather than sleep-then-check so a fast machine doesn't pay the full
/// timeout when dispatch lands quickly.
fn poll_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}
