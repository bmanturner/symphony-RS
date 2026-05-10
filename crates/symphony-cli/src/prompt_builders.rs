//! v4 prompt builders for decomposition and integration-owner runs.

#![allow(dead_code)]

use anyhow::{Context, Result};
use symphony_config::{InstructionPackBundle, RoleCatalogBuilder, WorkflowConfig};
use symphony_core::handoff::Handoff;
use symphony_core::integration_request::IntegrationRunRequest;
use symphony_core::prompt::{
    PromptAssembly, PromptBlocker, PromptContext, PromptParent, PromptSection, PromptSectionKind,
    PromptSectionProvenance, PromptWorkspace, default_handoff_output_schema,
    render as render_prompt,
};
use symphony_core::tracker::Issue;

pub(crate) fn build_decomposition_prompt(
    workflow: &WorkflowConfig,
    instruction_packs: &InstructionPackBundle,
    workflow_prompt: &str,
    platform_role: &str,
    parent_issue: &Issue,
) -> Result<PromptAssembly> {
    let ctx = PromptContext::for_issue(parent_issue);
    let global = render_prompt(workflow_prompt, &ctx).context("render workflow prompt")?;
    let role_pack = instruction_packs.roles.get(platform_role);
    let catalog = RoleCatalogBuilder {
        workflow,
        instruction_packs,
        current_role: platform_role,
    }
    .build()
    .render_for_prompt();

    let mut sections = vec![
        PromptSection::new(PromptSectionKind::GlobalWorkflowInstructions, global).with_provenance(
            PromptSectionProvenance {
                source: "WORKFLOW.md".into(),
                content_hash: None,
            },
        ),
    ];

    if let Some(pack) = role_pack {
        if let Some(role_prompt) = &pack.role_prompt {
            sections.push(
                PromptSection::new(PromptSectionKind::RolePrompt, role_prompt.content.clone())
                    .with_provenance(PromptSectionProvenance {
                        source: role_prompt.source.path.display().to_string(),
                        content_hash: Some(role_prompt.source.content_hash.clone()),
                    }),
            );
        }
        if let Some(soul) = &pack.soul {
            sections.push(
                PromptSection::new(PromptSectionKind::RoleSoul, soul.content.clone())
                    .with_provenance(PromptSectionProvenance {
                        source: soul.source.path.display().to_string(),
                        content_hash: Some(soul.source.content_hash.clone()),
                    }),
            );
        }
    }

    sections.extend([
        PromptSection::new(PromptSectionKind::IssueContext, issue_context(parent_issue)),
        PromptSection::new(
            PromptSectionKind::DecompositionPolicy,
            format!(
                "child_issue_policy: {:?}\nrequire_acceptance_criteria_per_child: {}\ndependency_dispatch_gate: {}",
                workflow.decomposition.child_issue_policy,
                workflow.decomposition.require_acceptance_criteria_per_child,
                workflow.decomposition.dependency_policy.dispatch_gate
            ),
        ),
        PromptSection::new(PromptSectionKind::RoleCatalog, catalog),
        PromptSection::new(
            PromptSectionKind::Blockers,
            format!("tracker_blocked_by: {}", ctx.issue.blocked_by.count),
        ),
        PromptSection::new(
            PromptSectionKind::OutputSchema,
            "Emit a decomposition draft with children, assigned_role, acceptance_criteria, and depends_on edges.",
        ),
    ]);

    Ok(PromptAssembly::new(sections))
}

