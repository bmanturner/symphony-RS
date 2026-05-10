//! Layered configuration loader: defaults → environment → `WORKFLOW.md`.
//!
//! Where [`WorkflowLoader`](crate::loader::WorkflowLoader) only knows how to split a file into
//! `{front_matter, body}`, this module composes the *full* configuration
//! the orchestrator needs at startup. It uses [`figment`] to merge three
//! providers in fixed precedence:
//!
//! 1. **Defaults** — `WorkflowConfig::default()`, the SPEC §5.3 baseline.
//! 2. **Environment** — every `SYMPHONY_*` variable, with `__` denoting
//!    nesting (`SYMPHONY_POLLING__INTERVAL_MS=100` →
//!    `polling.interval_ms = 100`). Values are coerced from strings during
//!    typed extraction.
//! 3. **WORKFLOW.md front matter** — wins over both. This matches SPEC
//!    §5.4: "Environment variables do not globally override YAML values."
//!    An operator can set tracker credentials in env without forcing every
//!    YAML to override them, but anything explicitly written into
//!    `WORKFLOW.md` is authoritative.
//!
//! The body of `WORKFLOW.md` (the prompt template) is unaffected by this
//! merge — figment only governs the typed front-matter view.
//!
//! ## Why a separate loader?
//!
//! Keeping [`WorkflowLoader`](crate::loader::WorkflowLoader) free of figment lets us test the on-disk
//! format in pure unit tests with no env interference. [`LayeredLoader`]
//! is what production callers (the CLI, the orchestrator composition
//! root) reach for; tests that don't care about layering can keep using
//! the simpler loader.

use std::fs;
use std::path::{Path, PathBuf};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Yaml},
};
use gray_matter::Matter;
use gray_matter::engine::YAML;

use crate::config::WorkflowConfig;
use crate::loader::{LoadedWorkflow, WorkflowLoadError, load_instruction_packs};

