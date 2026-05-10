//! `WORKFLOW.md` loader: split YAML front matter from the prompt body.
//!
//! This module implements SPEC §5.2 ("File Format") for the in-repo
//! `WORKFLOW.md` contract:
//!
//! 1. Split `---`-delimited YAML front matter from the trailing Markdown
//!    body using [`gray_matter`].
//! 2. Re-parse the raw front-matter string through `serde_yaml` into our
//!    typed [`WorkflowConfig`]. We deliberately **do not** go through
//!    `gray_matter`'s `Pod` deserializer — it does not honour
//!    `#[serde(deny_unknown_fields)]`, and we want typos like
//!    `polling.interva_ms` to be a hard error per SPEC §5.3.
//! 3. Reject non-map YAML (e.g. a top-level YAML list) with the
//!    `workflow_front_matter_not_a_map` error called out in SPEC §10.1.
//! 4. Trim the prompt body before returning it.
//!
//! Layered configuration (defaults → env → front matter) lives in a
//! sibling module; this loader only owns the on-disk format.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use gray_matter::Matter;
use gray_matter::engine::YAML;

use crate::config::WorkflowConfig;

/// Parsed `WORKFLOW.md` payload — the typed front matter plus the
/// trimmed Markdown prompt body.
///
/// Mirrors the SPEC §5.1 "Workflow Loader" return shape. The body is
/// already trimmed; downstream code should not re-trim or re-strip
/// delimiters.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedWorkflow {
    /// Path the workflow was loaded from. Retained so workspace-relative
    /// path resolution (SPEC §5.3.3) can use the containing directory.
    pub source_path: PathBuf,

    /// Typed front matter. Defaults filled in for any omitted keys.
    pub config: WorkflowConfig,

    /// Markdown body after the closing `---`, trimmed. May be empty if
    /// the file contains only front matter (legal per SPEC §5.2; the
    /// runtime will surface a separate "empty prompt" error at dispatch
    /// preflight, not here).
    pub prompt_template: String,
}

/// Errors raised while reading and parsing `WORKFLOW.md`.
///
/// Variants align with the typed-error catalogue in SPEC §10.1 so the
/// CLI and HTTP surfaces can map them onto stable error codes without
/// stringly-typed comparisons.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowLoadError {
    /// The file does not exist or is unreadable. Maps to
    /// `missing_workflow_file` in SPEC §10.1.
    #[error("workflow file at {path} could not be read: {source}")]
    Read {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// Front matter parsed as YAML but the root value was a scalar or
    /// sequence rather than a mapping. Maps to
    /// `workflow_front_matter_not_a_map` (SPEC §10.1).
    #[error("workflow front matter at {path} must be a YAML mapping")]
    NotAMap {
        /// Path that was attempted.
        path: PathBuf,
    },

    /// Front matter is malformed YAML or violates the typed schema —
    /// unknown nested keys, wrong value types, etc. Maps to
    /// `invalid_workflow_front_matter` (SPEC §10.1).
    #[error("workflow front matter at {path} is invalid: {source}")]
    Yaml {
        /// Path that was attempted.
        path: PathBuf,
        /// Underlying YAML error.
        #[source]
        source: serde_yaml::Error,
    },

    /// A configured role instruction path escaped the workflow root or
    /// did not resolve to a readable file.
    #[error(
        "role `{role}` {kind} instruction path `{path}` is invalid at {workflow_path}: {reason}"
    )]
    InstructionPath {
        /// Workflow file being loaded.
        workflow_path: PathBuf,
        /// Role whose instruction path failed validation.
        role: String,
        /// `role_prompt` or `soul`.
        kind: &'static str,
        /// Configured path value.
        path: PathBuf,
        /// Operator-facing reason.
        reason: String,
    },
}

/// Loader entry point for `WORKFLOW.md`.
///
/// Stateless on purpose — every call re-reads from disk. Hot-reload
/// orchestration (debouncing, swap-on-success) is the caller's
/// responsibility and lives behind the `notify` watcher in the
/// orchestrator.
pub struct WorkflowLoader;

impl WorkflowLoader {
    /// Read `path` and return its parsed `{config, prompt_template}`.
    ///
    /// Errors are returned, never panicked. Callers that need a default
    /// when the file is absent (e.g. tests) should match on
    /// [`WorkflowLoadError::Read`] explicitly rather than swallowing all
    /// errors.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<LoadedWorkflow, WorkflowLoadError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| WorkflowLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_str_with_path(&raw, path)
    }

    /// Parse an in-memory `WORKFLOW.md` payload. The `path` is purely
    /// informational — used to populate `source_path` and to attach a
    /// helpful location to error messages.
    ///
    /// This split exists so tests can drive the parser without touching
    /// the filesystem and so a future stdin / HTTP upload code path can
    /// reuse the same logic.
    pub fn from_str_with_path(raw: &str, path: &Path) -> Result<LoadedWorkflow, WorkflowLoadError> {
        let parsed = Matter::<YAML>::new().parse(raw);

        // gray_matter populates `matter` only when a leading `---`
        // delimiter is present. SPEC §5.2 says: absent front matter →
        // empty config (defaults) and the whole file is prompt body.
        let config = if parsed.matter.trim().is_empty() {
            WorkflowConfig::default()
        } else {
            parse_front_matter(&parsed.matter, path)?
        };
        validate_role_instruction_paths(&config, path)?;

        // SPEC §5.2: prompt body is trimmed before use. We trim the
        // *content* (post-front-matter) string from gray_matter, which
        // already excludes the delimiters and the YAML block.
        let prompt_template = parsed.content.trim().to_string();

        Ok(LoadedWorkflow {
            source_path: path.to_path_buf(),
            config,
            prompt_template,
        })
    }
}

