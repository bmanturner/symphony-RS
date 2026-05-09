//! Issue tracker abstraction for Symphony-RS.
//!
//! The orchestrator never speaks Linear (or any other tracker) directly.
//! Instead, it depends on the [`TrackerRead`] trait (read-only candidate /
//! state / recovery queries) and the optional [`TrackerMutations`] trait
//! (write capabilities) defined in `symphony-core`; adapters such as
//! `LinearTracker` translate the trait calls into tracker-specific
//! GraphQL/REST traffic.
//!
//! v2 splits read and mutation surfaces (SPEC v2 §7.1, §7.2) so workflows
//! can detect read-only vs mutation-capable adapters and so adapters that
//! genuinely cannot mutate participate without faking writes.

// Re-export the trait surface from `symphony-core` so adapter call sites
// don't need to know which crate defines the traits. Keeps the public
// API of this crate self-contained: a downstream user wires
// `symphony_tracker::{TrackerRead, MockTracker}` and never has to think
// about the core/tracker split.
pub use symphony_core::tracker::{BlockerRef, Issue, IssueId, IssueState};
pub use symphony_core::tracker_trait::{
    TrackerError, TrackerMutations, TrackerRead, TrackerResult,
};

pub mod mock;
pub use mock::{MockCall, MockTracker};

pub mod fixtures;

pub mod conformance;

pub mod linear;
pub use linear::{LinearConfig, LinearTracker};

pub mod github;
pub use github::{GitHubConfig, GitHubTracker};