/// Errors that can surface from a layered load.
///
/// Wraps [`WorkflowLoadError`] so the existing on-disk failure modes
/// (missing file, non-map front matter, malformed YAML) flow through
/// unchanged, and adds a figment-merge variant for cases where the
/// merged value tree fails to satisfy [`WorkflowConfig`]'s typed schema
/// (e.g. an out-of-range env override, an unknown nested key from any
/// source).
#[derive(Debug, thiserror::Error)]
pub enum LayeredLoadError {
    /// On-disk read or parse step (the [`WorkflowLoader`](crate::loader::WorkflowLoader) surface).
    #[error(transparent)]
    Workflow(#[from] WorkflowLoadError),

    /// Figment failed to merge or extract the typed config.
    ///
    /// This catches both schema violations (an env var like
    /// `SYMPHONY_AGENT__KIND=banana` that doesn't match the
    /// [`AgentKind`](crate::AgentKind) enum) and structural mismatches
    /// (e.g. an env var name that nests under a non-section key). Figment
    /// already produces user-friendly diagnostics for both; we just
    /// preserve them. Boxed because `figment::Error` is large enough that
    /// clippy flags an unboxed `Result` as `result_large_err`.
    #[error("layered configuration is invalid: {0}")]
    Merge(#[from] Box<figment::Error>),
}

/// Loader for the full layered configuration.
///
/// Stateless: every call rebuilds the figment from scratch, so changes
/// to environment variables or the file on disk are picked up on the
/// next call. Hot-reload orchestration (debouncing, swap-on-success) is
/// the caller's responsibility — same contract as [`WorkflowLoader`](crate::loader::WorkflowLoader).
pub struct LayeredLoader;

impl LayeredLoader {
    /// Read `path`, layer env on top of defaults, then layer the file's
    /// front matter on top of that. Returns the merged
    /// [`LoadedWorkflow`].
    ///
    /// Errors are returned, never panicked. The body is *not* subject to
    /// layering — it is taken verbatim (and trimmed) from the file.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<LoadedWorkflow, LayeredLoadError> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path).map_err(|source| WorkflowLoadError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_str_with_path(&raw, path)
    }

    /// In-memory variant of [`from_path`](Self::from_path).
    ///
    /// Exists for the same reason [`WorkflowLoader::from_str_with_path`](crate::loader::WorkflowLoader::from_str_with_path)
    /// does: tests should be able to drive the loader without touching
    /// the filesystem, and a future stdin / HTTP upload code path can
    /// reuse the same logic.
    pub fn from_str_with_path(raw: &str, path: &Path) -> Result<LoadedWorkflow, LayeredLoadError> {
        // Step 1: split front matter from body. We intentionally do not
        // route through `WorkflowLoader::from_str_with_path` here because
        // that path eagerly materialises a typed `WorkflowConfig` from
        // YAML alone — which would erase the "absent in YAML" signal
        // figment needs to let env vars take effect.
        let parsed = Matter::<YAML>::new().parse(raw);
        let prompt_template = parsed.content.trim().to_string();
        let front_matter = parsed.matter;

        // Step 2: pre-validate the front matter shape. We accept either
        // an empty block (no `---` delimiters at all, or an empty YAML
        // body between delimiters) or a YAML mapping. Anything else
        // (top-level scalar, sequence) is the same hard error
        // `WorkflowLoader` surfaces.
        if !front_matter.trim().is_empty() {
            let probe: serde_yaml::Value =
                serde_yaml::from_str(&front_matter).map_err(|source| {
                    LayeredLoadError::Workflow(WorkflowLoadError::Yaml {
                        path: path.to_path_buf(),
                        source,
                    })
                })?;
            match &probe {
                serde_yaml::Value::Mapping(_) | serde_yaml::Value::Null => {}
                _ => {
                    return Err(LayeredLoadError::Workflow(WorkflowLoadError::NotAMap {
                        path: path.to_path_buf(),
                    }));
                }
            }
        }

        // Step 3: build the figment. Order matters — later providers win.
        // We start from the typed default (so figment knows the full
        // shape and types), let env override defaults, then let the YAML
        // front matter override env. The figment crate itself handles
        // string-to-typed coercion at extract time.
        let figment = Figment::new()
            .merge(Serialized::defaults(WorkflowConfig::default()))
            .merge(Env::prefixed("SYMPHONY_").split("__"))
            .merge(Yaml::string(&front_matter));

        let config: WorkflowConfig = figment.extract().map_err(Box::new)?;
        let instruction_packs = load_instruction_packs(&config, path)?;

        Ok(LoadedWorkflow {
            source_path: path.to_path_buf(),
            config,
            prompt_template,
            instruction_packs,
        })
    }
}

/// Convenience accessor for the [`LayeredLoader`]'s source path. Kept on
/// the loader rather than as a free function so future relative-path
/// resolution (SPEC §5.3.3) has a place to grow.
impl LayeredLoader {
    /// Resolve a workflow path relative to the current working directory
    /// if it is relative, otherwise leave it absolute. Public so the CLI
    /// and tests can share the same canonicalisation rule.
    pub fn canonicalise<P: AsRef<Path>>(path: P) -> PathBuf {
        let path = path.as_ref();
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
// `figment::Jail::expect_with` requires its closure to return `Result<(),
// figment::Error>`. The error variant is ~200B, which trips clippy's
// `result_large_err` lint at every test site. Boxing the closure return
// type isn't an option (the API is fixed), so we silence the lint locally.
#[allow(clippy::result_large_err)]
mod tests {
    use std::path::{Path, PathBuf};

    use figment::Jail;

    use super::*;
    use crate::config::{AgentKind, TrackerKind};

    fn fake_path() -> PathBuf {
        PathBuf::from("/virtual/WORKFLOW.md")
    }

    /// Baseline: no env, valid YAML, layered loader returns the same
    /// typed config the simple loader would.
    #[test]
    fn yaml_only_load_matches_simple_loader() {
        Jail::expect_with(|_jail| {
            let input = r#"---
tracker:
  kind: github
  repository: foglet-io/rust-symphony
agent:
  kind: claude
  max_turns: 5
---
prompt body
"#;
            let loaded = LayeredLoader::from_str_with_path(input, &fake_path()).unwrap();
            assert_eq!(loaded.config.tracker.kind, TrackerKind::Github);
            assert_eq!(
                loaded.config.tracker.repository.as_deref(),
                Some("foglet-io/rust-symphony")
            );
            assert_eq!(loaded.config.agent.kind, AgentKind::Claude);
            assert_eq!(loaded.config.agent.max_turns, 5);
            assert_eq!(loaded.prompt_template, "prompt body");
            Ok(())
        });
    }

    /// Env vars fill in fields that the YAML did not specify. Default
    /// values still apply for fields neither source mentioned.
    #[test]
    fn env_overrides_defaults_when_yaml_silent() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_POLLING__INTERVAL_MS", "1500");
            jail.set_env("SYMPHONY_AGENT__MAX_TURNS", "9");

            let input = "---\ntracker:\n  project_slug: ENG\n---\nbody\n";
            let loaded = LayeredLoader::from_str_with_path(input, &fake_path()).unwrap();
            assert_eq!(loaded.config.polling.interval_ms, 1_500);
            assert_eq!(loaded.config.agent.max_turns, 9);
            // Untouched defaults survive.
            assert_eq!(loaded.config.agent.max_concurrent_agents, 10);
            assert_eq!(loaded.config.tracker.project_slug.as_deref(), Some("ENG"));
            Ok(())
        });
    }

