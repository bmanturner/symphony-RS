//! Prompt context and rendering (SPEC v2 §10.2, §4.4–§4.9).
//!
//! Phase 7 of the v2 checklist asks the prompt the kernel hands an agent
//! to carry more than the issue body. The agent needs to know:
//!
//! * which role it is acting as and which kernel authorities that role
//!   carries (SPEC §4.4);
//! * which workspace claim was provisioned for the run, including the
//!   strategy, base ref, and branch the dispatcher expects it to be on
//!   (SPEC §4.6 / §5.8);
//! * the parent/child context for the current work item — broad parents
//!   know about their children, specialists know about their parent;
//! * any open blockers that bear on the current run (SPEC §4.8);
//! * the acceptance criteria the QA gate will trace against (SPEC §4.9);
//! * the structured output schema the agent must conform to so the
//!   kernel can parse the handoff downstream (SPEC §4.7).
//!
//! This module supplies the typed [`PromptContext`] envelope that
//! collects those fields and a [`render`] function that fills
//! `{{path.to.value}}` placeholders against it. The renderer is
//! deliberately strict: an unknown path, an unclosed `{{`, or an
//! empty placeholder is an error. Operators get a loud failure at
//! prompt-build time rather than an agent prompt that silently
//! contains literal `{{role.bogus}}` text.
//!
//! ## Substitution surface
//!
//! Single-value placeholders (empty string when the optional source is
//! missing):
//!
//! * `{{identifier}}`, `{{title}}`, `{{description}}`, `{{state}}`,
//!   `{{branch_name}}`, `{{url}}` — from [`PromptIssue`].
//! * `{{role.name}}`, `{{role.kind}}`, `{{role.authority}}` — from
//!   [`PromptContext::role`]. `role.authority` renders as a sorted,
//!   comma-separated list of the granted authority flags.
//! * `{{workspace.path}}`, `{{workspace.strategy}}`,
//!   `{{workspace.branch}}`, `{{workspace.base_ref}}` — from
//!   [`PromptContext::workspace`].
//! * `{{parent.identifier}}`, `{{parent.title}}`, `{{parent.url}}` —
//!   from [`PromptContext::parent`].
//! * `{{output_schema}}` — verbatim from
//!   [`PromptContext::output_schema`].
//!
//! List placeholders (rendered as one bullet per entry, empty string
//! when the list is empty):
//!
//! * `{{children}}` — `- {identifier}: {title} [{status}]`.
//! * `{{blockers}}` — `- [{severity}] {reason}`.
//! * `{{acceptance_criteria}}` — `- {item}`.
//!
//! Any placeholder that does not resolve to one of the paths above
//! returns [`RenderError::UnknownPath`]. Unclosed `{{` returns
//! [`RenderError::Malformed`]. Empty placeholders (e.g. `{{}}`)
//! return [`RenderError::InvalidPath`].

use std::path::PathBuf;

use crate::blocker::{BlockerId, BlockerSeverity};
use crate::role::{RoleAuthority, RoleContext};
use crate::tracker::Issue;
use crate::work_item::WorkItemId;

/// Issue fields the prompt renderer needs.
///
/// Mirrors the legacy [`Issue`]-driven substitution so existing
/// fixtures keep working without forcing the prompt layer to depend on
/// the wider tracker model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptIssue {
    /// Tracker-facing identifier (e.g. `ENG-7`).
    pub identifier: String,
    /// Human-readable title.
    pub title: String,
    /// Free-form description, when the tracker has one.
    pub description: Option<String>,
    /// Raw tracker state string (e.g. `Todo`).
    pub state: String,
    /// Suggested branch name, when the tracker provides one.
    pub branch_name: Option<String>,
    /// Tracker-facing URL, when known.
    pub url: Option<String>,
}

impl PromptIssue {
    /// Build a [`PromptIssue`] from a normalized [`Issue`].
    pub fn from_issue(issue: &Issue) -> Self {
        Self {
            identifier: issue.identifier.clone(),
            title: issue.title.clone(),
            description: issue.description.clone(),
            state: issue.state.to_string(),
            branch_name: issue.branch_name.clone(),
            url: issue.url.clone(),
        }
    }
}

