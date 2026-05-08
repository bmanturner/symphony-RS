//! Shared helpers for subprocess fake-binary tests.
//!
//! The fake-binary strategy: write a small bash script to a tempfile,
//! mark it executable, point the runner's `command` at it. This keeps the
//! fixture self-contained (no committed shell scripts to lose +x on
//! checkout, no fragile cargo bin builds to wait on) and yet exercises
//! the full spawn / pipe / stdin-stdout / kill code path of each runner.
//!
//! Lives in `tests/common/` rather than `tests/fixtures/` because Cargo
//! treats every `.rs` directly under `tests/` as its own integration
//! binary; nesting under `common/mod.rs` keeps the helpers as a module
//! shared by the real test binaries.

#![allow(dead_code)] // each test binary uses a subset

use futures::Stream;
use futures::StreamExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use symphony_core::agent::AgentEvent;
use tokio::time::timeout;

/// Write `body` as an executable script at `dir/name` and return its
/// absolute path. Sets mode 0o755 so both Codex (`bash -lc <path>`) and
/// Claude (`exec <path>`) can launch it.
pub fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write fake script");
    let mut perms = std::fs::metadata(&path)
        .expect("stat fake script")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod fake script");
    path
}

/// Pull the next event from the runner's stream with a 5-second cap.
///
/// Tests that block forever on a fake binary that misbehaves are a
/// nightmare to triage — failing fast with a clear timeout message is
/// always better than a hung CI job.
pub async fn next_event<S>(stream: &mut S) -> AgentEvent
where
    S: Stream<Item = AgentEvent> + Unpin,
{
    timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timeout waiting for AgentEvent")
        .expect("AgentEvent stream closed unexpectedly")
}

/// Drain the stream until it closes or the timeout fires. Used by abort
/// tests that want to assert the stream eventually terminates without
/// caring about the specific tail events.
pub async fn drain_until_closed<S>(stream: &mut S, within: Duration)
where
    S: Stream<Item = AgentEvent> + Unpin,
{
    timeout(within, async { while stream.next().await.is_some() {} })
        .await
        .expect("stream did not close inside the deadline");
}
