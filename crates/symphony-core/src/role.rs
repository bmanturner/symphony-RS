//! Domain model for roles (SPEC v2 §4.4).
//!
//! A role is a configurable specialist capability profile. The workflow
//! YAML decides the *names* of roles and which agent profile drives them;
//! the kernel reasons about a role's [`RoleKind`] and its derived
//! [`RoleAuthority`] flags rather than the operator-chosen label.
//!
//! This module mirrors `symphony_config::RoleKind` and
//! `symphony_config::RoleConfig` without depending on the config crate
//! — the rule is the same as for [`crate::work_item::WorkItemId`] vs
//! `symphony-state`'s row id: keep the domain shape free of the
//! upstream's parsing/serde quirks so an accidental swap shows up as a
//! type error.
//!
//! Resolution from `RoleConfig` (where authority flags are
//! `Option<bool>`, preserving operator intent) into [`RoleContext`]
//! (where flags are concrete `bool`s the kernel branches on) lives at
//! the config-load seam — see Phase 5 routing for the call site. This
//! module supplies [`RoleAuthority::defaults_for`] so that resolver has
//! one canonical source of truth for kind-derived defaults.

use serde::{Deserialize, Serialize};

/// Operator-chosen role label (the map key in `WorkflowConfig::roles`).
///
/// Newtyped so the kernel can't accidentally swap it with an agent
/// profile name — both are `String`s in YAML but live in different
/// keyspaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleName(String);

impl RoleName {
    /// Construct a [`RoleName`] from a workflow label.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Borrow the underlying label.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the inner `String`.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for RoleName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for RoleName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for RoleName {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// Semantic role kind (SPEC v2 §4.4).
///
/// Only [`Self::IntegrationOwner`] and [`Self::QaGate`] are semantically
/// special in the kernel. The remaining variants exist so workflows can
/// declare role intent without forcing every non-special role through
/// `Specialist`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// May decompose, assign, merge child outputs, request QA, and close
    /// parent work. Exactly one role of this kind is required per
    /// workflow.
    IntegrationOwner,
    /// May verify, reject, file blockers, and create follow-up issues.
    /// Required when QA is enabled.
    QaGate,
    /// Implements scoped work and reports evidence.
    Specialist,
    /// Reviews design/security/performance/content without owning
    /// implementation.
    Reviewer,
    /// Performs runtime/deployment/data tasks.
    Operator,
    /// Workflow-defined behaviour outside the well-known kinds.
    Custom,
}

impl RoleKind {
    /// All variants in declaration order. Useful for exhaustive
    /// iteration in tests and validators.
    pub const ALL: [Self; 6] = [
        Self::IntegrationOwner,
        Self::QaGate,
        Self::Specialist,
        Self::Reviewer,
        Self::Operator,
        Self::Custom,
    ];

    /// Stable lowercase identifier used in YAML config and JSON
    /// payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IntegrationOwner => "integration_owner",
            Self::QaGate => "qa_gate",
            Self::Specialist => "specialist",
            Self::Reviewer => "reviewer",
            Self::Operator => "operator",
            Self::Custom => "custom",
        }
    }

    /// True for role kinds the kernel branches on directly. Equivalent
    /// to `matches!(self, IntegrationOwner | QaGate)`.
    pub fn is_kernel_special(self) -> bool {
        matches!(self, Self::IntegrationOwner | Self::QaGate)
    }
}

impl std::fmt::Display for RoleKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Effective per-role authority flags (SPEC v2 §4.4 / §5.4).
///
/// Each flag answers a single yes/no kernel question. Construct via
/// [`Self::defaults_for`] for kind-driven defaults, then layer explicit
/// operator overrides on top via [`Self::with_overrides`]. The kernel
/// only ever reads the resolved `bool` form — `Option<bool>`'s
/// "unset/explicit-false" distinction belongs in the config layer, not
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleAuthority {
    /// May decompose broad parent work into child issues.
    pub can_decompose: bool,
    /// May assign children to roles.
    pub can_assign: bool,
    /// May request QA on a draft PR or integration handoff.
    pub can_request_qa: bool,
    /// May close the parent issue once children, integration, and QA
    /// gates are satisfied.
    pub can_close_parent: bool,
    /// May file blocker work items against the item under review.
    pub can_file_blockers: bool,
    /// May file follow-up work items (subject to follow-up policy).
    pub can_file_followups: bool,
    /// This role's pass is required before the parent can be closed.
    pub required_for_done: bool,
}

impl RoleAuthority {
    /// Authority flags with every capability disabled. Useful as a base
    /// for `Specialist`, `Reviewer`, `Operator`, and `Custom` roles
    /// which start with no kernel authority and earn it explicitly via
    /// operator overrides.
    pub const NONE: Self = Self {
        can_decompose: false,
        can_assign: false,
        can_request_qa: false,
        can_close_parent: false,
        can_file_blockers: false,
        can_file_followups: false,
        required_for_done: false,
    };

