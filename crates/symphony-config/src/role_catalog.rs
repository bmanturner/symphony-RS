//! Platform-lead role catalog projection.
//!
//! The catalog is generated from `WORKFLOW.md` configuration and loaded
//! instruction-pack metadata. It is prompt-facing guidance: the
//! dispatcher still uses the global routing table as the source of truth
//! for actual work-item routing.

use crate::config::{
    AgentBackend, AgentProfileConfig, RoleAssignmentMetadata, RoleConfig, RoleKind,
    RoleRoutingHints, RoutingMatch, WorkflowConfig,
};
use crate::loader::{InstructionPackBundle, LoadedRoleInstructionPack};

/// Generated platform-lead assignment catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCatalog {
    /// Deterministically ordered eligible entries.
    pub entries: Vec<RoleCatalogEntry>,

    /// Summary of how much of the catalog came from structured metadata
    /// versus fallback descriptions/routing rules.
    pub source_summary: RoleCatalogSourceSummary,
}

impl RoleCatalog {
    /// Render a deterministic prompt section for platform-lead runs.
    pub fn render_for_prompt(&self) -> String {
        let mut out = String::from("Eligible child roles:\n");
        for entry in &self.entries {
            out.push_str(&format!(
                "\n## {}\n- kind: {}\n",
                entry.name,
                role_kind_label(entry.kind)
            ));
            if let Some(description) = &entry.description {
                out.push_str(&format!("- description: {description}\n"));
            }
            if let Some(agent_profile) = &entry.agent_profile {
                out.push_str(&format!("- agent_profile: {agent_profile}\n"));
            }
            if let Some(agent_backend) = entry.agent_backend {
                out.push_str(&format!(
                    "- agent_backend: {}\n",
                    agent_backend_label(agent_backend)
                ));
            }
            if let Some(max_concurrent) = entry.max_concurrent {
                out.push_str(&format!("- max_concurrent: {max_concurrent}\n"));
            }
            render_list(&mut out, "tools", &entry.tools);
            render_list(&mut out, "owns", &entry.owns);
            render_list(&mut out, "does_not_own", &entry.does_not_own);
            render_list(&mut out, "requires", &entry.requires);
            render_list(
                &mut out,
                "handoff_expectations",
                &entry.handoff_expectations,
            );
            render_list(
                &mut out,
                "routing_paths_any",
                &entry.routing_hints.paths_any,
            );
            render_list(
                &mut out,
                "routing_labels_any",
                &entry.routing_hints.labels_any,
            );
            render_list(
                &mut out,
                "routing_issue_types_any",
                &entry.routing_hints.issue_types_any,
            );
            render_list(
                &mut out,
                "routing_domains_any",
                &entry.routing_hints.domains_any,
            );
            for rule in &entry.matching_global_rules {
                out.push_str(&format!("- global_route: {}\n", rule.render()));
            }
            if let Some(summary) = &entry.instruction_pack_summary.role_prompt_summary {
                out.push_str(&format!("- instruction_summary: {summary}\n"));
            }
            if entry.instruction_pack_summary.has_soul {
                out.push_str("- has_soul: true\n");
            }
        }

        out
    }
}

/// One prompt-facing catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleCatalogEntry {
    /// Workflow role name.
    pub name: String,
    /// Semantic role kind.
    pub kind: RoleKind,
    /// Human-readable role description.
    pub description: Option<String>,
    /// Referenced `agents:` profile.
    pub agent_profile: Option<String>,
    /// Concrete backend if the role points directly at a backend profile.
    pub agent_backend: Option<AgentBackend>,
    /// Role-local concurrency cap.
    pub max_concurrent: Option<u32>,
    /// Allowed tools/toolsets from the concrete agent profile, if known.
    pub tools: Vec<String>,
    /// Work this role owns.
    pub owns: Vec<String>,
    /// Work this role must not own.
    pub does_not_own: Vec<String>,
    /// Inputs/prerequisites this role needs.
    pub requires: Vec<String>,
    /// Handoff evidence expected from this role.
    pub handoff_expectations: Vec<String>,
    /// Role-local advisory routing hints.
    pub routing_hints: RoleRoutingHints,
    /// Global routing rules that target this role.
    pub matching_global_rules: Vec<RoutingRuleSummary>,
    /// Concise loaded instruction-pack summary.
    pub instruction_pack_summary: InstructionPackSummary,
    /// True when assignment content came only from fallback sources.
    pub used_fallback_assignment: bool,
}