/// Workspace claim summary surfaced to the agent.
///
/// Slim mirror of `symphony_workspace::WorkspaceClaim` — the prompt
/// layer must not depend on the workspace crate, so the dispatcher
/// projects the relevant fields onto this struct at render time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptWorkspace {
    /// Absolute path to the workspace directory.
    pub path: PathBuf,
    /// Strategy in force (e.g. `git_worktree`, `shared_branch`).
    pub strategy: String,
    /// Branch the run is expected to be on, when applicable.
    pub branch: Option<String>,
    /// Base ref the strategy started from (e.g. `main`).
    pub base_ref: Option<String>,
}

/// Parent work-item summary for child runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptParent {
    /// Tracker identifier of the parent.
    pub identifier: String,
    /// Parent title.
    pub title: String,
    /// Parent URL, when known.
    pub url: Option<String>,
}

/// Child work-item summary for parent/integration-owner runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptChild {
    /// Tracker identifier of the child.
    pub identifier: String,
    /// Child title.
    pub title: String,
    /// Tracker-facing status string (free-form so the prompt does not
    /// pretend the kernel knows the precise classification).
    pub status: String,
}

/// Open-blocker entry surfaced to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBlocker {
    /// Durable blocker id, when one has been minted.
    pub id: Option<BlockerId>,
    /// Work item that causes the block, when known.
    pub blocking_id: Option<WorkItemId>,
    /// Operator-facing reason. Required.
    pub reason: String,
    /// Severity hint.
    pub severity: BlockerSeverity,
}

/// Bundle of context the prompt renderer needs.
///
/// Construct with [`Self::for_issue`] for the minimal v1-equivalent
/// shape and layer additional fields with the `with_*` setters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptContext {
    /// Required issue fields.
    pub issue: PromptIssue,
    /// Role the agent is acting as for this run.
    pub role: Option<RoleContext>,
    /// Workspace claim provisioned for this run.
    pub workspace: Option<PromptWorkspace>,
    /// Parent work-item context, when this is a child run.
    pub parent: Option<PromptParent>,
    /// Child work-items, when this is a parent/integration run.
    pub children: Vec<PromptChild>,
    /// Open blockers relevant to this run.
    pub blockers: Vec<PromptBlocker>,
    /// Acceptance criteria the QA gate will trace against.
    pub acceptance_criteria: Vec<String>,
    /// Structured output schema the agent must conform to.
    pub output_schema: Option<String>,
}

impl PromptContext {
    /// Minimal context carrying only the issue.
    pub fn for_issue(issue: &Issue) -> Self {
        Self {
            issue: PromptIssue::from_issue(issue),
            ..Self::default()
        }
    }

    /// Set the role context.
    pub fn with_role(mut self, role: RoleContext) -> Self {
        self.role = Some(role);
        self
    }

    /// Set the workspace claim summary.
    pub fn with_workspace(mut self, workspace: PromptWorkspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Set the parent context.
    pub fn with_parent(mut self, parent: PromptParent) -> Self {
        self.parent = Some(parent);
        self
    }

    /// Replace the children list.
    pub fn with_children(mut self, children: Vec<PromptChild>) -> Self {
        self.children = children;
        self
    }

    /// Replace the blockers list.
    pub fn with_blockers(mut self, blockers: Vec<PromptBlocker>) -> Self {
        self.blockers = blockers;
        self
    }

    /// Replace the acceptance criteria list.
    pub fn with_acceptance_criteria(mut self, criteria: Vec<String>) -> Self {
        self.acceptance_criteria = criteria;
        self
    }

    /// Set the structured output schema.
    pub fn with_output_schema(mut self, schema: impl Into<String>) -> Self {
        self.output_schema = Some(schema.into());
        self
    }
}

/// Canonical structured-handoff output schema (SPEC v2 §4.7).
///
/// Convenience for `with_output_schema` callers that want the default
/// JSON skeleton matching [`crate::Handoff`]. The dispatcher is free to
/// override per workflow.
pub fn default_handoff_output_schema() -> String {
    r#"{
  "summary": "<one-line operator-facing summary>",
  "changed_files": ["path/to/file"],
  "tests_run": ["cargo test --workspace"],
  "verification_evidence": ["logs/run-N.txt"],
  "known_risks": ["risk description"],
  "blockers_created": [
    {"blocking_id": null, "reason": "...", "severity": "medium"}
  ],
  "followups_created_or_proposed": [
    {"title": "...", "summary": "...", "blocking": false, "propose_only": true}
  ],
  "branch_or_workspace": {
    "branch": "feat/foo",
    "workspace_path": "/abs/path",
    "base_ref": "main"
  },
  "ready_for": "integration | qa | human_review | blocked | done",
  "block_reason": null,
  "reporting_role": null,
  "verdict_request": null
}"#
    .to_string()
}