    /// Kind-derived default authority (SPEC v2 §4.4).
    ///
    /// * [`RoleKind::IntegrationOwner`] earns the parent-ownership
    ///   capabilities (decompose/assign/request QA/close parent).
    /// * [`RoleKind::QaGate`] earns the gating capabilities (file
    ///   blockers, file follow-ups, required for done).
    /// * Every other kind starts at [`Self::NONE`].
    ///
    /// The set of kind→authority defaults is deliberately conservative.
    /// Operators wanting wider authority for `specialist`/`reviewer`/
    /// `operator`/`custom` roles must opt in explicitly in the YAML.
    pub const fn defaults_for(kind: RoleKind) -> Self {
        match kind {
            RoleKind::IntegrationOwner => Self {
                can_decompose: true,
                can_assign: true,
                can_request_qa: true,
                can_close_parent: true,
                ..Self::NONE
            },
            RoleKind::QaGate => Self {
                can_file_blockers: true,
                can_file_followups: true,
                required_for_done: true,
                ..Self::NONE
            },
            RoleKind::Specialist | RoleKind::Reviewer | RoleKind::Operator | RoleKind::Custom => {
                Self::NONE
            }
        }
    }

    /// Layer explicit operator overrides on top of these defaults.
    /// `Some(b)` replaces the field; `None` leaves the existing value.
    pub fn with_overrides(mut self, overrides: RoleAuthorityOverrides) -> Self {
        if let Some(b) = overrides.can_decompose {
            self.can_decompose = b;
        }
        if let Some(b) = overrides.can_assign {
            self.can_assign = b;
        }
        if let Some(b) = overrides.can_request_qa {
            self.can_request_qa = b;
        }
        if let Some(b) = overrides.can_close_parent {
            self.can_close_parent = b;
        }
        if let Some(b) = overrides.can_file_blockers {
            self.can_file_blockers = b;
        }
        if let Some(b) = overrides.can_file_followups {
            self.can_file_followups = b;
        }
        if let Some(b) = overrides.required_for_done {
            self.required_for_done = b;
        }
        self
    }
}

impl Default for RoleAuthority {
    fn default() -> Self {
        Self::NONE
    }
}

/// Optional operator-supplied overrides for [`RoleAuthority`] flags.
///
/// Mirrors the `Option<bool>` shape of `symphony_config::RoleConfig`'s
/// authority fields so a config-layer resolver can pass them through
/// without re-encoding "unset".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoleAuthorityOverrides {
    /// Override for [`RoleAuthority::can_decompose`].
    #[serde(default)]
    pub can_decompose: Option<bool>,
    /// Override for [`RoleAuthority::can_assign`].
    #[serde(default)]
    pub can_assign: Option<bool>,
    /// Override for [`RoleAuthority::can_request_qa`].
    #[serde(default)]
    pub can_request_qa: Option<bool>,
    /// Override for [`RoleAuthority::can_close_parent`].
    #[serde(default)]
    pub can_close_parent: Option<bool>,
    /// Override for [`RoleAuthority::can_file_blockers`].
    #[serde(default)]
    pub can_file_blockers: Option<bool>,
    /// Override for [`RoleAuthority::can_file_followups`].
    #[serde(default)]
    pub can_file_followups: Option<bool>,
    /// Override for [`RoleAuthority::required_for_done`].
    #[serde(default)]
    pub required_for_done: Option<bool>,
}

/// Resolved role record passed to the kernel at dispatch time.
///
/// `name` is the operator-chosen label (the map key in
/// `WorkflowConfig::roles`); `kind` is the semantic kind the kernel
/// branches on; `authority` is the kind-derived defaults already merged
/// with explicit operator overrides. `agent_profile` references the
/// profile name in `WorkflowConfig::agents` that runs work for this
/// role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleContext {
    /// Operator-chosen role label.
    pub name: RoleName,
    /// Semantic kind the kernel branches on.
    pub kind: RoleKind,
    /// Effective authority flags, post-resolution.
    pub authority: RoleAuthority,
    /// Agent profile name (key into `WorkflowConfig::agents`). `None`
    /// when a role is declared without an agent yet — config-layer
    /// validators decide whether that's acceptable for the workflow.
    pub agent_profile: Option<String>,
    /// Hard cap on simultaneously running agents for this role. `None`
    /// inherits the global cap.
    pub max_concurrent: Option<u32>,
    /// Free-form operator-facing description.
    pub description: Option<String>,
}

impl RoleContext {
    /// Build a [`RoleContext`] with kind-derived authority defaults and
    /// no overrides. Intended for tests and the simple resolver path.
    pub fn from_kind(name: impl Into<RoleName>, kind: RoleKind) -> Self {
        Self {
            name: name.into(),
            kind,
            authority: RoleAuthority::defaults_for(kind),
            agent_profile: None,
            max_concurrent: None,
            description: None,
        }
    }

