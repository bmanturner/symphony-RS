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
        // Naive join for now. Sanitization (replace [^A-Za-z0-9._-] with
        // `_`) and containment validation (path must remain rooted at
        // `self.root`) are the next two checklist items.
        let path = self.root.join(identifier);

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
        let path = self.root.join(identifier);
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