fn validate_role_instruction_paths(
    config: &WorkflowConfig,
    workflow_path: &Path,
) -> Result<(), WorkflowLoadError> {
    let root = workflow_path.parent().unwrap_or_else(|| Path::new("."));
    for (role_name, role) in &config.roles {
        for (kind, maybe_path) in [
            ("role_prompt", role.instructions.role_prompt.as_ref()),
            ("soul", role.instructions.soul.as_ref()),
        ] {
            let Some(path) = maybe_path else {
                continue;
            };
            if path.is_absolute()
                || path.components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(WorkflowLoadError::InstructionPath {
                    workflow_path: workflow_path.to_path_buf(),
                    role: role_name.clone(),
                    kind,
                    path: path.clone(),
                    reason: "path must stay inside the workflow root".to_string(),
                });
            }
            let resolved = root.join(path);
            match fs::metadata(&resolved) {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => {
                    return Err(WorkflowLoadError::InstructionPath {
                        workflow_path: workflow_path.to_path_buf(),
                        role: role_name.clone(),
                        kind,
                        path: path.clone(),
                        reason: "path exists but is not a file".to_string(),
                    });
                }
                Err(source) => {
                    return Err(WorkflowLoadError::InstructionPath {
                        workflow_path: workflow_path.to_path_buf(),
                        role: role_name.clone(),
                        kind,
                        path: path.clone(),
                        reason: format!("file could not be read: {source}"),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Re-parse the raw YAML front-matter string into our typed config.
///
/// We do this in two passes — first into `serde_yaml::Value` to detect
/// non-map roots cleanly, then into [`WorkflowConfig`] so that
/// `deny_unknown_fields` and field-level type errors surface as
/// `WorkflowLoadError::Yaml` with their natural `serde_yaml` location
/// information intact.
fn parse_front_matter(raw: &str, path: &Path) -> Result<WorkflowConfig, WorkflowLoadError> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|source| WorkflowLoadError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;

    // SPEC §5.2: "YAML front matter MUST decode to a map/object;
    // non-map YAML is an error." A null root (front matter present but
    // empty body, e.g. just `---\n---`) we treat as the empty map for
    // the same reason gray_matter treats absent front matter as empty.
    match &value {
        serde_yaml::Value::Mapping(_) => {}
        serde_yaml::Value::Null => return Ok(WorkflowConfig::default()),
        _ => {
            return Err(WorkflowLoadError::NotAMap {
                path: path.to_path_buf(),
            });
        }
    }

    serde_yaml::from_value(value).map_err(|source| WorkflowLoadError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{AgentKind, TrackerKind};

    fn fake_path() -> PathBuf {
        PathBuf::from("/virtual/WORKFLOW.md")
    }

    #[test]
    fn parses_front_matter_and_trimmed_body() {
        let input = r#"---
tracker:
  kind: github
  repository: foglet-io/rust-symphony
agent:
  kind: claude
  max_turns: 5
---

You are an autonomous coding agent.

Implement the issue described below.
"#;
        let loaded = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap();
        assert_eq!(loaded.config.tracker.kind, TrackerKind::Github);
        assert_eq!(
            loaded.config.tracker.repository.as_deref(),
            Some("foglet-io/rust-symphony")
        );
        assert_eq!(loaded.config.agent.kind, AgentKind::Claude);
        assert_eq!(loaded.config.agent.max_turns, 5);
        // Body must be trimmed — no leading blank line, no trailing newline.
        assert_eq!(
            loaded.prompt_template,
            "You are an autonomous coding agent.\n\nImplement the issue described below."
        );
        assert_eq!(loaded.source_path, fake_path());
    }

    #[test]
    fn missing_front_matter_yields_default_config() {
        let input = "Just a body. No front matter.\n";
        let loaded = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap();
        assert_eq!(loaded.config, WorkflowConfig::default());
        assert_eq!(loaded.prompt_template, "Just a body. No front matter.");
    }

    #[test]
    fn empty_front_matter_block_yields_default_config() {
        // Edge case: delimiters present but the YAML block is empty.
        let input = "---\n---\nbody\n";
        let loaded = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap();
        assert_eq!(loaded.config, WorkflowConfig::default());
        assert_eq!(loaded.prompt_template, "body");
    }

    #[test]
    fn front_matter_only_file_has_empty_prompt() {
        let input = "---\ntracker:\n  kind: linear\n  project_slug: ENG\n---\n";
        let loaded = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap();
        assert_eq!(loaded.config.tracker.kind, TrackerKind::Linear);
        assert_eq!(loaded.config.tracker.project_slug.as_deref(), Some("ENG"));
        assert_eq!(loaded.prompt_template, "");
    }

    #[test]
    fn non_map_front_matter_is_rejected() {
        // A top-level YAML list is legal YAML but illegal per SPEC §5.2.
        let input = "---\n- a\n- b\n---\nbody\n";
        let err = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap_err();
        match err {
            WorkflowLoadError::NotAMap { path } => assert_eq!(path, fake_path()),
            other => panic!("expected NotAMap, got {other:?}"),
        }
    }

    #[test]
    fn malformed_yaml_is_rejected() {
        // Broken YAML — unmatched indentation under a sequence.
        let input = "---\ntracker:\n  kind: linear\n   bad_indent\n---\nbody\n";
        let err = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap_err();
        assert!(matches!(err, WorkflowLoadError::Yaml { .. }));
    }

    #[test]
    fn unknown_nested_key_propagates_as_yaml_error() {
        // Forward-compat applies to *top-level* keys only. Typos inside
        // a known section must surface as a hard error so an operator
        // does not silently fall back to defaults.
        let input = "---\npolling:\n  interva_ms: 100\n---\nbody\n";
        let err = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap_err();
        match err {
            WorkflowLoadError::Yaml { source, .. } => {
                assert!(
                    source.to_string().contains("interva_ms")
                        || source.to_string().contains("unknown field"),
                    "unexpected yaml error: {source}"
                );
            }
            other => panic!("expected Yaml error, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_returns_read_error() {
        let err = WorkflowLoader::from_path("/definitely/does/not/exist/WORKFLOW.md").unwrap_err();
        assert!(matches!(err, WorkflowLoadError::Read { .. }));
    }

    #[test]
    fn from_path_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        fs::write(
            &path,
            "---\ntracker:\n  kind: linear\n  project_slug: ENG\n---\nhello\n",
        )
        .unwrap();
        let loaded = WorkflowLoader::from_path(&path).unwrap();
        assert_eq!(loaded.config.tracker.project_slug.as_deref(), Some("ENG"));
        assert_eq!(loaded.prompt_template, "hello");
        assert_eq!(loaded.source_path, path);
    }

    #[test]
    fn role_instruction_paths_must_stay_inside_workflow_root() {
        let input = r#"---
tracker:
  kind: linear
  project_slug: ENG
roles:
  backend:
    kind: specialist
    instructions:
      role_prompt: ../outside/AGENTS.md
---
body
"#;
        let err = WorkflowLoader::from_str_with_path(input, &fake_path()).unwrap_err();
        match err {
            WorkflowLoadError::InstructionPath {
                role, kind, reason, ..
            } => {
                assert_eq!(role, "backend");
                assert_eq!(kind, "role_prompt");
                assert!(reason.contains("inside the workflow root"));
            }
            other => panic!("expected InstructionPath, got {other:?}"),
        }
    }

    #[test]
    fn role_instruction_paths_must_exist_and_be_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        let input = r#"---
tracker:
  kind: linear
  project_slug: ENG
roles:
  backend:
    kind: specialist
    instructions:
      soul: .symphony/roles/backend/SOUL.md
---
body
"#;
        fs::write(&path, input).unwrap();

        let err = WorkflowLoader::from_path(&path).unwrap_err();
        match err {
            WorkflowLoadError::InstructionPath {
                role, kind, reason, ..
            } => {
                assert_eq!(role, "backend");
                assert_eq!(kind, "soul");
                assert!(reason.contains("could not be read"));
            }
            other => panic!("expected InstructionPath, got {other:?}"),
        }
    }

    #[test]
    fn role_instruction_paths_resolve_relative_to_workflow_root() {
        let dir = tempfile::tempdir().unwrap();
        let role_dir = dir.path().join(".symphony/roles/backend");
        fs::create_dir_all(&role_dir).unwrap();
        fs::write(role_dir.join("AGENTS.md"), "backend instructions").unwrap();
        fs::write(role_dir.join("SOUL.md"), "backend soul").unwrap();
        let path = dir.path().join("WORKFLOW.md");
        let input = r#"---
tracker:
  kind: linear
  project_slug: ENG
roles:
  backend:
    kind: specialist
    instructions:
      role_prompt: .symphony/roles/backend/AGENTS.md
      soul: .symphony/roles/backend/SOUL.md
---
body
"#;
        fs::write(&path, input).unwrap();

        let loaded = WorkflowLoader::from_path(&path).unwrap();
        let backend = loaded.config.roles.get("backend").unwrap();
        assert_eq!(
            backend.instructions.role_prompt.as_deref(),
            Some(Path::new(".symphony/roles/backend/AGENTS.md"))
        );
        assert_eq!(
            backend.instructions.soul.as_deref(),
            Some(Path::new(".symphony/roles/backend/SOUL.md"))
        );
    }
}
