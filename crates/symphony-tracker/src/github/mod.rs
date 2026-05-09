//! GitHub Issues adapter.
//!
//! Companion to [`crate::linear`]: same trait surface (`TrackerRead`),
//! different wire protocol. The full trait — `fetch_active`,
//! `fetch_state`, `fetch_terminal_recent` — is implemented on
//! `octocrab`, with `blocked_by` recovered from issue body text and
//! `branch_name` recovered from the most-recently-opened linked PR via
//! the issue timeline.
//!
//! ## Why a configurable `base_uri`
//!
//! `octocrab` defaults to `https://api.github.com`. Production callers
//! should leave that alone, but the integration tests in
//! `tests/github.rs` stand up a `wiremock` server on `http://127.0.0.1:<port>`
//! and need the adapter to send its requests there. Routing the test
//! base_uri through [`GitHubConfig`] keeps the wiremock setup as
//! straightforward as it is for [`crate::LinearTracker`].
//!
//! ## State derivation
//!
//! GitHub's native issue state is a two-valued enum (`open` / `closed`).
//! Symphony's orchestrator wants richer states like `"todo"`,
//! `"in progress"`, `"done"`. We bridge the two via a configurable
//! [`GitHubConfig::status_label_prefix`]: a label such as
//! `"status:in progress"` projects to a derived state of
//! `"in progress"`. Issues with no matching label fall back to the
//! native GitHub state (`"open"` or `"closed"`). See
//! `adapter::derive_state` for the exact rule and unit tests (private).
//!
//! [`TrackerRead::fetch_active`]: crate::TrackerRead::fetch_active

pub mod adapter;
pub use adapter::{GitHubConfig, GitHubTracker};
