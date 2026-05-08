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
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Default per-hook timeout (SPEC §9.4: `60_000 ms`). Hooks that exceed it
/// are killed and reported as a [`WorkspaceError::Hook`]. Wrapped as a
/// `const` so the default lands in one place and `serde` can default the
/// deserialised field to it.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 60_000;

fn default_hook_timeout_ms() -> u64 {
    DEFAULT_HOOK_TIMEOUT_MS
}

/// Optional shell scripts that fire at well-defined points in the
/// workspace lifecycle. SPEC §9.4 names four of them:
///
/// | Hook            | Fires on                       | Failure semantics             |
/// |-----------------|--------------------------------|-------------------------------|
/// | `after_create`  | the `ensure` that created dir  | fatal — `ensure` returns Err  |
/// | `before_run`    | orchestrator pre-spawn         | fatal — caller skips the run  |
/// | `after_run`     | orchestrator post-spawn        | logged + ignored              |
/// | `before_remove` | the `release` before rmdir     | logged + ignored              |
///
/// Each script is executed via `sh -lc <script>` with the workspace
/// directory as `cwd`. The `Option<String>` shape — rather than
/// `Vec<String>` of arg-vectors — matches the SPEC's intent that hooks
/// are arbitrary shell snippets the operator chooses, not a structured
/// command we want to interpret.
///
/// Default is "no hooks configured": every field `None`, timeout 60s.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHooks {
    /// Fires once, the first time `ensure` materialises the directory.
    /// Failure aborts `ensure` and the just-created directory is removed
    /// so a retry sees a clean slate (matches the SPEC's "fatal to
    /// workspace creation" wording — a half-initialised workspace would
    /// silently break on the next ensure).
    #[serde(default)]
    pub after_create: Option<String>,

    /// Fires before every agent dispatch. Returns a fatal error to the
    /// caller (the orchestrator), which is expected to keep the issue
    /// claimed but not start the turn.
    #[serde(default)]
    pub before_run: Option<String>,

    /// Fires after every agent dispatch. Failures are logged and
    /// swallowed — this hook is for observability/cleanup, not control
    /// flow. It cannot block or fail a run that already happened.
    #[serde(default)]
    pub after_run: Option<String>,

    /// Fires before `release` removes the workspace. Failures are logged
    /// and swallowed; the rmdir proceeds. Useful for archival snapshots
    /// that must not block teardown.
    #[serde(default)]
    pub before_remove: Option<String>,

    /// Per-hook timeout in milliseconds; defaults to
    /// [`DEFAULT_HOOK_TIMEOUT_MS`]. Applied uniformly to all four hooks
    /// because SPEC §9.4 only specifies a single `hooks.timeout_ms`.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for WorkspaceHooks {
    /// Empty hooks with the SPEC-default timeout. Hand-rolled rather
    /// than `#[derive(Default)]` because deriving would zero `timeout_ms`,
    /// which would silently make every hook time out instantly the
    /// moment a caller used `..Default::default()` to set just one
    /// field.
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_ms: DEFAULT_HOOK_TIMEOUT_MS,
        }
    }
}

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
    /// same path; only the first call sets `created_now = true`. When
    /// `created_now` is true, implementations also fire the configured
    /// `after_create` hook before returning; a hook failure is fatal and
    /// the partially-created directory is rolled back.
    async fn ensure(&self, identifier: &str) -> WorkspaceResult<Workspace>;

    /// Fire the `before_run` hook against `identifier`'s workspace.
    ///
    /// Called by the orchestrator immediately before spawning an agent
    /// process. A hook error is propagated and the orchestrator must
    /// abort the dispatch (SPEC §9.4: fatal to the run attempt). When no
    /// `before_run` hook is configured this is a cheap no-op.
    async fn before_run(&self, identifier: &str) -> WorkspaceResult<()>;

    /// Fire the `after_run` hook against `identifier`'s workspace.
    ///
    /// Called by the orchestrator after an agent process exits, whether
    /// it succeeded or failed. Per SPEC §9.4 this hook's failures are
    /// logged and ignored — it returns `()`, not `Result`, to make the
    /// "cannot fail the run" guarantee structural rather than a comment.
    async fn after_run(&self, identifier: &str);

    /// Remove the workspace for `identifier` if it exists.
    ///
    /// Used by SPEC §8.6 startup terminal cleanup and §16 release paths.
    /// Missing directories are not an error — the operation is a "make
    /// it not exist" assertion, not a strict delete. Fires the
    /// `before_remove` hook before rmdir; that hook's failures are
    /// logged and ignored (SPEC §9.4).
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
    hooks: WorkspaceHooks,
}