    /// True when this role has [`RoleKind::IntegrationOwner`].
    pub fn is_integration_owner(&self) -> bool {
        self.kind == RoleKind::IntegrationOwner
    }

    /// True when this role has [`RoleKind::QaGate`].
    pub fn is_qa_gate(&self) -> bool {
        self.kind == RoleKind::QaGate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_kind_serializes_as_snake_case_for_every_variant() {
        for kind in RoleKind::ALL {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let back: RoleKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn role_kind_is_kernel_special_only_for_integration_owner_and_qa_gate() {
        assert!(RoleKind::IntegrationOwner.is_kernel_special());
        assert!(RoleKind::QaGate.is_kernel_special());
        assert!(!RoleKind::Specialist.is_kernel_special());
        assert!(!RoleKind::Reviewer.is_kernel_special());
        assert!(!RoleKind::Operator.is_kernel_special());
        assert!(!RoleKind::Custom.is_kernel_special());
    }

    #[test]
    fn integration_owner_defaults_grant_parent_ownership_authority() {
        let a = RoleAuthority::defaults_for(RoleKind::IntegrationOwner);
        assert!(a.can_decompose);
        assert!(a.can_assign);
        assert!(a.can_request_qa);
        assert!(a.can_close_parent);
        // Gating authority belongs to QA, not the integration owner.
        assert!(!a.can_file_blockers);
        assert!(!a.required_for_done);
    }

    #[test]
    fn qa_gate_defaults_grant_gating_and_filing_authority() {
        let a = RoleAuthority::defaults_for(RoleKind::QaGate);
        assert!(a.can_file_blockers);
        assert!(a.can_file_followups);
        assert!(a.required_for_done);
        // Parent-ownership authority belongs to the integration owner.
        assert!(!a.can_decompose);
        assert!(!a.can_close_parent);
    }

    #[test]
    fn other_role_kinds_default_to_no_authority() {
        for kind in [
            RoleKind::Specialist,
            RoleKind::Reviewer,
            RoleKind::Operator,
            RoleKind::Custom,
        ] {
            assert_eq!(
                RoleAuthority::defaults_for(kind),
                RoleAuthority::NONE,
                "{kind} should start at NONE",
            );
        }
    }

    #[test]
    fn overrides_replace_only_specified_fields() {
        let base = RoleAuthority::defaults_for(RoleKind::IntegrationOwner);
        let merged = base.with_overrides(RoleAuthorityOverrides {
            can_close_parent: Some(false),
            can_file_followups: Some(true),
            ..Default::default()
        });
        // Replaced fields take the override value...
        assert!(!merged.can_close_parent);
        assert!(merged.can_file_followups);
        // ...untouched fields retain the kind-derived default.
        assert!(merged.can_decompose);
        assert!(merged.can_request_qa);
        assert!(!merged.can_file_blockers);
    }

    #[test]
    fn overrides_with_no_fields_set_is_identity() {
        let base = RoleAuthority::defaults_for(RoleKind::QaGate);
        let merged = base.with_overrides(RoleAuthorityOverrides::default());
        assert_eq!(base, merged);
    }

    #[test]
    fn role_authority_round_trips_through_json() {
        let a = RoleAuthority::defaults_for(RoleKind::IntegrationOwner);
        let json = serde_json::to_string(&a).unwrap();
        let back: RoleAuthority = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn role_context_from_kind_inherits_kind_defaults() {
        let ctx = RoleContext::from_kind("platform_lead", RoleKind::IntegrationOwner);
        assert_eq!(ctx.name.as_str(), "platform_lead");
        assert!(ctx.is_integration_owner());
        assert!(!ctx.is_qa_gate());
        assert_eq!(
            ctx.authority,
            RoleAuthority::defaults_for(RoleKind::IntegrationOwner),
        );
        assert!(ctx.agent_profile.is_none());
        assert!(ctx.description.is_none());
    }

    #[test]
    fn role_context_round_trips_through_json() {
        let ctx = RoleContext {
            name: RoleName::new("qa"),
            kind: RoleKind::QaGate,
            authority: RoleAuthority::defaults_for(RoleKind::QaGate),
            agent_profile: Some("qa_agent".into()),
            max_concurrent: Some(2),
            description: Some("Verifies acceptance criteria".into()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: RoleContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, back);
    }

    #[test]
    fn role_name_compares_byte_exact() {
        // Role names are operator-chosen labels — the kernel matches
        // them verbatim, no case folding (unlike tracker statuses).
        assert_ne!(RoleName::new("QA"), RoleName::new("qa"));
        assert_eq!(RoleName::new("qa"), RoleName::new("qa"));
    }
}
