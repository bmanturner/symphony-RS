//! Implementation of the `symphony debug` namespace.
//!
//! These commands are read-only operator surfaces for v4 prompt work:
//! render the prompt or generated role catalog that would be handed to
//! an agent, but stop before any workspace claim or agent launch.

use serde::Serialize;
use symphony_config::{
    ConfigValidationError, LayeredLoadError, LayeredLoader, LoadedRoleInstructionPack,
    LoadedWorkflow, RoleCatalog, RoleCatalogBuilder, RoleKind,
};
use symphony_core::prompt::{
    PromptAssembly, PromptSectionKind, PromptSectionMetadata, PromptWorkspace,
};
use symphony_core::tracker::Issue;

use crate::cli::{DebugCatalogArgs, DebugCommand, DebugPromptArgs};
use crate::prompt_builders::{
    build_decomposition_prompt, build_qa_prompt, build_specialist_prompt,
};

#[derive(Debug)]
pub enum DebugOutcome {
    PromptOk(PromptPreview),
    CatalogOk(CatalogPreview),
    LoadFailed(LayeredLoadError),
    Invalid(Box<LoadedWorkflow>, ConfigValidationError),
    UnknownRole(Box<LoadedWorkflow>, String),
    PromptFailed(Box<LoadedWorkflow>, anyhow::Error),
}

impl DebugOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            DebugOutcome::PromptOk(_) | DebugOutcome::CatalogOk(_) => 0,
            DebugOutcome::LoadFailed(_) => 1,
            DebugOutcome::Invalid(_, _) => 2,
            DebugOutcome::UnknownRole(_, _) => 3,
            DebugOutcome::PromptFailed(_, _) => 4,
        }
    }
}

#[derive(Debug)]
pub struct PromptPreview {
    pub workflow_path: String,
    pub role: String,
    pub json: bool,
    pub assembly: PromptAssembly,
}

#[derive(Debug)]
pub struct CatalogPreview {
    pub workflow_path: String,
    pub role: String,
    pub json: bool,
    pub catalog: RoleCatalog,
}

pub fn run(command: &DebugCommand) -> DebugOutcome {
    match command {
        DebugCommand::Prompt(args) => run_prompt(args),
        DebugCommand::Catalog(args) => run_catalog(args),
    }
}

fn run_prompt(args: &DebugPromptArgs) -> DebugOutcome {
    let loaded = match load_validated(&args.path) {
        Ok(loaded) => loaded,
        Err(outcome) => return outcome,
    };
    let role = match loaded.config.roles.get(&args.role) {
        Some(role) => role,
        None => return DebugOutcome::UnknownRole(Box::new(loaded), args.role.clone()),
    };
    let issue = Issue::minimal("preview", &args.identifier, &args.title, &args.state);
    let result = match role.kind {
        RoleKind::IntegrationOwner => build_decomposition_prompt(
            &loaded.config,
            &loaded.instruction_packs,
            &loaded.prompt_template,
            &args.role,
            &issue,
        ),
        RoleKind::QaGate => build_qa_prompt(
            &loaded.prompt_template,
            &loaded.instruction_packs,
            &args.role,
            &issue,
            &[],
            Vec::new(),
            Vec::new(),
            None,
        ),
        RoleKind::Specialist | RoleKind::Reviewer | RoleKind::Operator | RoleKind::Custom => {
            build_specialist_prompt(
                &loaded.prompt_template,
                &loaded.instruction_packs,
                &args.role,
                None,
                &issue,
                Some(PromptWorkspace {
                    path: std::env::temp_dir().join("symphony-preview"),
                    strategy: "preview".into(),
                    branch: Some(format!("preview/{}", args.identifier.to_ascii_lowercase())),
                    base_ref: Some(loaded.config.branching.default_base.clone()),
                }),
                Vec::new(),
                Vec::new(),
            )
        }
    };

    let mut assembly = match result {
        Ok(assembly) => assembly,
        Err(err) => return DebugOutcome::PromptFailed(Box::new(loaded), err),
    };
    if !args.unsafe_unredacted {
        redact_instruction_sections(
            &mut assembly,
            loaded.instruction_packs.roles.get(&args.role),
        );
    }

    DebugOutcome::PromptOk(PromptPreview {
        workflow_path: loaded.source_path.display().to_string(),
        role: args.role.clone(),
        json: args.json,
        assembly,
    })
}