pub(crate) fn build_integration_owner_prompt(
    workflow_prompt: &str,
    owner_role: &str,
    request: &IntegrationRunRequest,
) -> Result<PromptAssembly> {
    let global = workflow_prompt.trim().to_string();
    let children = render_integration_children(&request.children);
    Ok(PromptAssembly::new(vec![
        PromptSection::new(PromptSectionKind::GlobalWorkflowInstructions, global).with_provenance(
            PromptSectionProvenance {
                source: "WORKFLOW.md".into(),
                content_hash: None,
            },
        ),
        PromptSection::new(
            PromptSectionKind::IssueContext,
            format!(
                "parent: {} - {}\nowner_role: {}\nrequest_owner_role: {}\ncause: {}\nopen_blocker_count: {}",
                request.parent_identifier,
                request.parent_title,
                owner_role,
                request.owner_role,
                request.cause,
                request.open_blocker_count
            ),
        ),
        PromptSection::new(
            PromptSectionKind::ParentChildGraph,
            format!("Children and latest handoffs:\n{children}"),
        ),
        PromptSection::new(
            PromptSectionKind::WorkspaceBranch,
            format!(
                "path: {}\nstrategy: {}\nbranch: {}\nbase_ref: {}",
                request.workspace.path.display(),
                request.workspace.strategy,
                request.workspace.branch.as_deref().unwrap_or(""),
                request.workspace.base_ref.as_deref().unwrap_or("")
            ),
        ),
        PromptSection::new(
            PromptSectionKind::OutputSchema,
            default_handoff_output_schema(),
        ),
    ]))
}

