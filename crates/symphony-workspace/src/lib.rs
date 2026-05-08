//! Per-issue workspace lifecycle for Symphony-RS.
//!
//! This crate implements SPEC §9 (Workspace Management and Safety): every
//! issue the orchestrator dispatches to an agent gets a dedicated directory
//! under a configured root, where the agent process is executed with that
//! directory as `cwd`.
//!
//! ## What this iteration delivers
//!
//! The trait surface and the [`LocalFsWorkspace`] adapter that maps an
//! issue identifier to a directory under `workspace.root`. The adapter is
//! deliberately *naive* on its first commit — it joins identifier to root
//! without sanitization or containment checks. Those land in follow-up
//! checklist items so each safety property gets a dedicated commit and a
//! dedicated test surface (sanitization rule, containment invariant,
//! lifecycle hooks).
//!
//! ## Why a trait at all
//!
//! The orchestrator never calls `std::fs` directly. It depends on
//! [`WorkspaceManager`] so that:
//!
//! 1. Tests can substitute an in-memory or temp-dir-only fake.
//! 2. Future deployments can swap in a remote workspace (e.g. one backed
//!    by a sandbox VM) without touching the orchestrator.
//! 3. Each safety invariant from SPEC §9.5 has exactly one place where
//!    it is enforced: inside the manager. The orchestrator only sees the
//!    return value of [`WorkspaceManager::ensure`], which is by
//!    construction a path inside the workspace root.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs;

/// Replace every character that is not in `[A-Za-z0-9._-]` with `_`.
///
/// This is the SPEC §4.2 / §9.5 "filesystem-safe identifier" rule. The
/// orchestrator hands the manager whatever identifier the tracker produced
/// (Linear keys like `ENG-42`, GitHub identifiers like `octo/repo#7`,
/// hypothetical Jira keys with slashes) and the manager has to land that
/// somewhere on disk. Sanitisation lives here, in one named function, so
/// every adapter shares the same rule and the property tests cover it
/// exactly once.
///
/// Replacement is char-by-char (not byte-by-byte): a single multi-byte
/// codepoint becomes a single `_`, preserving the visible-length of the
/// caller's identifier. Idempotent — `sanitize(sanitize(x)) == sanitize(x)`
/// because every produced char is already in the allowlist.
///
/// Containment of the resulting path inside the workspace root is the
/// *next* checklist item; this function is intentionally narrow. The dot
/// is in the allowlist (so `0.1.2` survives unchanged), which means
/// inputs like `..`, `.`, or the empty string pass through here and must
/// be rejected by the containment check that lands next.
pub fn sanitize_identifier(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// Errors a [`WorkspaceManager`] can report.
///
/// Mirrors the structure of `TrackerError` in `symphony-core`: a small,
/// orchestrator-actionable enum, not a leak of every underlying I/O
/// failure mode. Adapters convert their native errors (a `std::io::Error`,
/// a hook subprocess failure) into the closest variant here.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The configured `workspace.root` cannot be used: missing parent
    /// directory, permission denied, or — once containment lands — an
    /// identifier that escapes the root. Surfaced loudly because the
    /// orchestrator cannot make progress without a valid workspace.
    #[error("workspace root invalid: {0}")]
    InvalidRoot(String),

    /// I/O error while creating, querying, or removing a workspace
    /// directory. Transient by default; the orchestrator typically logs
    /// and retries on the next tick.
    #[error("workspace I/O failure: {0}")]
    Io(String),

    /// A configured lifecycle hook (`after_create`, `before_run`, …)
    /// failed or timed out. SPEC §9.4 distinguishes fatal vs. ignorable
    /// hook failures; the manager surfaces both as this variant and the
    /// orchestrator decides how to react based on which hook fired.
    #[error("workspace hook failed: {0}")]
    Hook(String),
}

/// Convenience alias matching the project-wide convention.
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

/// The handle [`WorkspaceManager::ensure`] returns to the orchestrator.
///
/// Carries the absolute path the agent should run in plus a `created_now`
/// flag. SPEC §9.2 requires `created_now` to drive the `after_create`
/// hook firing decision: it must be true *only* when this call is the one
/// that materialised the directory, never on subsequent reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Absolute path to the per-issue workspace directory.
    pub path: PathBuf,
    /// True iff this call created the directory (vs. found it existing).
    pub created_now: bool,
}