fn run_catalog(args: &DebugCatalogArgs) -> DebugOutcome {
    let loaded = match load_validated(&args.path) {
        Ok(loaded) => loaded,
        Err(outcome) => return outcome,
    };
    if !loaded.config.roles.contains_key(&args.role) {
        return DebugOutcome::UnknownRole(Box::new(loaded), args.role.clone());
    }
    let catalog = RoleCatalogBuilder {
        workflow: &loaded.config,
        instruction_packs: &loaded.instruction_packs,
        current_role: &args.role,
    }
    .build();
    DebugOutcome::CatalogOk(CatalogPreview {
        workflow_path: loaded.source_path.display().to_string(),
        role: args.role.clone(),
        json: args.json,
        catalog,
    })
}

fn load_validated(path: &std::path::Path) -> Result<LoadedWorkflow, DebugOutcome> {
    let loaded = LayeredLoader::from_path(path).map_err(DebugOutcome::LoadFailed)?;
    if let Err(err) = loaded.config.validate() {
        return Err(DebugOutcome::Invalid(Box::new(loaded), err));
    }
    Ok(loaded)
}

fn redact_instruction_sections(
    assembly: &mut PromptAssembly,
    pack: Option<&LoadedRoleInstructionPack>,
) {
    let Some(pack) = pack else {
        return;
    };
    for section in &mut assembly.sections {
        match section.kind {
            PromptSectionKind::RolePrompt => {
                if let Some(role_prompt) = &pack.role_prompt {
                    section.content = role_prompt.redacted_content();
                }
            }
            PromptSectionKind::RoleSoul => {
                if let Some(soul) = &pack.soul {
                    section.content = soul.redacted_content();
                }
            }
            _ => {}
        }
    }
}

pub fn render(outcome: &DebugOutcome) -> i32 {
    match outcome {
        DebugOutcome::PromptOk(preview) => render_prompt_preview(preview),
        DebugOutcome::CatalogOk(preview) => render_catalog_preview(preview),
        DebugOutcome::LoadFailed(err) => eprintln!("error: failed to load workflow: {err}"),
        DebugOutcome::Invalid(loaded, err) => eprintln!(
            "error: workflow at {} is semantically invalid: {err}",
            loaded.source_path.display()
        ),
        DebugOutcome::UnknownRole(loaded, role) => eprintln!(
            "error: role `{role}` is not configured in {}",
            loaded.source_path.display()
        ),
        DebugOutcome::PromptFailed(loaded, err) => eprintln!(
            "error: failed to render prompt for {}: {err:#}",
            loaded.source_path.display()
        ),
    }
    outcome.exit_code()
}

fn render_prompt_preview(preview: &PromptPreview) {
    if preview.json {
        let doc = PromptPreviewJson {
            workflow_path: &preview.workflow_path,
            role: &preview.role,
            rendered_prompt: preview.assembly.render(),
            provenance: metadata_json(preview.assembly.section_metadata()),
        };
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
    } else {
        println!("{}", preview.assembly.render());
        render_provenance_footer(preview.assembly.section_metadata());
    }
}

fn render_catalog_preview(preview: &CatalogPreview) {
    if preview.json {
        let doc = CatalogPreviewJson {
            workflow_path: &preview.workflow_path,
            current_role: &preview.role,
            total_entries: preview.catalog.source_summary.total_entries,
            structured_entries: preview.catalog.source_summary.structured_entries,
            fallback_entries: preview.catalog.source_summary.fallback_entries,
            entries: preview
                .catalog
                .entries
                .iter()
                .map(|entry| CatalogEntryJson {
                    name: entry.name.clone(),
                    kind: format!("{:?}", entry.kind),
                    agent_profile: entry.agent_profile.clone(),
                    owns: entry.owns.clone(),
                    does_not_own: entry.does_not_own.clone(),
                    requires: entry.requires.clone(),
                    handoff_expectations: entry.handoff_expectations.clone(),
                    routing_labels_any: entry.routing_hints.labels_any.clone(),
                    instruction_summary: entry.instruction_pack_summary.role_prompt_summary.clone(),
                    has_soul: entry.instruction_pack_summary.has_soul,
                })
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&doc).unwrap());
    } else {
        println!("{}", preview.catalog.render_for_prompt());
    }
}