/// Errors raised by [`render`] when a template cannot be expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The template references a path that the renderer does not know
    /// how to resolve against [`PromptContext`].
    UnknownPath(String),
    /// The template has a `{{` that is never closed by `}}`.
    Malformed(String),
    /// A placeholder resolved to an empty path (e.g. `{{}}` or
    /// `{{ }}`) or contained empty path segments.
    InvalidPath(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::UnknownPath(p) => write!(f, "unknown prompt placeholder: {{{{{p}}}}}"),
            RenderError::Malformed(s) => write!(f, "unclosed prompt placeholder near: {s}"),
            RenderError::InvalidPath(p) => write!(f, "invalid prompt placeholder path: {p:?}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Strictly substitute `{{path.to.value}}` placeholders in `template`
/// against `ctx`.
///
/// See the module docs for the resolvable path surface. Anything else
/// is an error: unknown paths return [`RenderError::UnknownPath`],
/// unclosed `{{` returns [`RenderError::Malformed`], and empty paths
/// return [`RenderError::InvalidPath`].
pub fn render(template: &str, ctx: &PromptContext) -> Result<String, RenderError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("}}") else {
            return Err(RenderError::Malformed(rest[start..].to_string()));
        };
        let raw = &after_open[..end];
        let path = raw.trim();
        if path.is_empty() {
            return Err(RenderError::InvalidPath(raw.to_string()));
        }
        if path.split('.').any(|seg| seg.trim().is_empty()) {
            return Err(RenderError::InvalidPath(path.to_string()));
        }
        out.push_str(&resolve_path(ctx, path)?);
        rest = &after_open[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn resolve_path(ctx: &PromptContext, path: &str) -> Result<String, RenderError> {
    let mut segs = path.split('.');
    let head = segs.next().expect("non-empty path checked by caller");
    let tail: Vec<&str> = segs.collect();
    match (head, tail.as_slice()) {
        ("identifier", []) => Ok(ctx.issue.identifier.clone()),
        ("title", []) => Ok(ctx.issue.title.clone()),
        ("description", []) => Ok(ctx.issue.description.clone().unwrap_or_default()),
        ("state", []) => Ok(ctx.issue.state.clone()),
        ("branch_name", []) => Ok(ctx.issue.branch_name.clone().unwrap_or_default()),
        ("url", []) => Ok(ctx.issue.url.clone().unwrap_or_default()),
        ("output_schema", []) => Ok(ctx.output_schema.clone().unwrap_or_default()),
        ("role", [field]) => {
            let role = ctx.role.as_ref();
            match *field {
                "name" => Ok(role.map(|r| r.name.to_string()).unwrap_or_default()),
                "kind" => Ok(role.map(|r| r.kind.to_string()).unwrap_or_default()),
                "authority" => Ok(role
                    .map(|r| render_authority(&r.authority))
                    .unwrap_or_default()),
                _ => Err(RenderError::UnknownPath(path.to_string())),
            }
        }
        ("workspace", [field]) => {
            let ws = ctx.workspace.as_ref();
            match *field {
                "path" => Ok(ws.map(|w| w.path.display().to_string()).unwrap_or_default()),
                "strategy" => Ok(ws.map(|w| w.strategy.clone()).unwrap_or_default()),
                "branch" => Ok(ws.and_then(|w| w.branch.clone()).unwrap_or_default()),
                "base_ref" => Ok(ws.and_then(|w| w.base_ref.clone()).unwrap_or_default()),
                _ => Err(RenderError::UnknownPath(path.to_string())),
            }
        }
        ("parent", [field]) => {
            let p = ctx.parent.as_ref();
            match *field {
                "identifier" => Ok(p.map(|p| p.identifier.clone()).unwrap_or_default()),
                "title" => Ok(p.map(|p| p.title.clone()).unwrap_or_default()),
                "url" => Ok(p.and_then(|p| p.url.clone()).unwrap_or_default()),
                _ => Err(RenderError::UnknownPath(path.to_string())),
            }
        }
        ("children", []) => Ok(render_children(&ctx.children)),
        ("blockers", []) => Ok(render_blockers(&ctx.blockers)),
        ("acceptance_criteria", []) => Ok(render_acceptance(&ctx.acceptance_criteria)),
        _ => Err(RenderError::UnknownPath(path.to_string())),
    }
}

fn render_authority(a: &RoleAuthority) -> String {
    let mut flags: Vec<&'static str> = Vec::new();
    if a.can_decompose {
        flags.push("can_decompose");
    }
    if a.can_assign {
        flags.push("can_assign");
    }
    if a.can_request_qa {
        flags.push("can_request_qa");
    }
    if a.can_close_parent {
        flags.push("can_close_parent");
    }
    if a.can_file_blockers {
        flags.push("can_file_blockers");
    }
    if a.can_file_followups {
        flags.push("can_file_followups");
    }
    if a.required_for_done {
        flags.push("required_for_done");
    }
    flags.join(", ")
}

fn render_children(children: &[PromptChild]) -> String {
    children
        .iter()
        .map(|c| format!("- {}: {} [{}]", c.identifier, c.title, c.status))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_blockers(blockers: &[PromptBlocker]) -> String {
    blockers
        .iter()
        .map(|b| format!("- [{}] {}", b.severity, b.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_acceptance(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("- {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::RoleKind;

    fn issue() -> Issue {
        let mut i = Issue::minimal("id-1", "ENG-7", "Add fizz to buzz", "Todo");
        i.description = Some("steps:\n1. fizz\n2. buzz".into());
        i.branch_name = Some("feature/eng-7".into());
        i.url = Some("https://example.test/eng-7".into());
        i
    }

    #[test]
    fn for_issue_carries_every_issue_field_into_the_substitution_surface() {
        let ctx = PromptContext::for_issue(&issue());
        let tpl = "Issue {{identifier}}: {{title}}\nState: {{state}}\nBranch: {{branch_name}}\nURL: {{url}}\n{{description}}";
        let out = render(tpl, &ctx).unwrap();
        assert!(out.contains("Issue ENG-7: Add fizz to buzz"));
        assert!(out.contains("State: Todo"));
        assert!(out.contains("Branch: feature/eng-7"));
        assert!(out.contains("URL: https://example.test/eng-7"));
        assert!(out.contains("1. fizz"));
    }

    #[test]
    fn missing_optional_issue_fields_render_as_empty_strings() {
        let mut i = Issue::minimal("id-2", "ENG-8", "T", "Todo");
        i.description = None;
        i.branch_name = None;
        i.url = None;
        let ctx = PromptContext::for_issue(&i);
        let out = render("[{{description}}][{{branch_name}}][{{url}}]", &ctx).unwrap();
        assert_eq!(out, "[][][]");
    }

    #[test]
    fn unknown_top_level_placeholder_returns_error() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("hello {{nope}}", &ctx).unwrap_err();
        assert_eq!(err, RenderError::UnknownPath("nope".into()));
    }

    #[test]
    fn unknown_subfield_returns_error() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("hello {{role.bogus}}", &ctx).unwrap_err();
        assert_eq!(err, RenderError::UnknownPath("role.bogus".into()));
    }

    #[test]
    fn unclosed_placeholder_returns_malformed_error() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("hello {{identifier", &ctx).unwrap_err();
        assert!(matches!(err, RenderError::Malformed(_)));
    }

    #[test]
    fn empty_placeholder_returns_invalid_path_error() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("hello {{ }}", &ctx).unwrap_err();
        assert!(matches!(err, RenderError::InvalidPath(_)));
    }

    #[test]
    fn whitespace_around_path_is_trimmed() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render("[{{  identifier  }}]", &ctx).unwrap();
        assert_eq!(out, "[ENG-7]");
    }

    #[test]
    fn role_fields_render_when_role_is_present() {
        let role = RoleContext::from_kind("platform_lead", RoleKind::IntegrationOwner);
        let ctx = PromptContext::for_issue(&issue()).with_role(role);
        let out = render(
            "role={{role.name}} kind={{role.kind}} grants=[{{role.authority}}]",
            &ctx,
        )
        .unwrap();
        assert!(out.contains("role=platform_lead"));
        assert!(out.contains("kind=integration_owner"));
        assert!(
            out.contains("grants=[can_decompose, can_assign, can_request_qa, can_close_parent]"),
            "got: {out}"
        );
    }

    #[test]
    fn role_fields_render_as_empty_when_role_is_absent() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render(
            "role=[{{role.name}}] kind=[{{role.kind}}] grants=[{{role.authority}}]",
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "role=[] kind=[] grants=[]");
    }

    #[test]
    fn workspace_fields_render_when_workspace_is_present() {
        let ws = PromptWorkspace {
            path: PathBuf::from("/tmp/wt/eng-7"),
            strategy: "git_worktree".into(),
            branch: Some("symphony/eng-7".into()),
            base_ref: Some("main".into()),
        };
        let ctx = PromptContext::for_issue(&issue()).with_workspace(ws);
        let out = render(
            "ws={{workspace.path}} s={{workspace.strategy}} b={{workspace.branch}} base={{workspace.base_ref}}",
            &ctx,
        )
        .unwrap();
        assert_eq!(
            out,
            "ws=/tmp/wt/eng-7 s=git_worktree b=symphony/eng-7 base=main"
        );
    }

    #[test]
    fn workspace_fields_render_as_empty_when_workspace_is_absent() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render(
            "[{{workspace.path}}][{{workspace.strategy}}][{{workspace.branch}}][{{workspace.base_ref}}]",
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "[][][][]");
    }

    #[test]
    fn parent_fields_render_when_parent_is_present() {
        let ctx = PromptContext::for_issue(&issue()).with_parent(PromptParent {
            identifier: "ENG-1".into(),
            title: "Broad parent".into(),
            url: Some("https://example.test/eng-1".into()),
        });
        let out = render(
            "parent={{parent.identifier}}|{{parent.title}}|{{parent.url}}",
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "parent=ENG-1|Broad parent|https://example.test/eng-1");
    }

    #[test]
    fn children_render_as_bullet_list() {
        let ctx = PromptContext::for_issue(&issue()).with_children(vec![
            PromptChild {
                identifier: "ENG-8".into(),
                title: "Child A".into(),
                status: "Done".into(),
            },
            PromptChild {
                identifier: "ENG-9".into(),
                title: "Child B".into(),
                status: "In Progress".into(),
            },
        ]);
        let out = render("children:\n{{children}}", &ctx).unwrap();
        assert_eq!(
            out,
            "children:\n- ENG-8: Child A [Done]\n- ENG-9: Child B [In Progress]"
        );
    }

    #[test]
    fn empty_children_list_renders_as_empty_string() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render("children:[{{children}}]", &ctx).unwrap();
        assert_eq!(out, "children:[]");
    }

    #[test]
    fn blockers_render_with_severity_and_reason() {
        let ctx = PromptContext::for_issue(&issue()).with_blockers(vec![
            PromptBlocker {
                id: Some(BlockerId::new(1)),
                blocking_id: Some(WorkItemId::new(42)),
                reason: "schema not approved".into(),
                severity: BlockerSeverity::High,
            },
            PromptBlocker {
                id: None,
                blocking_id: None,
                reason: "vendor outage".into(),
                severity: BlockerSeverity::Medium,
            },
        ]);
        let out = render("blockers:\n{{blockers}}", &ctx).unwrap();
        assert_eq!(
            out,
            "blockers:\n- [high] schema not approved\n- [medium] vendor outage"
        );
    }

    #[test]
    fn acceptance_criteria_render_as_bullet_list() {
        let ctx = PromptContext::for_issue(&issue())
            .with_acceptance_criteria(vec!["tests pass".into(), "no clippy warnings".into()]);
        let out = render("ac:\n{{acceptance_criteria}}", &ctx).unwrap();
        assert_eq!(out, "ac:\n- tests pass\n- no clippy warnings");
    }

    #[test]
    fn output_schema_is_inserted_verbatim() {
        let schema = "{\"summary\": \"...\"}";
        let ctx = PromptContext::for_issue(&issue()).with_output_schema(schema);
        let out = render("emit:\n{{output_schema}}", &ctx).unwrap();
        assert_eq!(out, format!("emit:\n{schema}"));
    }

    #[test]
    fn output_schema_renders_as_empty_string_when_unset() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render("[{{output_schema}}]", &ctx).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn list_field_rejects_subpath() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("{{children.first}}", &ctx).unwrap_err();
        assert_eq!(err, RenderError::UnknownPath("children.first".into()));
    }

    #[test]
    fn scalar_field_rejects_subpath() {
        let ctx = PromptContext::for_issue(&issue());
        let err = render("{{title.upper}}", &ctx).unwrap_err();
        assert_eq!(err, RenderError::UnknownPath("title.upper".into()));
    }

    #[test]
    fn template_without_placeholders_is_returned_unchanged() {
        let ctx = PromptContext::for_issue(&issue());
        let out = render("plain text\nno braces", &ctx).unwrap();
        assert_eq!(out, "plain text\nno braces");
    }

    #[test]
    fn default_handoff_output_schema_lists_every_required_handoff_field() {
        // Sanity check: the schema we hand agents must mention every
        // SPEC §4.7 field name so the agent has a chance of producing a
        // valid Handoff. If the Handoff shape changes, this test forces
        // a deliberate update of the default schema.
        let schema = default_handoff_output_schema();
        for field in [
            "summary",
            "changed_files",
            "tests_run",
            "verification_evidence",
            "known_risks",
            "blockers_created",
            "followups_created_or_proposed",
            "branch_or_workspace",
            "ready_for",
        ] {
            assert!(
                schema.contains(field),
                "default schema missing field: {field}"
            );
        }
    }

    #[test]
    fn full_prompt_combines_every_section() {
        let role = RoleContext::from_kind("qa", RoleKind::QaGate);
        let ws = PromptWorkspace {
            path: PathBuf::from("/tmp/wt/eng-7"),
            strategy: "git_worktree".into(),
            branch: Some("symphony/eng-7".into()),
            base_ref: Some("main".into()),
        };
        let ctx = PromptContext::for_issue(&issue())
            .with_role(role)
            .with_workspace(ws)
            .with_parent(PromptParent {
                identifier: "ENG-1".into(),
                title: "Broad parent".into(),
                url: None,
            })
            .with_children(vec![PromptChild {
                identifier: "ENG-8".into(),
                title: "Child A".into(),
                status: "Done".into(),
            }])
            .with_blockers(vec![PromptBlocker {
                id: None,
                blocking_id: None,
                reason: "vendor outage".into(),
                severity: BlockerSeverity::Critical,
            }])
            .with_acceptance_criteria(vec!["tests pass".into()])
            .with_output_schema("SCHEMA");

        let tpl = "\
Issue {{identifier}} ({{state}}): {{title}}
Role: {{role.name}} ({{role.kind}})
Authority: {{role.authority}}
Workspace: {{workspace.path}} on {{workspace.branch}} from {{workspace.base_ref}}
Parent: {{parent.identifier}} - {{parent.title}}
Children:
{{children}}
Blockers:
{{blockers}}
Acceptance:
{{acceptance_criteria}}
Output schema:
{{output_schema}}
";
        let out = render(tpl, &ctx).unwrap();
        assert!(out.contains("Issue ENG-7 (Todo): Add fizz to buzz"));
        assert!(out.contains("Role: qa (qa_gate)"));
        assert!(
            out.contains("Authority: can_file_blockers, can_file_followups, required_for_done")
        );
        assert!(out.contains("Workspace: /tmp/wt/eng-7 on symphony/eng-7 from main"));
        assert!(out.contains("Parent: ENG-1 - Broad parent"));
        assert!(out.contains("- ENG-8: Child A [Done]"));
        assert!(out.contains("- [critical] vendor outage"));
        assert!(out.contains("- tests pass"));
        assert!(out.contains("Output schema:\nSCHEMA"));
    }
}