pub(crate) fn build_specialist_prompt(
    workflow_prompt: &str,
    instruction_packs: &InstructionPackBundle,
    role_name: &str,
    parent: Option<PromptParent>,
    current_issue: &Issue,
    workspace: Option<PromptWorkspace>,
    blockers: Vec<PromptBlocker>,
    acceptance_criteria: Vec<String>,
) -> Result<PromptAssembly> {
    let mut ctx = PromptContext::for_issue(current_issue)
        .with_blockers(blockers.clone())
        .with_acceptance_criteria(acceptance_criteria.clone())
        .with_output_schema(default_handoff_output_schema());
    if let Some(parent) = parent.clone() {
        ctx = ctx.with_parent(parent);
    }
    if let Some(workspace) = workspace.clone() {
        ctx = ctx.with_workspace(workspace);
    }
    let global = render_prompt(workflow_prompt, &ctx).context("render workflow prompt")?;

    let mut sections = vec![PromptSection::new(
        PromptSectionKind::GlobalWorkflowInstructions,
        global,
    )];
    if let Some(pack) = instruction_packs.roles.get(role_name) {
        if let Some(role_prompt) = &pack.role_prompt {
            sections.push(
                PromptSection::new(PromptSectionKind::RolePrompt, role_prompt.content.clone())
                    .with_provenance(PromptSectionProvenance {
                        source: role_prompt.source.path.display().to_string(),
                        content_hash: Some(role_prompt.source.content_hash.clone()),
                    }),
            );
        }
        if let Some(soul) = &pack.soul {
            sections.push(
                PromptSection::new(PromptSectionKind::RoleSoul, soul.content.clone())
                    .with_provenance(PromptSectionProvenance {
                        source: soul.source.path.display().to_string(),
                        content_hash: Some(soul.source.content_hash.clone()),
                    }),
            );
        }
    }

    sections.extend([
        PromptSection::new(
            PromptSectionKind::IssueContext,
            issue_context(current_issue),
        ),
        PromptSection::new(
            PromptSectionKind::ParentChildGraph,
            parent
                .map(|p| format!("parent: {} - {}", p.identifier, p.title))
                .unwrap_or_else(|| "parent: none".into()),
        ),
        PromptSection::new(
            PromptSectionKind::Blockers,
            if blockers.is_empty() {
                "none".into()
            } else {
                blockers
                    .iter()
                    .map(|b| format!("- [{}] {}", b.severity, b.reason))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ),
        PromptSection::new(
            PromptSectionKind::WorkspaceBranch,
            workspace
                .map(|w| {
                    format!(
                        "path: {}\nstrategy: {}\nbranch: {}\nbase_ref: {}",
                        w.path.display(),
                        w.strategy,
                        w.branch.as_deref().unwrap_or(""),
                        w.base_ref.as_deref().unwrap_or("")
                    )
                })
                .unwrap_or_else(|| "workspace: unclaimed".into()),
        ),
        PromptSection::new(
            PromptSectionKind::AcceptanceCriteria,
            acceptance_criteria
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        PromptSection::new(
            PromptSectionKind::OutputSchema,
            default_handoff_output_schema(),
        ),
    ]);

    Ok(PromptAssembly::new(sections))
}

pub(crate) fn build_qa_prompt(
    workflow_prompt: &str,
    instruction_packs: &InstructionPackBundle,
    qa_role: &str,
    issue: &Issue,
    child_handoffs: &[symphony_core::integration_request::IntegrationChild],
    known_blockers: Vec<PromptBlocker>,
    acceptance_criteria: Vec<String>,
    ci_status: Option<&str>,
) -> Result<PromptAssembly> {
    let ctx = PromptContext::for_issue(issue)
        .with_blockers(known_blockers.clone())
        .with_acceptance_criteria(acceptance_criteria.clone());
    let mut sections = vec![PromptSection::new(
        PromptSectionKind::GlobalWorkflowInstructions,
        render_prompt(workflow_prompt, &ctx).context("render workflow prompt")?,
    )];
    if let Some(pack) = instruction_packs.roles.get(qa_role) {
        if let Some(role_prompt) = &pack.role_prompt {
            sections.push(PromptSection::new(
                PromptSectionKind::RolePrompt,
                role_prompt.content.clone(),
            ));
        }
        if let Some(soul) = &pack.soul {
            sections.push(PromptSection::new(
                PromptSectionKind::RoleSoul,
                soul.content.clone(),
            ));
        }
    }
    sections.extend([
        PromptSection::new(
            PromptSectionKind::IssueContext,
            format!(
                "{}\nci_status: {}\nqa_scope: integrated output gate",
                issue_context(issue),
                ci_status.unwrap_or("unknown")
            ),
        ),
        PromptSection::new(
            PromptSectionKind::ParentChildGraph,
            render_integration_children(child_handoffs),
        ),
        PromptSection::new(
            PromptSectionKind::Blockers,
            if known_blockers.is_empty() {
                "none".into()
            } else {
                known_blockers
                    .iter()
                    .map(|b| format!("- [{}] {}", b.severity, b.reason))
                    .collect::<Vec<_>>()
                    .join("\n")
            },
        ),
        PromptSection::new(
            PromptSectionKind::AcceptanceCriteria,
            acceptance_criteria
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        PromptSection::new(
            PromptSectionKind::OutputSchema,
            "Emit a QA verdict with verdict, evidence, acceptance_trace, blockers_created, waiver_role, and reason.",
        ),
    ]);
    Ok(PromptAssembly::new(sections))
}

fn issue_context(issue: &Issue) -> String {
    format!(
        "identifier: {}\ntitle: {}\nlabels: {}\npriority: {}\nblocked_by: {}",
        issue.identifier,
        issue.title,
        issue.labels.join(", "),
        issue.priority.map(|p| p.to_string()).unwrap_or_default(),
        issue.blocked_by.len()
    )
}

fn render_integration_children(
    children: &[symphony_core::integration_request::IntegrationChild],
) -> String {
    children
        .iter()
        .map(|child| {
            let handoff = child
                .latest_handoff
                .as_ref()
                .map(render_handoff)
                .unwrap_or_else(|| "latest_handoff: none".to_string());
            format!(
                "- {}: {} [{}]\n  branch: {}\n  {}",
                child.identifier,
                child.title,
                child.status_class,
                child.branch.as_deref().unwrap_or(""),
                handoff.replace('\n', "\n  ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_handoff(handoff: &Handoff) -> String {
    format!(
        "latest_handoff:\n  summary: {}\n  changed_files: {}\n  tests_run: {}\n  verification_evidence: {}\n  known_risks: {}\n  branch_or_workspace: {}\n  ready_for: {}",
        handoff.summary,
        handoff.changed_files.join(", "),
        handoff.tests_run.join(", "),
        handoff.verification_evidence.join(", "),
        handoff.known_risks.join(", "),
        render_branch_or_workspace(&handoff.branch_or_workspace),
        handoff.ready_for.as_str()
    )
}

fn render_branch_or_workspace(value: &symphony_core::handoff::BranchOrWorkspace) -> String {
    format!(
        "branch={}, workspace_path={}, base_ref={}",
        value.branch.as_deref().unwrap_or(""),
        value.workspace_path.as_deref().unwrap_or(""),
        value.base_ref.as_deref().unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use symphony_config::{
        InstructionKind, InstructionSource, LoadedRoleInstruction, LoadedRoleInstructionPack,
    };
    use symphony_core::handoff::{BranchOrWorkspace, ReadyFor};
    use symphony_core::integration_request::{
        IntegrationChild, IntegrationGates, IntegrationRequestCause, IntegrationWorkspace,
    };
    use symphony_core::role::RoleName;
    use symphony_core::tracker::Issue;
    use symphony_core::work_item::{WorkItemId, WorkItemStatusClass};

    fn workflow() -> WorkflowConfig {
        serde_yaml::from_str(
            r#"
roles:
  platform_lead:
    kind: integration_owner
  backend:
    kind: specialist
    assignment:
      owns: [Rust backend]
  frontend:
    kind: specialist
    assignment:
      owns: [UI]
  reviewer:
    kind: reviewer
    assignment:
      owns: [security review]
  operator:
    kind: operator
    assignment:
      owns: [deployments]
  qa:
    kind: qa_gate
followups:
  enabled: true
"#,
        )
        .unwrap()
    }

    fn packs() -> InstructionPackBundle {
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "platform_lead".into(),
            LoadedRoleInstructionPack {
                role_prompt: Some(LoadedRoleInstruction {
                    source: InstructionSource {
                        role: "platform_lead".into(),
                        kind: InstructionKind::RolePrompt,
                        path: ".symphony/roles/platform_lead/AGENTS.md".into(),
                        content_hash: "leadhash".into(),
                        modified_unix_ms: Some(1),
                        loaded_at_unix_ms: 2,
                    },
                    content: "Lead decomposes broad issues.".into(),
                }),
                soul: Some(LoadedRoleInstruction {
                    source: InstructionSource {
                        role: "platform_lead".into(),
                        kind: InstructionKind::Soul,
                        path: ".symphony/roles/platform_lead/SOUL.md".into(),
                        content_hash: "soulhash".into(),
                        modified_unix_ms: Some(1),
                        loaded_at_unix_ms: 2,
                    },
                    content: "Be precise.".into(),
                }),
            },
        );
        packs
    }

    fn specialist_pack(role: &str, prompt: &str, soul: &str) -> LoadedRoleInstructionPack {
        LoadedRoleInstructionPack {
            role_prompt: Some(LoadedRoleInstruction {
                source: InstructionSource {
                    role: role.into(),
                    kind: InstructionKind::RolePrompt,
                    path: format!(".symphony/roles/{role}/AGENTS.md").into(),
                    content_hash: format!("{role}-prompt"),
                    modified_unix_ms: Some(1),
                    loaded_at_unix_ms: 2,
                },
                content: prompt.into(),
            }),
            soul: Some(LoadedRoleInstruction {
                source: InstructionSource {
                    role: role.into(),
                    kind: InstructionKind::Soul,
                    path: format!(".symphony/roles/{role}/SOUL.md").into(),
                    content_hash: format!("{role}-soul"),
                    modified_unix_ms: Some(1),
                    loaded_at_unix_ms: 2,
                },
                content: soul.into(),
            }),
        }
    }

    #[test]
    fn decomposition_prompt_includes_catalog_and_excludes_qa_child_role() {
        let mut issue = Issue::minimal("p", "ENG-1", "Broad work", "Todo");
        issue.labels = vec!["epic".into()];
        let prompt = build_decomposition_prompt(
            &workflow(),
            &packs(),
            "Global {{identifier}} labels {{labels}}",
            "platform_lead",
            &issue,
        )
        .unwrap()
        .render();

        assert!(prompt.contains("Global ENG-1 labels epic"));
        assert!(prompt.contains("Lead decomposes broad issues."));
        assert!(prompt.contains("Be precise."));
        assert!(prompt.contains("## Role Catalog"));
        assert!(prompt.contains("## backend"));
        assert!(prompt.contains("## frontend"));
        assert!(prompt.contains("## reviewer"));
        assert!(prompt.contains("## operator"));
        assert!(!prompt.contains("## qa"));
        assert!(prompt.contains("assigned_role"));
    }

    #[test]
    fn integration_prompt_includes_latest_handoff_fields_and_missing_handoff_marker() {
        let handoff = Handoff {
            summary: "Backend complete".into(),
            changed_files: vec!["crates/api.rs".into()],
            tests_run: vec!["cargo test -p api".into()],
            verification_evidence: vec!["green tests".into()],
            known_risks: vec!["migration risk".into()],
            blockers_created: Vec::new(),
            followups_created_or_proposed: Vec::new(),
            branch_or_workspace: BranchOrWorkspace {
                branch: Some("feat/backend".into()),
                workspace_path: Some("/tmp/backend".into()),
                base_ref: Some("main".into()),
            },
            ready_for: ReadyFor::Integration,
            block_reason: None,
            reporting_role: Some(RoleName::new("backend")),
            verdict_request: None,
        };
        let request = IntegrationRunRequest::try_new(
            WorkItemId::new(1),
            "ENG-1",
            "Broad work",
            RoleName::new("platform_lead"),
            symphony_core::IntegrationMergeStrategy::SequentialCherryPick,
            IntegrationWorkspace {
                path: PathBuf::from("/tmp/integration"),
                strategy: "git_worktree".into(),
                branch: Some("integration/eng-1".into()),
                base_ref: Some("main".into()),
            },
            vec![
                IntegrationChild {
                    child_id: WorkItemId::new(2),
                    identifier: "ENG-2".into(),
                    title: "Backend".into(),
                    status_class: WorkItemStatusClass::Done,
                    branch: Some("feat/backend".into()),
                    latest_handoff: Some(handoff),
                },
                IntegrationChild {
                    child_id: WorkItemId::new(3),
                    identifier: "ENG-3".into(),
                    title: "Frontend".into(),
                    status_class: WorkItemStatusClass::Done,
                    branch: Some("feat/frontend".into()),
                    latest_handoff: None,
                },
            ],
            IntegrationGates::default(),
            0,
            IntegrationRequestCause::AllChildrenTerminal,
        )
        .unwrap();
        let prompt = build_integration_owner_prompt("Global", "platform_lead", &request)
            .unwrap()
            .render();

        assert!(prompt.contains("summary: Backend complete"));
        assert!(prompt.contains("changed_files: crates/api.rs"));
        assert!(prompt.contains("tests_run: cargo test -p api"));
        assert!(prompt.contains("verification_evidence: green tests"));
        assert!(prompt.contains("known_risks: migration risk"));
        assert!(prompt.contains("branch_or_workspace: branch=feat/backend"));
        assert!(prompt.contains("ready_for: integration"));
        assert!(prompt.contains("ENG-3: Frontend"));
        assert!(prompt.contains("latest_handoff: none"));
    }

    #[test]
    fn specialist_prompt_includes_only_current_role_doctrine() {
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "backend".into(),
            specialist_pack("backend", "Backend role prompt", "Backend soul"),
        );
        packs.roles.insert(
            "frontend".into(),
            specialist_pack("frontend", "Frontend role prompt", "Frontend soul"),
        );
        let mut issue = Issue::minimal("c", "ENG-2", "Backend child", "Todo");
        issue.labels = vec!["backend".into()];
        let prompt = build_specialist_prompt(
            "Global {{identifier}}",
            &packs,
            "backend",
            Some(PromptParent {
                identifier: "ENG-1".into(),
                title: "Parent".into(),
                url: None,
            }),
            &issue,
            Some(PromptWorkspace {
                path: PathBuf::from("/tmp/backend"),
                strategy: "git_worktree".into(),
                branch: Some("feat/backend".into()),
                base_ref: Some("main".into()),
            }),
            Vec::new(),
            vec!["API works".into()],
        )
        .unwrap()
        .render();

        assert!(prompt.contains("Backend role prompt"));
        assert!(prompt.contains("Backend soul"));
        assert!(!prompt.contains("Frontend role prompt"));
        assert!(!prompt.contains("Frontend soul"));
        assert!(prompt.contains("parent: ENG-1 - Parent"));
        assert!(prompt.contains("branch: feat/backend"));
        assert!(prompt.contains("- API works"));
        assert!(prompt.contains("\"ready_for\""));
    }

    #[test]
    fn blocked_specialist_prompt_surfaces_dependency_reason() {
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "backend".into(),
            specialist_pack("backend", "Backend role prompt", "Backend soul"),
        );
        let issue = Issue::minimal("c", "ENG-2", "Backend child", "Todo");
        let prompt = build_specialist_prompt(
            "Global",
            &packs,
            "backend",
            None,
            &issue,
            None,
            vec![PromptBlocker {
                id: None,
                blocking_id: Some(WorkItemId::new(1)),
                reason: "waiting for schema child".into(),
                severity: symphony_core::BlockerSeverity::High,
            }],
            Vec::new(),
        )
        .unwrap()
        .render();

        assert!(prompt.contains("waiting for schema child"));
        assert!(prompt.contains("[high]"));
    }

    #[test]
    fn qa_prompt_includes_verdict_schema_and_child_handoffs() {
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "qa".into(),
            specialist_pack("qa", "QA role prompt", "QA soul"),
        );
        let issue = Issue::minimal("p", "ENG-1", "Integrated broad work", "QA");
        let handoff = Handoff {
            summary: "Backend complete".into(),
            changed_files: vec!["crates/api.rs".into()],
            tests_run: vec!["cargo test -p api".into()],
            verification_evidence: vec!["green tests".into()],
            known_risks: Vec::new(),
            blockers_created: Vec::new(),
            followups_created_or_proposed: Vec::new(),
            branch_or_workspace: BranchOrWorkspace {
                branch: Some("feat/backend".into()),
                workspace_path: Some("/tmp/backend".into()),
                base_ref: Some("main".into()),
            },
            ready_for: ReadyFor::Qa,
            block_reason: None,
            reporting_role: Some(RoleName::new("backend")),
            verdict_request: None,
        };
        let children = vec![IntegrationChild {
            child_id: WorkItemId::new(2),
            identifier: "ENG-2".into(),
            title: "Backend".into(),
            status_class: WorkItemStatusClass::Done,
            branch: Some("feat/backend".into()),
            latest_handoff: Some(handoff),
        }];

        let prompt = build_qa_prompt(
            "Global {{identifier}}",
            &packs,
            "qa",
            &issue,
            &children,
            Vec::new(),
            vec!["Acceptance traced".into()],
            Some("green"),
        )
        .unwrap()
        .render();

        assert!(prompt.contains("QA role prompt"));
        assert!(prompt.contains("QA soul"));
        assert!(prompt.contains("qa_scope: integrated output gate"));
        assert!(prompt.contains("ci_status: green"));
        assert!(prompt.contains("summary: Backend complete"));
        assert!(prompt.contains("changed_files: crates/api.rs"));
        assert!(prompt.contains("- Acceptance traced"));
        assert!(prompt.contains("verdict"));
        assert!(prompt.contains("acceptance_trace"));
        assert!(prompt.contains("evidence"));
    }

    #[test]
    fn qa_prompt_uses_same_handoff_rendering_as_integration_prompt() {
        let children = vec![IntegrationChild {
            child_id: WorkItemId::new(3),
            identifier: "ENG-3".into(),
            title: "Frontend".into(),
            status_class: WorkItemStatusClass::Done,
            branch: Some("feat/frontend".into()),
            latest_handoff: None,
        }];
        let issue = Issue::minimal("p", "ENG-1", "Integrated broad work", "QA");
        let qa_prompt = build_qa_prompt(
            "Global",
            &InstructionPackBundle::default(),
            "qa",
            &issue,
            &children,
            Vec::new(),
            Vec::new(),
            None,
        )
        .unwrap()
        .render();
        let rendered_children = render_integration_children(&children);
        assert!(qa_prompt.contains(&rendered_children));
        assert!(qa_prompt.contains("latest_handoff: none"));
    }
}