fn render_provenance_footer(metadata: Vec<PromptSectionMetadata>) {
    let with_source: Vec<_> = metadata
        .into_iter()
        .filter(|item| item.source.is_some() || item.content_hash.is_some())
        .collect();
    if with_source.is_empty() {
        return;
    }
    println!("\n## Prompt Provenance");
    for item in with_source {
        println!(
            "- kind={:?}, source={}, content_hash={}",
            item.kind,
            item.source.as_deref().unwrap_or(""),
            item.content_hash.as_deref().unwrap_or("")
        );
    }
}

fn metadata_json(metadata: Vec<PromptSectionMetadata>) -> Vec<PromptSectionMetadataJson> {
    metadata
        .into_iter()
        .map(|item| PromptSectionMetadataJson {
            kind: format!("{:?}", item.kind),
            source: item.source,
            content_hash: item.content_hash,
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct PromptPreviewJson<'a> {
    workflow_path: &'a str,
    role: &'a str,
    rendered_prompt: String,
    provenance: Vec<PromptSectionMetadataJson>,
}

#[derive(Debug, Serialize)]
struct PromptSectionMetadataJson {
    kind: String,
    source: Option<String>,
    content_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct CatalogPreviewJson<'a> {
    workflow_path: &'a str,
    current_role: &'a str,
    total_entries: usize,
    structured_entries: usize,
    fallback_entries: usize,
    entries: Vec<CatalogEntryJson>,
}

#[derive(Debug, Serialize)]
struct CatalogEntryJson {
    name: String,
    kind: String,
    agent_profile: Option<String>,
    owns: Vec<String>,
    does_not_own: Vec<String>,
    requires: Vec<String>,
    handoff_expectations: Vec<String>,
    routing_labels_any: Vec<String>,
    instruction_summary: Option<String>,
    has_soul: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_config::{
        InstructionKind, InstructionPackBundle, InstructionSource, LoadedRoleInstruction,
    };

    fn instruction(content: &str, kind: InstructionKind) -> LoadedRoleInstruction {
        LoadedRoleInstruction {
            source: InstructionSource {
                role: "backend".into(),
                kind,
                path: match kind {
                    InstructionKind::RolePrompt => ".symphony/roles/backend/AGENTS.md",
                    InstructionKind::Soul => ".symphony/roles/backend/SOUL.md",
                }
                .into(),
                content_hash: "hash123".into(),
                modified_unix_ms: Some(1),
                loaded_at_unix_ms: 2,
            },
            content: content.into(),
        }
    }

    #[test]
    fn prompt_preview_redacts_instruction_sections_but_keeps_provenance() {
        let mut packs = InstructionPackBundle::default();
        packs.roles.insert(
            "backend".into(),
            LoadedRoleInstructionPack {
                role_prompt: Some(instruction(
                    "api_key: super-secret",
                    InstructionKind::RolePrompt,
                )),
                soul: Some(instruction("No secrets here", InstructionKind::Soul)),
            },
        );
        let mut assembly = PromptAssembly::new(vec![
            symphony_core::prompt::PromptSection::new(
                PromptSectionKind::RolePrompt,
                "api_key: super-secret",
            )
            .with_provenance(symphony_core::prompt::PromptSectionProvenance {
                source: ".symphony/roles/backend/AGENTS.md".into(),
                content_hash: Some("hash123".into()),
            }),
            symphony_core::prompt::PromptSection::new(PromptSectionKind::IssueContext, "safe"),
        ]);

        redact_instruction_sections(&mut assembly, packs.roles.get("backend"));
        let rendered = assembly.render();

        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("[REDACTED]"));
        let metadata = assembly.section_metadata();
        assert_eq!(
            metadata[0].source.as_deref(),
            Some(".symphony/roles/backend/AGENTS.md")
        );
        assert_eq!(metadata[0].content_hash.as_deref(), Some("hash123"));
    }
}
