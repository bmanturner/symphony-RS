//! YAML fixture loader for [`MockTracker`].
//!
//! The Quickstart flow (Phase 7) lets an operator point `WORKFLOW.md`'s
//! `tracker.fixtures` at a YAML file describing the canned set of issues
//! the in-memory tracker should serve. This module owns that file
//! format so the CLI factory only has to hand us a path.
//!
//! ## Why YAML and not JSON
//!
//! `WORKFLOW.md` itself is YAML front matter, and the fixture file is
//! authored alongside it. Keeping the same syntax means an operator who
//! can write a workflow can write a fixture without learning a second
//! grammar. [`Issue`] / [`IssueId`] / [`IssueState`] all derive
//! `Deserialize` directly, so the fixture schema is "an `Issue` per list
//! entry" — additions to [`Issue`] are picked up automatically.
//!
//! ## Schema
//!
//! ```yaml
//! active:
//!   - id: id-1
//!     identifier: ABC-1
//!     title: Wire the mock tracker
//!     state: Todo
//! terminal:
//!   - id: id-2
//!     identifier: ABC-2
//!     title: Already done
//!     state: Done
//! ```
//!
//! Both lists default to empty. Required `Issue` fields (`id`,
//! `identifier`, `title`, `state`) must be present; everything else
//! follows the [`Issue`]-level defaults. Unknown top-level keys are
//! rejected so a typo (`actve:`) fails at load time rather than silently
//! producing an empty tracker.

use std::path::Path;

use serde::Deserialize;
use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};

use crate::MockTracker;

/// On-disk shape for a single issue.
///
/// Mirrors [`Issue`] but defaults every optional field so the YAML
/// author can omit them. We deliberately keep this distinct from
/// [`Issue`] itself: [`Issue`] is the orchestrator's load-bearing data
/// type and adding `#[serde(default)]` to its definition would let real
/// adapters silently drop fields that the wire format actually contains.
/// The fixture loader is the only place where partial issues make
/// sense, so the leniency lives here.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureIssue {
    id: IssueId,
    identifier: String,
    title: String,
    state: IssueState,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    blocked_by: Vec<BlockerRef>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

impl From<FixtureIssue> for Issue {
    fn from(f: FixtureIssue) -> Self {
        Issue {
            id: f.id,
            identifier: f.identifier,
            title: f.title,
            description: f.description,
            priority: f.priority,
            state: f.state,
            branch_name: f.branch_name,
            url: f.url,
            labels: f.labels,
            blocked_by: f.blocked_by,
            created_at: f.created_at,
            updated_at: f.updated_at,
        }
    }
}

/// On-disk fixture file shape.
///
/// Public so callers that want to inspect the parsed file before handing
/// it off (e.g. tests asserting the YAML round-tripped exactly) can
/// reuse the type. Construct one through [`load`] in normal use.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixtures {
    /// Issues returned by `fetch_active`. The fabrication policy from
    /// the trait still applies — the loader trusts the YAML author to
    /// only mark issues `active` whose state matches the workflow's
    /// `tracker.active_states` list. The conformance suite's properties
    /// remain the contract source of truth.
    #[serde(default)]
    active: Vec<FixtureIssue>,

    /// Issues returned by `fetch_terminal_recent` (subject to the
    /// caller-supplied state filter, like the real adapters).
    #[serde(default)]
    terminal: Vec<FixtureIssue>,
}

impl Fixtures {
    /// Number of canned active issues. Useful for diagnostics
    /// (`"loaded N active issues from PATH"`) without exposing the
    /// internal shape.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Number of canned terminal issues. Symmetrical to
    /// [`Self::active_count`].
    pub fn terminal_count(&self) -> usize {
        self.terminal.len()
    }
}

/// Errors raised by [`load`].
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// File could not be read off disk.
    #[error("reading fixtures from {path}: {source}")]
    Io {
        /// Path the loader attempted to read.
        path: String,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },

    /// File contents did not parse as a [`Fixtures`] document.
    #[error("parsing fixtures from {path}: {source}")]
    Parse {
        /// Path the loader was parsing when the error occurred.
        path: String,
        /// Underlying YAML error.
        #[source]
        source: serde_yaml::Error,
    },
}