impl LocalFsWorkspace {
    /// Construct a manager rooted at `root` with no hooks configured.
    /// The directory must exist before this constructor returns
    /// successfully — symphony does not create the workspace root
    /// itself, because the operator's choice of root is a deployment
    /// concern (mount point, volume, etc.) and silently creating it
    /// would mask configuration mistakes.
    pub fn new(root: impl AsRef<Path>) -> WorkspaceResult<Self> {
        Self::with_hooks(root, WorkspaceHooks::default())
    }

    /// Construct a manager with explicit hooks. Same root requirements
    /// as [`Self::new`]. The hooks are stored verbatim; their scripts
    /// are not validated until they fire (SPEC §9.4 keeps the contract
    /// "execute via shell at the right moment", not "parse and lint at
    /// startup").
    pub fn with_hooks(root: impl AsRef<Path>, hooks: WorkspaceHooks) -> WorkspaceResult<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(WorkspaceError::InvalidRoot(format!(
                "workspace root {} does not exist or is not a directory",
                root.display()
            )));
        }
        // Canonicalize so that downstream containment checks compare
        // absolute paths, and so symbolic links in the root cannot
        // smuggle issue paths outside it.
        let canonical = std::fs::canonicalize(root)
            .map_err(|e| WorkspaceError::InvalidRoot(format!("canonicalize: {e}")))?;
        Ok(Self {
            root: canonical,
            hooks,
        })
    }

    /// Return the absolute, canonicalized root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sanitize `identifier` and validate the result is a usable single
    /// path component. Sanitisation already maps every character outside
    /// `[A-Za-z0-9._-]` to `_`, so Unicode look-alike attacks (fullwidth
    /// solidus `／`, modifier letters, RTL overrides, etc.) collapse to
    /// `_` before we ever look at them. What survives sanitisation are
    /// the *purely-allowlisted* hostile inputs:
    ///
    /// - the empty string (no on-disk name at all),
    /// - `"."` (the workspace root itself),
    /// - `".."` (the parent of the workspace root — SPEC §9.5 Invariant 2
    ///   explicitly forbids this).
    ///
    /// Each is rejected with [`WorkspaceError::InvalidRoot`] because they
    /// indicate a misconfigured tracker or a truly malicious identifier;
    /// either way the orchestrator cannot proceed.
    fn safe_component(identifier: &str) -> WorkspaceResult<String> {
        let safe = sanitize_identifier(identifier);
        match safe.as_str() {
            "" => Err(WorkspaceError::InvalidRoot(
                "identifier sanitised to empty string".into(),
            )),
            "." | ".." => Err(WorkspaceError::InvalidRoot(format!(
                "identifier sanitised to reserved component {safe:?}"
            ))),
            _ => Ok(safe),
        }
    }

    /// Belt-and-suspenders containment check (SPEC §9.5 Invariant 2).
    ///
    /// `safe_component` already prevents the only inputs that could
    /// escape `root` via path semantics, but a symlink planted under
    /// `root` between manager construction and `ensure` could still
    /// redirect the materialised directory elsewhere. Canonicalising
    /// *after* creation and asserting the canonical path is a child of
    /// `self.root` (which is itself canonical, set in [`Self::new`])
    /// catches that race. On mismatch we surface an [`InvalidRoot`]
    /// rather than silently falling through.
    /// Run a single hook script with the workspace as `cwd`, applying the
    /// configured timeout. Returns `Ok(())` if the script exits 0 within
    /// the deadline; otherwise a [`WorkspaceError::Hook`] whose message
    /// includes the hook name, exit status (or "timeout"), and a tail of
    /// the captured stderr to help the operator diagnose without
    /// shelling in.
    ///
    /// `name` is the SPEC hook name (`"after_create"` etc.) — used only
    /// for log lines and the error message; control flow is identical
    /// for every hook.
    async fn run_hook(&self, name: &'static str, script: &str, cwd: &Path) -> WorkspaceResult<()> {
        debug!(hook = name, cwd = %cwd.display(), "running workspace hook");
        // SPEC §9.4: `sh -lc <script>` is a conforming POSIX default.
        // We do not opt into `bash` so the binary stays portable to
        // minimal containers that ship only `sh`.
        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(script).current_dir(cwd);
        // Capture stdout/stderr so a failure can be reported with
        // context. Inheriting them would interleave hook output with
        // the daemon's tracing logs — confusing for operators tailing
        // journald.
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let deadline = Duration::from_millis(self.hooks.timeout_ms);
        let output = match timeout(deadline, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(WorkspaceError::Hook(format!("{name}: spawn failed: {e}")));
            }
            Err(_) => {
                return Err(WorkspaceError::Hook(format!(
                    "{name}: timed out after {}ms",
                    self.hooks.timeout_ms
                )));
            }
        };

        if !output.status.success() {
            // `String::from_utf8_lossy` so a non-UTF-8 stderr (rare but
            // possible on some shells/locales) still produces a usable
            // diagnostic instead of a separate error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(WorkspaceError::Hook(format!(
                "{name}: exited {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(())
    }

    fn assert_contained(&self, path: &Path) -> WorkspaceResult<()> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|e| WorkspaceError::Io(format!("canonicalize {}: {e}", path.display())))?;
        if !canonical.starts_with(&self.root) {
            return Err(WorkspaceError::InvalidRoot(format!(
                "workspace path {} escaped root {}",
                canonical.display(),
                self.root.display()
            )));
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceManager for LocalFsWorkspace {
    async fn ensure(&self, identifier: &str) -> WorkspaceResult<Workspace> {
        // Sanitise per SPEC §4.2, then reject the three "all-allowlisted
        // but still hostile" survivors (empty, `.`, `..`) before joining.
        let safe = Self::safe_component(identifier)?;
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

        // SPEC §9.5 Invariant 2: canonicalise after creation and confirm
        // we landed inside the canonical root. Catches a symlink planted
        // between construction and ensure.
        self.assert_contained(&path)?;

        // Fire `after_create` only on the call that materialised the
        // directory. SPEC §9.4 makes a failure here fatal; we also
        // remove the just-created directory so a retry sees a fresh
        // slate (otherwise the dir exists but never ran `after_create`,
        // and the next ensure would skip the hook entirely).
        if !existed
            && let Some(script) = &self.hooks.after_create
            && let Err(e) = self.run_hook("after_create", script, &path).await
        {
            let _ = fs::remove_dir_all(&path).await;
            return Err(e);
        }

        Ok(Workspace {
            path,
            created_now: !existed,
        })
    }

    async fn before_run(&self, identifier: &str) -> WorkspaceResult<()> {
        let Some(script) = &self.hooks.before_run else {
            return Ok(());
        };
        let safe = Self::safe_component(identifier)?;
        let path = self.root.join(safe);
        // We do not re-create the directory here: `before_run` runs
        // *after* `ensure`, and asking the orchestrator to call it on a
        // missing workspace is a programming error worth surfacing.
        if !fs::try_exists(&path)
            .await
            .map_err(|e| WorkspaceError::Io(format!("stat {}: {e}", path.display())))?
        {
            return Err(WorkspaceError::Hook(format!(
                "before_run: workspace {} does not exist (call ensure first)",
                path.display()
            )));
        }
        self.run_hook("before_run", script, &path).await
    }

    async fn after_run(&self, identifier: &str) {
        let Some(script) = &self.hooks.after_run else {
            return;
        };
        // Validation failure here is a bug, but `after_run` is best-
        // effort — log and return rather than panic. The orchestrator
        // does not get to know.
        let safe = match Self::safe_component(identifier) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "after_run skipped: invalid identifier");
                return;
            }
        };
        let path = self.root.join(safe);
        if !path.is_dir() {
            warn!(
                path = %path.display(),
                "after_run skipped: workspace does not exist",
            );
            return;
        }
        if let Err(e) = self.run_hook("after_run", script, &path).await {
            // SPEC §9.4: failures logged and ignored.
            warn!(error = %e, "after_run hook failed (ignored)");
        }
    }

    async fn release(&self, identifier: &str) -> WorkspaceResult<()> {
        // Apply the same sanitisation+validation as `ensure` so a release
        // call computes the same on-disk path. Reserved components are
        // rejected here too — there is nothing legitimate to release at
        // `root/..` or `root/.`.
        let safe = Self::safe_component(identifier)?;
        let path = self.root.join(safe);

        // Fire `before_remove` only when the directory exists — running
        // a teardown hook against a non-existent workspace would just
        // produce confusing log noise.
        if let Some(script) = &self.hooks.before_remove
            && path.is_dir()
            && let Err(e) = self.run_hook("before_remove", script, &path).await
        {
            // SPEC §9.4: failures logged and ignored, rmdir proceeds.
            warn!(error = %e, "before_remove hook failed (ignored)");
        }

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

    /// `..` is in the sanitiser's allowlist (because `.` is — `0.1.2`
    /// has to survive intact), so the containment layer must catch it
    /// explicitly. The orchestrator must never end up with a workspace
    /// path that resolves to the parent of `root`.
    #[tokio::test]
    async fn ensure_rejects_dot_dot_identifier() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let err = mgr.ensure("..").await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::InvalidRoot(_)),
            "expected InvalidRoot, got {err:?}"
        );
    }

    /// `.` sanitises to itself and would point at `root` itself if
    /// joined naively. Reject so that an agent never runs *in* the
    /// workspace root (which is shared across issues).
    #[tokio::test]
    async fn ensure_rejects_dot_identifier() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let err = mgr.ensure(".").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// Empty input has no on-disk representation and almost certainly
    /// indicates a bug upstream (a tracker returning a blank identifier).
    /// Surface it loudly rather than materialising the workspace root.
    #[tokio::test]
    async fn ensure_rejects_empty_identifier() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let err = mgr.ensure("").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// Unicode look-alike attacks rely on a non-ASCII codepoint that
    /// *renders* like a separator (fullwidth solidus `／`, division
    /// slash `∕`) tricking a naive joiner into spawning a path component
    /// elsewhere. The sanitiser collapses every non-allowlisted codepoint
    /// to `_` *before* the join, so the resulting path is always a
    /// single direct child of `root`. This test pins that property for
    /// the specific homoglyphs an attacker is most likely to try.
    #[tokio::test]
    async fn ensure_neutralises_unicode_separator_lookalikes() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        // Fullwidth solidus, division slash, fraction slash, and an
        // RTL override embedded mid-identifier.
        for hostile in ["a／b", "a∕b", "a⁄b", "a\u{202E}b"] {
            let ws = mgr.ensure(hostile).await.unwrap();
            assert_eq!(
                ws.path.parent().unwrap(),
                mgr.root(),
                "homoglyph {hostile:?} should land directly under root",
            );
            let component = ws.path.file_name().unwrap().to_str().unwrap();
            assert!(!component.contains('/'));
            assert!(!component.contains('\\'));
        }
    }

    /// Symlink planted under `root` post-construction must not redirect
    /// the materialised workspace outside `root`. The containment check
    /// canonicalises after `mkdir` and rejects the escape. This is
    /// SPEC §9.5 Invariant 2 in action.
    #[tokio::test]
    #[cfg(unix)]
    async fn ensure_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        // After the manager is constructed, plant `root/escape` →
        // outside. A subsequent `ensure("escape")` will find the path
        // already exists (the symlink), skip mkdir, then canonicalise
        // through the symlink and detect the containment violation.
        symlink(outside.path(), mgr.root().join("escape")).unwrap();

        let err = mgr.ensure("escape").await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::InvalidRoot(_)),
            "expected InvalidRoot from symlink escape, got {err:?}"
        );
    }

    /// `release` mirrors `ensure`'s validation: the same hostile inputs
    /// are rejected, so a misbehaving caller cannot use `release("..")`
    /// to nuke the workspace root's parent.
    #[tokio::test]
    async fn release_rejects_dot_dot_identifier() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let err = mgr.release("..").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
        // Critically: the parent of root must still exist.
        assert!(tmp.path().exists());
    }

    // ----- lifecycle hooks (SPEC §9.4) -----------------------------------

    /// `after_create` fires inside `ensure`, with the workspace as `cwd`.
    /// We assert "ran with cwd = workspace" by having the hook drop a
    /// marker file inside `.` and then checking its existence on disk;
    /// this also doubles as a positive proof the script ran at all.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_create_hook_fires_in_workspace_cwd() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_create: Some("touch ./created.marker".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let ws = mgr.ensure("ENG-1").await.unwrap();
        assert!(ws.path.join("created.marker").exists());
    }

    /// SPEC §9.4: `after_create` failure is fatal and the workspace must
    /// be rolled back so the next ensure can retry from scratch.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_create_failure_aborts_ensure_and_removes_dir() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_create: Some("exit 7".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.ensure("ENG-1").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Hook(_)), "got {err:?}");
        assert!(
            !tmp.path().join("ENG-1").exists(),
            "directory must be rolled back on hook failure",
        );
    }

    /// `after_create` only fires the *first* time `ensure` materialises
    /// the directory; reuse must not re-fire it. Otherwise a per-issue
    /// "git clone" hook would run twice.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_create_does_not_fire_on_reuse() {
        let tmp = TempDir::new().unwrap();
        // Increment a counter so we can prove exactly-one fire.
        let hooks = WorkspaceHooks {
            after_create: Some(
                "n=$(cat ./count 2>/dev/null || echo 0); echo $((n+1)) > ./count".into(),
            ),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        mgr.ensure("ENG-1").await.unwrap();
        mgr.ensure("ENG-1").await.unwrap();
        let count = std::fs::read_to_string(tmp.path().join("ENG-1/count")).unwrap();
        assert_eq!(count.trim(), "1", "after_create must fire exactly once");
    }

    /// A hook that exceeds `timeout_ms` is killed and reported as a
    /// `Hook` error. The error message names the timeout so an operator
    /// tailing logs can distinguish it from a non-zero exit.
    #[tokio::test]
    #[cfg(unix)]
    async fn hook_timeout_is_fatal_with_clear_error() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_create: Some("sleep 5".into()),
            timeout_ms: 100,
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.ensure("ENG-1").await.unwrap_err();
        match err {
            WorkspaceError::Hook(msg) => assert!(
                msg.contains("timed out"),
                "expected timeout in message, got {msg:?}",
            ),
            other => panic!("expected Hook(timeout), got {other:?}"),
        }
    }

    /// `before_run` failure is fatal to the dispatch attempt: the
    /// orchestrator must surface the error and not run the agent.
    #[tokio::test]
    #[cfg(unix)]
    async fn before_run_failure_is_fatal() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            before_run: Some("exit 1".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        mgr.ensure("ENG-1").await.unwrap();
        let err = mgr.before_run("ENG-1").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Hook(_)), "got {err:?}");
    }

    /// `before_run` is a no-op when no hook is configured. The trait
    /// must always be callable so the orchestrator can stay structurally
    /// the same regardless of operator config.
    #[tokio::test]
    async fn before_run_with_no_hook_is_ok() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();
        mgr.ensure("ENG-1").await.unwrap();
        mgr.before_run("ENG-1").await.unwrap();
    }

    /// Calling `before_run` before `ensure` is a programming error —
    /// surface it loudly rather than silently fabricating a workspace.
    #[tokio::test]
    #[cfg(unix)]
    async fn before_run_without_ensure_errors() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            before_run: Some("true".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.before_run("ENG-1").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Hook(_)), "got {err:?}");
    }

    /// SPEC §9.4: `after_run` failures are logged and swallowed. The
    /// trait signature returns `()`, so this test is mostly a witness
    /// that calling it on a failing hook neither panics nor leaks.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_run_failure_is_swallowed() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_run: Some("exit 99".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();
        mgr.ensure("ENG-1").await.unwrap();
        mgr.after_run("ENG-1").await; // returns `()`, must not panic
    }

    /// SPEC §9.4: `before_remove` failures are logged and ignored, and
    /// the directory must still be removed. Otherwise a flaky archival
    /// hook would pin stale workspaces forever.
    #[tokio::test]
    #[cfg(unix)]
    async fn before_remove_failure_is_swallowed_and_dir_is_removed() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            before_remove: Some("exit 1".into()),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let ws = mgr.ensure("ENG-1").await.unwrap();
        assert!(ws.path.is_dir());
        mgr.release("ENG-1").await.unwrap();
        assert!(!ws.path.exists(), "release must rmdir even when hook fails");
    }

    /// `before_remove` runs with the workspace as `cwd` (so an archival
    /// hook can `tar` the directory before it's deleted) and is skipped
    /// when the workspace does not exist (avoids spurious shell errors
    /// during the §8.6 startup sweep).
    #[tokio::test]
    #[cfg(unix)]
    async fn before_remove_runs_in_workspace_and_skips_when_missing() {
        let tmp = TempDir::new().unwrap();
        let archive_dir = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            // Drop a marker into an *external* directory so we can
            // observe execution after the workspace has been removed.
            before_remove: Some(format!(
                "touch {}/before-remove.marker",
                archive_dir.path().display()
            )),
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        // Releasing a never-created workspace must not invoke the hook.
        mgr.release("ghost").await.unwrap();
        assert!(!archive_dir.path().join("before-remove.marker").exists());

        // After ensure → release the hook fires exactly once.
        mgr.ensure("ENG-1").await.unwrap();
        mgr.release("ENG-1").await.unwrap();
        assert!(archive_dir.path().join("before-remove.marker").exists());
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
