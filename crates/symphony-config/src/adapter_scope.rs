//! Product adapter scope manifest (SPEC v2 §7, §3 Non-Goals).
//!
//! SPEC v2 §7 explicitly limits the supported adapter set to:
//! `git`, `github`, `linear`, `codex`, `claude`, and `hermes`.
//! SPEC v2 §3 Non-Goals further forbids "adapters beyond git, GitHub,
//! Linear, Codex, Claude, and Hermes".
//!
//! The product engages with two adapter axes that are surfaced through
//! configuration enums and therefore reachable from a workflow file:
//!
//! 1. [`TrackerKind`] — issue tracker adapters.
//! 2. [`AgentBackend`] — agent runtime adapters.
//!
//! The third axis, source control, is covered by exactly one product
//! adapter (`git`) and is not currently selectable via an enum, so
//! locking it down is a code-organisation concern handled in
//! `symphony-workspace`. A future variant would have to add a new
//! discriminator there; this manifest documents that intent.
//!
//! This module exists so that any future contributor adding a new
//! variant to either enum is forced to update the supported-adapter
//! lists *and* the exhaustive `match` in the tests below. Reviewers can
//! then reject the change unless SPEC v2 §7 is amended first.

use crate::config::{AgentBackend, TrackerKind};

/// Tracker adapters that are part of the supported product surface.
///
/// `Mock` is intentionally absent — it is a test/fixture-only fake per
/// SPEC v2 §7: "Tests may use fakes, but fakes are not product
/// adapters."
pub const SUPPORTED_PRODUCT_TRACKERS: &[TrackerKind] = &[TrackerKind::Github, TrackerKind::Linear];

/// Agent runtime adapters that are part of the supported product
/// surface.
///
/// `Mock` is intentionally absent — same rationale as
/// [`SUPPORTED_PRODUCT_TRACKERS`].
pub const SUPPORTED_PRODUCT_AGENT_BACKENDS: &[AgentBackend] = &[
    AgentBackend::Codex,
    AgentBackend::Claude,
    AgentBackend::Hermes,
];

/// The single supported source-control adapter name. There is no enum
/// to lock this against today; if a second SCM is ever added, the
/// addition must update SPEC v2 §7.3 and this constant in the same
/// change.
pub const SUPPORTED_SOURCE_CONTROL_ADAPTER: &str = "git";

/// Returns whether a tracker kind is permitted in production
/// configuration. Test-only fakes (e.g. [`TrackerKind::Mock`]) return
/// `false`.
pub const fn is_product_tracker(kind: TrackerKind) -> bool {
    matches!(kind, TrackerKind::Github | TrackerKind::Linear)
}

/// Returns whether an agent backend is permitted in production
/// configuration. Test-only fakes (e.g. [`AgentBackend::Mock`]) return
/// `false`.
pub const fn is_product_agent_backend(backend: AgentBackend) -> bool {
    matches!(
        backend,
        AgentBackend::Codex | AgentBackend::Claude | AgentBackend::Hermes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive match over [`TrackerKind`]. If a new variant is
    /// added, this match no longer compiles and the author is forced
    /// to decide explicitly whether the new variant is a product
    /// adapter (and therefore must appear in SPEC v2 §7) or another
    /// test-only fake.
    #[test]
    fn tracker_kind_partition_is_exhaustive() {
        for kind in [TrackerKind::Linear, TrackerKind::Github, TrackerKind::Mock] {
            let classified = match kind {
                TrackerKind::Linear | TrackerKind::Github => true,
                TrackerKind::Mock => false,
            };
            assert_eq!(
                classified,
                is_product_tracker(kind),
                "tracker {kind:?} must be partitioned consistently with SPEC v2 §7"
            );
        }
    }

    /// Exhaustive match over [`AgentBackend`]. Same rationale as
    /// [`tracker_kind_partition_is_exhaustive`].
    #[test]
    fn agent_backend_partition_is_exhaustive() {
        for backend in [
            AgentBackend::Codex,
            AgentBackend::Claude,
            AgentBackend::Hermes,
            AgentBackend::Mock,
        ] {
            let classified = match backend {
                AgentBackend::Codex | AgentBackend::Claude | AgentBackend::Hermes => true,
                AgentBackend::Mock => false,
            };
            assert_eq!(
                classified,
                is_product_agent_backend(backend),
                "agent backend {backend:?} must be partitioned consistently with SPEC v2 §7"
            );
        }
    }

    #[test]
    fn supported_tracker_constants_match_classifier() {
        for kind in SUPPORTED_PRODUCT_TRACKERS {
            assert!(
                is_product_tracker(*kind),
                "{kind:?} listed in SUPPORTED_PRODUCT_TRACKERS but classifier rejects it"
            );
        }
        // Mock must not leak into the supported list.
        assert!(!SUPPORTED_PRODUCT_TRACKERS.contains(&TrackerKind::Mock));
    }

    #[test]
    fn supported_agent_constants_match_classifier() {
        for backend in SUPPORTED_PRODUCT_AGENT_BACKENDS {
            assert!(
                is_product_agent_backend(*backend),
                "{backend:?} listed in SUPPORTED_PRODUCT_AGENT_BACKENDS but classifier rejects it"
            );
        }
        assert!(!SUPPORTED_PRODUCT_AGENT_BACKENDS.contains(&AgentBackend::Mock));
    }

    #[test]
    fn source_control_adapter_is_locked_to_git() {
        assert_eq!(SUPPORTED_SOURCE_CONTROL_ADAPTER, "git");
    }
}