/// Read `path` and build a [`MockTracker`] populated from the file.
///
/// The returned tracker is fully constructed and ready to be wrapped in
/// `Arc<dyn TrackerRead>`. The original [`Fixtures`] document is also
/// returned for tests / diagnostics — callers that don't need it can
/// `let (tracker, _) = load(path)?;`.
pub fn load(path: impl AsRef<Path>) -> Result<(MockTracker, Fixtures), LoadError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let fixtures: Fixtures = serde_yaml::from_str(&raw).map_err(|source| LoadError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    let tracker = MockTracker::new();
    tracker.set_active(
        fixtures
            .active
            .iter()
            .cloned()
            .map(Issue::from)
            .collect::<Vec<_>>(),
    );
    tracker.set_terminal(
        fixtures
            .terminal
            .iter()
            .cloned()
            .map(Issue::from)
            .collect::<Vec<_>>(),
    );
    Ok((tracker, fixtures))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use symphony_core::tracker::{IssueId, IssueState};
    use symphony_core::tracker_trait::TrackerRead;

    fn write_tmp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new().suffix(".yaml").tempfile().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn load_populates_mock_tracker_active_and_terminal() {
        let f = write_tmp(
            r#"
active:
  - id: id-1
    identifier: ABC-1
    title: Wire mock
    state: Todo
terminal:
  - id: id-2
    identifier: ABC-2
    title: Done
    state: Done
"#,
        );
        let (tracker, fixtures) = load(f.path()).unwrap();
        assert_eq!(fixtures.active_count(), 1);
        assert_eq!(fixtures.terminal_count(), 1);

        let active = tracker.fetch_active().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, IssueId::new("id-1"));

        // Filter by state to confirm the terminal list flows through.
        let terminal = tracker
            .fetch_terminal_recent(&[IssueState::new("Done")])
            .await
            .unwrap();
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].identifier, "ABC-2");
    }

    #[test]
    fn missing_file_surfaces_io_error_with_path() {
        let err = load("/nonexistent/__symphony_fixture__.yaml").unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, LoadError::Io { .. }));
        assert!(msg.contains("/nonexistent/__symphony_fixture__.yaml"));
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        // Catches typos like `actve:` so the operator gets a loud error
        // rather than a silently empty tracker.
        let f = write_tmp("actve: []\n");
        let err = load(f.path()).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
        assert!(err.to_string().contains("actve") || err.to_string().contains("unknown"));
    }

    #[test]
    fn empty_file_yields_empty_tracker() {
        let f = write_tmp("active: []\nterminal: []\n");
        let (_, fixtures) = load(f.path()).unwrap();
        assert_eq!(fixtures.active_count(), 0);
        assert_eq!(fixtures.terminal_count(), 0);
    }

    #[tokio::test]
    async fn missing_optional_fields_default_to_none_or_empty() {
        // Smallest viable fixture entry: only the four required
        // [`Issue`] fields. Everything else flows through the
        // `FixtureIssue` defaults.
        let f = write_tmp(
            r#"
active:
  - id: id-1
    identifier: ABC-1
    title: minimal
    state: Todo
"#,
        );
        let (tracker, _) = load(f.path()).unwrap();
        let active = tracker.fetch_active().await.unwrap();
        assert_eq!(active.len(), 1);
        let issue = &active[0];
        assert_eq!(issue.description, None);
        assert_eq!(issue.priority, None);
        assert_eq!(issue.branch_name, None);
        assert_eq!(issue.url, None);
        assert!(issue.labels.is_empty());
        assert!(issue.blocked_by.is_empty());
    }

    #[test]
    fn missing_required_field_fails_loudly() {
        // Catch a missing required field (`title`) so the operator
        // hears about it at load time.
        let f = write_tmp(
            r#"
active:
  - id: id-1
    identifier: ABC-1
    state: Todo
"#,
        );
        let err = load(f.path()).unwrap_err();
        assert!(matches!(err, LoadError::Parse { .. }));
    }
}