/// Catalog source coverage summary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleCatalogSourceSummary {
    /// Number of entries in the catalog.
    pub total_entries: usize,
    /// Entries with structured assignment metadata.
    pub structured_entries: usize,
    /// Entries that rely on description/global-routing fallback.
    pub fallback_entries: usize,
}

/// Global routing-rule summary attached to the targeted role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRuleSummary {
    /// Optional operator-defined priority.
    pub priority: Option<u32>,
    /// Route match criteria.
    pub when: RoutingMatch,
}

impl RoutingRuleSummary {
    fn render(&self) -> String {
        let mut parts = Vec::new();
        if let Some(priority) = self.priority {
            parts.push(format!("priority={priority}"));
        }
        append_match_parts(&mut parts, &self.when);
        parts.join(", ")
    }
}

/// Concise summary of a role's loaded instruction pack.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstructionPackSummary {
    /// First non-empty role-prompt paragraph, when a role prompt exists.
    pub role_prompt_summary: Option<String>,
    /// Whether this role has a loaded SOUL file. The full SOUL is not
    /// rendered into peer catalog entries.
    pub has_soul: bool,
}

/// Builder for a platform-lead catalog.
pub struct RoleCatalogBuilder<'a> {
    /// Workflow configuration.
    pub workflow: &'a WorkflowConfig,
    /// Loaded instruction packs.
    pub instruction_packs: &'a InstructionPackBundle,
    /// Current platform-lead role. Excluded from the child catalog.
    pub current_role: &'a str,
}

impl RoleCatalogBuilder<'_> {
    /// Build a deterministic catalog.
    pub fn build(self) -> RoleCatalog {
        let mut entries = Vec::new();

        for (name, role) in &self.workflow.roles {
            if name == self.current_role || !self.role_is_eligible(name, role) {
                continue;
            }
            let matching_global_rules = matching_global_rules(self.workflow, name);
            let agent_profile = role.agent.clone();
            let (agent_backend, tools) =
                agent_profile_details(self.workflow, agent_profile.as_deref());
            let assignment = &role.assignment;
            let structured_assignment = !assignment_is_empty(assignment);
            let instruction_pack_summary = self
                .instruction_packs
                .roles
                .get(name)
                .map(instruction_pack_summary)
                .unwrap_or_default();

            entries.push(RoleCatalogEntry {
                name: name.clone(),
                kind: role.kind,
                description: role.description.clone(),
                agent_profile,
                agent_backend,
                max_concurrent: role.max_concurrent,
                tools,
                owns: assignment.owns.clone(),
                does_not_own: assignment.does_not_own.clone(),
                requires: assignment.requires.clone(),
                handoff_expectations: assignment.handoff_expectations.clone(),
                routing_hints: assignment.routing_hints.clone(),
                used_fallback_assignment: !structured_assignment,
                matching_global_rules,
                instruction_pack_summary,
            });
        }

        let source_summary = RoleCatalogSourceSummary {
            total_entries: entries.len(),
            structured_entries: entries
                .iter()
                .filter(|entry| !entry.used_fallback_assignment)
                .count(),
            fallback_entries: entries
                .iter()
                .filter(|entry| entry.used_fallback_assignment)
                .count(),
        };

        RoleCatalog {
            entries,
            source_summary,
        }
    }

    fn role_is_eligible(&self, name: &str, role: &RoleConfig) -> bool {
        match role.kind {
            RoleKind::Specialist => true,
            RoleKind::Reviewer => {
                self.workflow.followups.enabled
                    || role_has_assignment_or_route(self.workflow, name, role)
            }
            RoleKind::Operator => role_has_assignment_or_route(self.workflow, name, role),
            RoleKind::IntegrationOwner | RoleKind::QaGate | RoleKind::Custom => false,
        }
    }
}

fn agent_profile_details(
    workflow: &WorkflowConfig,
    profile_name: Option<&str>,
) -> (Option<AgentBackend>, Vec<String>) {
    let Some(profile_name) = profile_name else {
        return (None, Vec::new());
    };
    let Some(profile) = workflow.agents.get(profile_name) else {
        return (None, Vec::new());
    };
    match profile {
        AgentProfileConfig::Backend(backend) => (Some(backend.backend), backend.tools.clone()),
        AgentProfileConfig::Composite(_) => (None, Vec::new()),
    }
}