/// Abstract per-issue workspace lifecycle.
///
/// The orchestrator calls [`Self::ensure`] before launching an agent
/// session and [`Self::release`] when the issue reaches a terminal state.
/// Implementations are responsible for SPEC §9.5's safety invariants —
/// the orchestrator trusts whatever path comes back to be containment-
/// safe and ready to run a subprocess in.
///
/// `Send + Sync` so the orchestrator can store the manager as
/// `Arc<dyn WorkspaceManager>` and share it across poll-loop tasks.
#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    /// Ensure a workspace exists for `identifier` and return its handle.
    ///
    /// Idempotent: calling twice with the same identifier returns the
    /// same path; only the first call sets `created_now = true`.
    async fn ensure(&self, identifier: &str) -> WorkspaceResult<Workspace>;

    /// Remove the workspace for `identifier` if it exists.
    ///
    /// Used by SPEC §8.6 startup terminal cleanup and §16 release paths.
    /// Missing directories are not an error — the operation is a "make
    /// it not exist" assertion, not a strict delete.
    async fn release(&self, identifier: &str) -> WorkspaceResult<()>;
}

/// On-disk workspace manager backed by a single root directory.
///
/// The default and currently only adapter. Per-issue workspaces are
/// children of `root`. This first revision performs no sanitization or
/// containment checking — both land in the next checklist items so each
/// invariant gets a dedicated test surface and ADR. Until then, callers
/// are expected to pass identifiers that are already safe (e.g. those
/// produced by the GitHub or Linear adapters, which are bounded to
/// `[A-Za-z0-9-]`).
#[derive(Debug, Clone)]
pub struct LocalFsWorkspace {
    root: PathBuf,
}

