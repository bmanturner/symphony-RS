//! Issue tracker abstraction for Symphony-RS.
//!
//! The orchestrator never speaks Linear (or any other tracker) directly.
//! Instead, it depends only on the `IssueTracker` trait defined in this
//! crate, and adapters such as `LinearTracker` translate the trait calls
//! into tracker-specific GraphQL/REST traffic.
//!
//! Why a trait instead of a concrete type? SPEC §3.1 names Linear as the
//! shipped adapter, but the system is intended to be reused for GitHub
//! Issues, Jira, Plane, etc. Designing against a trait from day one means
//! we never need to untangle Linear-isms from the orchestrator later.
//!
//! Adapters will live in submodules (`linear`, `mock`); they are added in
//! Phase 2 and may be gated behind cargo features once the dependency
//! footprint matters.

// Re-export the trait surface from `symphony-core` so adapter call sites
// don't need to know which crate defines the trait. Keeps the public API
// of this crate self-contained: a downstream user wires
// `symphony_tracker::{IssueTracker, MockTracker}` and never has to think
// about the core/tracker split.
pub use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};
pub use symphony_core::tracker_trait::{IssueTracker, TrackerError, TrackerResult};

pub mod mock;
pub use mock::{MockCall, MockTracker};

pub mod conformance;

pub mod linear;
pub use linear::{LinearConfig, LinearTracker};

pub mod github;
pub use github::{GitHubConfig, GitHubTracker};