fn matching_global_rules(workflow: &WorkflowConfig, role_name: &str) -> Vec<RoutingRuleSummary> {
    workflow
        .routing
        .rules
        .iter()
        .filter(|rule| rule.assign_role == role_name)
        .map(|rule| RoutingRuleSummary {
            priority: rule.priority,
            when: rule.when.clone(),
        })
        .collect()
}

fn role_has_assignment_or_route(
    workflow: &WorkflowConfig,
    role_name: &str,
    role: &RoleConfig,
) -> bool {
    !assignment_is_empty(&role.assignment)
        || workflow
            .routing
            .rules
            .iter()
            .any(|rule| rule.assign_role == role_name)
}

fn assignment_is_empty(assignment: &RoleAssignmentMetadata) -> bool {
    assignment.owns.is_empty()
        && assignment.does_not_own.is_empty()
        && assignment.requires.is_empty()
        && assignment.handoff_expectations.is_empty()
        && assignment.routing_hints.paths_any.is_empty()
        && assignment.routing_hints.labels_any.is_empty()
        && assignment.routing_hints.issue_types_any.is_empty()
        && assignment.routing_hints.domains_any.is_empty()
}

fn instruction_pack_summary(pack: &LoadedRoleInstructionPack) -> InstructionPackSummary {
    InstructionPackSummary {
        role_prompt_summary: pack
            .role_prompt
            .as_ref()
            .and_then(|instruction| first_non_empty_paragraph(&instruction.content)),
        has_soul: pack.soul.is_some(),
    }
}

fn first_non_empty_paragraph(content: &str) -> Option<String> {
    content
        .split("\n\n")
        .map(|paragraph| paragraph.trim().replace('\n', " "))
        .find(|paragraph| !paragraph.is_empty())
}

fn render_list(out: &mut String, label: &str, values: &[String]) {
    if !values.is_empty() {
        out.push_str(&format!("- {label}: {}\n", values.join("; ")));
    }
}

fn append_match_parts(parts: &mut Vec<String>, when: &RoutingMatch) {
    if !when.labels_any.is_empty() {
        parts.push(format!("labels_any=[{}]", when.labels_any.join("|")));
    }
    if !when.paths_any.is_empty() {
        parts.push(format!("paths_any=[{}]", when.paths_any.join("|")));
    }
    if let Some(issue_size) = &when.issue_size {
        parts.push(format!("issue_size={issue_size}"));
    }
}

fn role_kind_label(kind: RoleKind) -> &'static str {
    match kind {
        RoleKind::IntegrationOwner => "integration_owner",
        RoleKind::QaGate => "qa_gate",
        RoleKind::Specialist => "specialist",
        RoleKind::Reviewer => "reviewer",
        RoleKind::Operator => "operator",
        RoleKind::Custom => "custom",
    }
}

