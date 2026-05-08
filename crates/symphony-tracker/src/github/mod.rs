//! GitHub Issues adapter.
//!
//! Companion to [`crate::linear`]: same trait surface (`IssueTracker`),
//! different wire protocol. This first slice ships [`GitHubConfig`],
//! [`GitHubTracker`], and a working [`IssueTracker::fetch_active`]
//! built on `octocrab`'s REST client. The remaining trait methods
//! (`fetch_state`, `fetch_terminal_recent`) and the richer mappings
//! (`blocked_by`, `branch_name`) land in the follow-up checklist
//! items (b), (c), and (d).
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
//! [`IssueTracker::fetch_active`]: crate::IssueTracker::fetch_active

pub mod adapter;
pub use adapter::{GitHubConfig, GitHubTracker};
