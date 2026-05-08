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