impl LocalFsWorkspace {
    /// Construct a manager rooted at `root`. The directory must exist
    /// before this constructor returns successfully — symphony does not
    /// create the workspace root itself, because the operator's choice
    /// of root is a deployment concern (mount point, volume, etc.) and
    /// silently creating it would mask configuration mistakes.
    pub fn new(root: impl AsRef<Path>) -> WorkspaceResult<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(WorkspaceError::InvalidRoot(format!(
                "workspace root {} does not exist or is not a directory",
                root.display()
            )));
        }
        // Canonicalize so that downstream containment checks (next
        // checklist item) compare absolute paths, and so symbolic links
        // in the root cannot smuggle issue paths outside it.
        let canonical = std::fs::canonicalize(root)
            .map_err(|e| WorkspaceError::InvalidRoot(format!("canonicalize: {e}")))?;
        Ok(Self { root: canonical })
    }

    /// Return the absolute, canonicalized root.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl WorkspaceManager for LocalFsWorkspace {
    async fn ensure(&self, identifier: &str) -> WorkspaceResult<Workspace> {
        // Sanitise per SPEC §4.2 before joining. Containment validation
        // (path must canonicalise inside `self.root`, no `..` smuggling)
        // is the next checklist item.
        let safe = sanitize_identifier(identifier);
        let path = self.root.join(&safe);

        // `try_exists` distinguishes "does not exist" from "I cannot
        // tell" (e.g. permission denied on the parent). The latter is an
        // I/O failure we want to surface, not paper over.
        let existed = fs::try_exists(&path)
            .await
            .map_err(|e| WorkspaceError::Io(format!("stat {}: {e}", path.display())))?;

        if !existed {
            fs::create_dir_all(&path)
                .await
                .map_err(|e| WorkspaceError::Io(format!("mkdir {}: {e}", path.display())))?;
        }

        Ok(Workspace {
            path,
            created_now: !existed,
        })
    }

    async fn release(&self, identifier: &str) -> WorkspaceResult<()> {
        // Apply the same sanitisation as `ensure` so a release call uses
        // the same on-disk path the ensure call materialised.
        let path = self.root.join(sanitize_identifier(identifier));
        match fs::remove_dir_all(&path).await {
            Ok(()) => Ok(()),
            // Missing is fine — release is an assertion of absence, not a
            // strict delete. Anything else propagates as an I/O failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(WorkspaceError::Io(format!(
                "rm -rf {}: {e}",
                path.display()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Constructing a manager at a non-existent root must fail loudly:
    /// silently creating the root would mask deployment-config bugs.
    #[tokio::test]
    async fn new_rejects_missing_root() {
        let tmp = TempDir::new().unwrap();
        let bogus = tmp.path().join("does-not-exist");
        let err = LocalFsWorkspace::new(&bogus).unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// `ensure` creates the directory the first time and reports
    /// `created_now = true`; the directory must exist after the call.
    #[tokio::test]
    async fn ensure_creates_first_time() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let ws = mgr.ensure("ENG-42").await.unwrap();

        assert!(ws.created_now, "first ensure should report created_now");
        assert!(ws.path.is_dir());
        assert!(ws.path.starts_with(mgr.root()));
    }

    /// SPEC §9.2 explicitly requires reuse: a second `ensure` for the
    /// same identifier returns the same path with `created_now = false`.
    /// The `after_create` hook firing decision depends on this.
    #[tokio::test]
    async fn ensure_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let first = mgr.ensure("ENG-42").await.unwrap();
        let second = mgr.ensure("ENG-42").await.unwrap();

        assert_eq!(first.path, second.path);
        assert!(first.created_now);
        assert!(!second.created_now, "reuse must not report created_now");
    }

    /// `release` removes the directory if present. Used by SPEC §8.6
    /// startup terminal cleanup.
    #[tokio::test]
    async fn release_removes_existing_workspace() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let ws = mgr.ensure("ENG-42").await.unwrap();
        assert!(ws.path.is_dir());

        mgr.release("ENG-42").await.unwrap();
        assert!(!ws.path.exists());
    }

    /// `release` on an unknown identifier is a no-op, not an error.
    /// SPEC §8.6 says terminal cleanup runs over a *list* of identifiers
    /// and we don't want one stale entry to abort the whole sweep.
    #[tokio::test]
    async fn release_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        mgr.release("never-existed").await.unwrap();
    }

    /// Identifiers built only from the allowlist are returned verbatim:
    /// the rule must not mangle inputs the trackers already produce
    /// (`ENG-42`, `gh-org_repo-7`, `0.1.2`).
    #[test]
    fn sanitize_passes_allowlisted_chars_through() {
        for id in [
            "ENG-42",
            "gh-org_repo-7",
            "0.1.2",
            "ABC.def_GHI-123",
            "_",
            ".",
            "-",
        ] {
            assert_eq!(sanitize_identifier(id), id, "input: {id:?}");
        }
    }

    /// Every disallowed character — slashes, spaces, NULs, glob meta-
    /// characters, control bytes — collapses to `_`. This is the table
    /// of cases the property tests would otherwise generate by random
    /// search; pinning them as a named test makes regressions obvious.
    #[test]
    fn sanitize_replaces_disallowed_chars_with_underscore() {
        let cases = [
            ("a/b", "a_b"),
            // `.` is allowlisted (so `0.1.2` survives), therefore `..`
            // passes through verbatim. Rejecting `..` is the next
            // checklist item's containment check, not this rule's job.
            ("..", ".."),
            ("a b", "a_b"),
            ("a\0b", "a_b"),
            ("a\nb", "a_b"),
            ("a:b", "a_b"),
            ("a*b?", "a_b_"),
            ("foo bar/baz", "foo_bar_baz"),
            // Non-ASCII codepoints are replaced char-by-char (one
            // codepoint → one `_`), not byte-by-byte.
            ("π", "_"),
            ("café", "caf_"),
        ];
        for (input, want) in cases {
            assert_eq!(
                sanitize_identifier(input),
                want,
                "input: {input:?} → expected {want:?}",
            );
        }
    }

    /// `ensure` must safely accept hostile-looking identifiers: the on-
    /// disk path stays a *child* of `root`. The next checklist item adds
    /// a stronger containment check (canonicalisation against the root);
    /// at this layer the guarantee is "every disallowed char is `_`,
    /// therefore no path separator survives".
    #[tokio::test]
    async fn ensure_sanitises_path_traversal_attempts() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let ws = mgr.ensure("../etc/passwd").await.unwrap();

        // Sanitisation maps each disallowed char (`.`, `.`, `/`, …) — but
        // `.` is *allowed*, so only the `/` chars become `_`. The result
        // is `..` becoming literal `..` then `_etc_passwd`. We assert two
        // properties separately:
        //   1. the produced path component contains no path separator,
        //   2. the resulting path is a direct child of `root` (one extra
        //      component, no `..` segments).
        let component = ws.path.file_name().unwrap().to_str().unwrap();
        assert_eq!(component, ".._etc_passwd");
        assert_eq!(
            ws.path.parent().unwrap(),
            mgr.root(),
            "sanitised id must land directly under root, not nested",
        );
        assert!(ws.path.starts_with(mgr.root()));
    }

    /// `release` must use the same sanitisation as `ensure` — otherwise
    /// the orchestrator could materialise a workspace and then fail to
    /// remove it because it computed a different on-disk name.
    #[tokio::test]
    async fn release_uses_same_sanitisation_as_ensure() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let ws = mgr.ensure("ORG/repo#7").await.unwrap();
        assert!(ws.path.is_dir());
        assert_eq!(ws.path.file_name().unwrap(), "ORG_repo_7");

        mgr.release("ORG/repo#7").await.unwrap();
        assert!(!ws.path.exists(), "release must remove what ensure created");
    }

    // ----- property tests --------------------------------------------------
    //
    // We use `proptest` for invariants of the pure sanitiser. These cover
    // the corners random fuzzing surfaces that an enumerated table cannot
    // (combining marks, surrogate-adjacent codepoints, mixed-script
    // identifiers, runs of disallowed bytes).

    use proptest::prelude::*;

    proptest! {
        /// Output may only contain characters from the allowlist.
        #[test]
        fn prop_output_only_allowlisted_chars(input in any::<String>()) {
            let out = sanitize_identifier(&input);
            for c in out.chars() {
                prop_assert!(
                    matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-'),
                    "char {:?} not in allowlist (input: {:?}, output: {:?})",
                    c, input, out,
                );
            }
        }

        /// Sanitisation is idempotent: a second pass changes nothing.
        /// This is what makes it safe to apply at every entry point —
        /// composing `ensure(sanitize(id))` with the manager's own
        /// internal sanitisation is a no-op.
        #[test]
        fn prop_sanitize_is_idempotent(input in any::<String>()) {
            let once = sanitize_identifier(&input);
            let twice = sanitize_identifier(&once);
            prop_assert_eq!(once, twice);
        }

        /// Char count is preserved (1:1 char replacement). This lets us
        /// reason about visible-length bounds when the orchestrator logs
        /// or displays the sanitised identifier.
        #[test]
        fn prop_sanitize_preserves_char_count(input in any::<String>()) {
            let out = sanitize_identifier(&input);
            prop_assert_eq!(input.chars().count(), out.chars().count());
        }

        /// Inputs already in the allowlist are returned verbatim. The
        /// orchestrator depends on this so well-formed tracker IDs
        /// (`ENG-42`, `0.1.2`) never get mangled by passing through a
        /// safety helper.
        #[test]
        fn prop_allowlisted_inputs_are_unchanged(
            input in proptest::string::string_regex("[A-Za-z0-9._-]{0,32}").unwrap()
        ) {
            prop_assert_eq!(input.clone(), sanitize_identifier(&input));
        }

        /// No path-separator survives, regardless of platform: the
        /// sanitised name must be a *single* path component. This is
        /// the workspace-safety property that lets the next checklist
        /// item layer a containment check on top — the path can grow
        /// at most by one component beyond `root`.
        #[test]
        fn prop_output_has_no_path_separator(input in any::<String>()) {
            let out = sanitize_identifier(&input);
            prop_assert!(!out.contains('/'));
            prop_assert!(!out.contains('\\'));
        }
    }

    /// The orchestrator stores managers as `Arc<dyn WorkspaceManager>`,
    /// so the trait must be object-safe and `Send + Sync`. This compile-
    /// time witness fails to build if either property regresses.
    #[tokio::test]
    async fn local_fs_workspace_is_dyn_compatible() {
        let tmp = TempDir::new().unwrap();
        let mgr: std::sync::Arc<dyn WorkspaceManager> =
            std::sync::Arc::new(LocalFsWorkspace::new(tmp.path()).unwrap());

        let ws = mgr.ensure("ENG-1").await.unwrap();
        assert!(ws.created_now);
    }
}