    /// SPEC §5.4: "Environment variables do not globally override YAML
    /// values." If the YAML names a key, that wins.
    #[test]
    fn yaml_wins_over_env_when_both_present() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_POLLING__INTERVAL_MS", "1500");
            let input = "---\npolling:\n  interval_ms: 250\n---\nbody\n";
            let loaded = LayeredLoader::from_str_with_path(input, &fake_path()).unwrap();
            assert_eq!(loaded.config.polling.interval_ms, 250);
            Ok(())
        });
    }

    /// Env-only configuration: YAML front matter is empty, env carries
    /// every override. Verifies that string→typed coercion (here, env
    /// strings → `AgentKind` enum + `u32`) works through figment.
    #[test]
    fn env_only_when_no_front_matter() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_AGENT__KIND", "claude");
            jail.set_env("SYMPHONY_AGENT__MAX_TURNS", "3");
            jail.set_env("SYMPHONY_TRACKER__KIND", "github");
            jail.set_env("SYMPHONY_TRACKER__REPOSITORY", "foglet-io/rust-symphony");

            let loaded =
                LayeredLoader::from_str_with_path("just a body, no fm\n", &fake_path()).unwrap();
            assert_eq!(loaded.config.agent.kind, AgentKind::Claude);
            assert_eq!(loaded.config.agent.max_turns, 3);
            assert_eq!(loaded.config.tracker.kind, TrackerKind::Github);
            assert_eq!(
                loaded.config.tracker.repository.as_deref(),
                Some("foglet-io/rust-symphony")
            );
            assert_eq!(loaded.prompt_template, "just a body, no fm");
            Ok(())
        });
    }

    /// An invalid env value (out-of-enum string for `AgentKind`) should
    /// surface as a `Merge` error, not a panic and not a silent default.
    #[test]
    fn malformed_env_value_is_rejected() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_AGENT__KIND", "banana");
            let err = LayeredLoader::from_str_with_path(
                "---\ntracker:\n  project_slug: ENG\n---\nbody\n",
                &fake_path(),
            )
            .unwrap_err();
            assert!(
                matches!(err, LayeredLoadError::Merge(_)),
                "expected Merge error, got {err:?}"
            );
            Ok(())
        });
    }

    /// Malformed YAML front matter (an unterminated quoted string —
    /// genuinely unparseable, not just type-incompatible) should
    /// surface as a `Workflow(Yaml)` error from our pre-probe before
    /// figment ever sees it.
    #[test]
    fn malformed_yaml_front_matter_is_rejected() {
        Jail::expect_with(|_jail| {
            let input = "---\ntracker:\n  kind: \"unterminated\n---\nbody\n";
            let err = LayeredLoader::from_str_with_path(input, &fake_path()).unwrap_err();
            assert!(
                matches!(
                    err,
                    LayeredLoadError::Workflow(WorkflowLoadError::Yaml { .. })
                ),
                "expected Workflow(Yaml), got {err:?}"
            );
            Ok(())
        });
    }

    /// Non-map front matter (e.g. a top-level YAML list) is the same
    /// hard error the simple loader raises.
    #[test]
    fn non_map_front_matter_is_rejected() {
        Jail::expect_with(|_jail| {
            let input = "---\n- a\n- b\n---\nbody\n";
            let err = LayeredLoader::from_str_with_path(input, &fake_path()).unwrap_err();
            assert!(
                matches!(
                    err,
                    LayeredLoadError::Workflow(WorkflowLoadError::NotAMap { .. })
                ),
                "expected Workflow(NotAMap), got {err:?}"
            );
            Ok(())
        });
    }

    /// Missing file path is a `Workflow(Read)` — same as the simple
    /// loader, just routed through the layered error type.
    #[test]
    fn missing_file_returns_workflow_read_error() {
        Jail::expect_with(|_jail| {
            let err =
                LayeredLoader::from_path("/definitely/does/not/exist/WORKFLOW.md").unwrap_err();
            assert!(
                matches!(
                    err,
                    LayeredLoadError::Workflow(WorkflowLoadError::Read { .. })
                ),
                "expected Workflow(Read), got {err:?}"
            );
            Ok(())
        });
    }

    /// Round-trip through disk: write a file, read it back through the
    /// layered loader, observe the merged result.
    #[test]
    fn from_path_round_trips_through_disk_with_env_layering() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_POLLING__INTERVAL_MS", "200");

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("WORKFLOW.md");
            std::fs::write(
                &path,
                "---\ntracker:\n  kind: linear\n  project_slug: ENG\n---\nhello\n",
            )
            .unwrap();
            let loaded = LayeredLoader::from_path(&path).unwrap();
            // Env wins over default.
            assert_eq!(loaded.config.polling.interval_ms, 200);
            // YAML supplies tracker fields.
            assert_eq!(loaded.config.tracker.project_slug.as_deref(), Some("ENG"));
            assert_eq!(loaded.prompt_template, "hello");
            assert_eq!(loaded.source_path, path);
            Ok(())
        });
    }

    /// Unrelated `SYMPHONY_*` variables that don't map to a known key
    /// should fail the merge — typos must not silently default.
    #[test]
    fn unknown_env_key_is_rejected() {
        Jail::expect_with(|jail| {
            jail.set_env("SYMPHONY_POLLING__INTERVA_MS", "100");
            let err = LayeredLoader::from_str_with_path(
                "---\ntracker:\n  project_slug: ENG\n---\nbody\n",
                &fake_path(),
            )
            .unwrap_err();
            assert!(
                matches!(err, LayeredLoadError::Merge(_)),
                "expected Merge error for typo'd env key, got {err:?}"
            );
            Ok(())
        });
    }

    /// `canonicalise` leaves absolute paths alone and resolves relative
    /// paths against the cwd. This matches the SPEC §5.3.3 contract for
    /// future workspace-root resolution.
    #[test]
    fn canonicalise_handles_absolute_and_relative() {
        let abs = PathBuf::from("/tmp/x/WORKFLOW.md");
        assert_eq!(LayeredLoader::canonicalise(&abs), abs);

        let rel = LayeredLoader::canonicalise(PathBuf::from("WORKFLOW.md"));
        assert!(rel.is_absolute() || rel == Path::new("WORKFLOW.md"));
    }
}