fn agent_backend_label(backend: AgentBackend) -> &'static str {
    match backend {
        AgentBackend::Codex => "codex",
        AgentBackend::Claude => "claude",
        AgentBackend::Mock => "mock",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigValidationWarning;
    use crate::loader::{
        InstructionKind, InstructionSource, LoadedRoleInstruction, LoadedRoleInstructionPack,
    };

    fn workflow(raw: &str) -> WorkflowConfig {
        serde_yaml::from_str(raw).unwrap()
    }

    #[test]
    fn catalog_includes_eligible_child_roles_with_metadata() {
        let cfg = workflow(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  qa:
    kind: qa_gate
  backend:
    kind: specialist
    description: Backend implementation
    agent: codex_fast
    max_concurrent: 2
    assignment:
      owns: [Rust core]
      does_not_own: [TUI polish]
      requires: [acceptance criteria]
      handoff_expectations: [tests run]
      routing_hints:
        paths_any: [crates/**]
        labels_any: [backend]
  reviewer:
    kind: reviewer
    description: Security review
    agent: claude_review
  release_ops:
    kind: operator
    description: Release work
    assignment:
      owns: [deployments]
agents:
  codex_fast:
    backend: codex
    tools: [git, tracker]
  claude_review:
    backend: claude
routing:
  rules:
    - priority: 20
      when:
        paths_any: [crates/**]
      assign_role: backend
followups:
  enabled: true
"#,
        );

        let catalog = RoleCatalogBuilder {
            workflow: &cfg,
            instruction_packs: &InstructionPackBundle::default(),
            current_role: "platform_lead",
        }
        .build();

        let names: Vec<_> = catalog
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["backend", "release_ops", "reviewer"]);
        let backend = catalog
            .entries
            .iter()
            .find(|entry| entry.name == "backend")
            .unwrap();
        assert_eq!(backend.kind, RoleKind::Specialist);
        assert_eq!(backend.agent_backend, Some(AgentBackend::Codex));
        assert_eq!(backend.tools, vec!["git", "tracker"]);
        assert_eq!(backend.max_concurrent, Some(2));
        assert_eq!(backend.owns, vec!["Rust core"]);
        assert_eq!(backend.does_not_own, vec!["TUI polish"]);
        assert_eq!(backend.requires, vec!["acceptance criteria"]);
        assert_eq!(backend.handoff_expectations, vec!["tests run"]);
        assert_eq!(backend.routing_hints.paths_any, vec!["crates/**"]);
        assert_eq!(backend.routing_hints.labels_any, vec!["backend"]);
        assert_eq!(backend.matching_global_rules.len(), 1);
        assert_eq!(catalog.source_summary.total_entries, 3);
        assert_eq!(catalog.source_summary.structured_entries, 2);
        assert_eq!(catalog.source_summary.fallback_entries, 1);
    }

    #[test]
    fn catalog_excludes_qa_and_other_integration_owners() {
        let cfg = workflow(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  secondary_lead:
    kind: integration_owner
  qa:
    kind: qa_gate
  backend:
    kind: specialist
"#,
        );

        let catalog = RoleCatalogBuilder {
            workflow: &cfg,
            instruction_packs: &InstructionPackBundle::default(),
            current_role: "platform_lead",
        }
        .build();

        let names: Vec<_> = catalog
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, vec!["backend"]);
    }

    #[test]
    fn catalog_rendering_is_deterministic_and_includes_instruction_summary() {
        let cfg = workflow(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  backend:
    kind: specialist
    description: Backend implementation
    assignment:
      owns: [Rust core]
"#,
        );
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "backend".to_string(),
            LoadedRoleInstructionPack {
                role_prompt: Some(LoadedRoleInstruction {
                    source: InstructionSource {
                        role: "backend".to_string(),
                        kind: InstructionKind::RolePrompt,
                        path: ".symphony/roles/backend/AGENTS.md".into(),
                        content_hash: "abc".to_string(),
                        modified_unix_ms: Some(1),
                        loaded_at_unix_ms: 2,
                    },
                    content: "Backend doctrine.\n\nExtra details stay out.".to_string(),
                }),
                soul: Some(LoadedRoleInstruction {
                    source: InstructionSource {
                        role: "backend".to_string(),
                        kind: InstructionKind::Soul,
                        path: ".symphony/roles/backend/SOUL.md".into(),
                        content_hash: "def".to_string(),
                        modified_unix_ms: Some(1),
                        loaded_at_unix_ms: 2,
                    },
                    content: "Private soul text".to_string(),
                }),
            },
        );

        let first = RoleCatalogBuilder {
            workflow: &cfg,
            instruction_packs: &packs,
            current_role: "platform_lead",
        }
        .build()
        .render_for_prompt();
        let second = RoleCatalogBuilder {
            workflow: &cfg,
            instruction_packs: &packs,
            current_role: "platform_lead",
        }
        .build()
        .render_for_prompt();

        assert_eq!(first, second);
        assert!(first.contains("## backend"));
        assert!(first.contains("- instruction_summary: Backend doctrine."));
        assert!(first.contains("- has_soul: true"));
        assert!(!first.contains("Private soul text"));
    }

    #[test]
    fn thin_specialist_metadata_still_emits_validation_warning() {
        let cfg = workflow(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  backend:
    kind: specialist
    description: Backend implementation
decomposition:
  enabled: true
"#,
        );

        let warnings = cfg.validation_warnings();
        assert!(warnings.iter().any(|warning| matches!(
            warning,
            ConfigValidationWarning::SpecialistRoleMissingAssignmentMetadata { role }
                if role == "backend"
        )));
    }
}
