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
//!    return value of [`WorkspaceManager::claim`], which is by
//!    construction a path inside the workspace root.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

/// Default per-snippet hook timeout (SPEC v2 §5.17: 5 minutes). The
/// longer default reflects that v2 hooks routinely do branch prep or
/// worktree creation. Hooks that exceed it are killed and reported as
/// a [`WorkspaceError::Hook`]. The timeout applies per snippet, not per
/// phase.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 300_000;

fn default_hook_timeout_ms() -> u64 {
    DEFAULT_HOOK_TIMEOUT_MS
}

/// Lifecycle shell hooks. SPEC v2 §5.17 names four phases:
///
/// | Hook            | Fires on                       | Failure semantics             |
/// |-----------------|--------------------------------|-------------------------------|
/// | `after_create`  | the `ensure` that created dir  | fatal — `ensure` returns Err  |
/// | `before_run`    | orchestrator pre-spawn         | fatal — caller skips the run  |
/// | `after_run`     | orchestrator post-spawn        | logged + ignored              |
/// | `before_remove` | the `release` before rmdir     | logged + ignored              |
///
/// Each phase is a *list of shell snippets*. Snippets in a list run
/// sequentially under `sh -lc <snippet>` with the workspace directory
/// as `cwd`; the first non-zero exit aborts the phase. An empty list is
/// equivalent to "no hook configured" — the call returns immediately.
/// `timeout_ms` applies per snippet, not per phase, so a phase composed
/// of several quick steps stays bounded without each step needing to
/// pad its own timeout.
///
/// Default: every list empty, timeout 5 minutes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHooks {
    /// Fires once, the first time `ensure` materialises the directory.
    /// A non-zero exit from any snippet aborts `ensure` and the just-
    /// created directory is removed so a retry sees a clean slate.
    #[serde(default)]
    pub after_create: Vec<String>,

    /// Fires before every agent dispatch. A non-zero exit from any
    /// snippet returns a fatal error to the caller (the orchestrator),
    /// which is expected to keep the issue claimed but not start the
    /// turn.
    #[serde(default)]
    pub before_run: Vec<String>,

    /// Fires after every agent dispatch. Snippet failures are logged
    /// and swallowed — this phase is for observability/cleanup, not
    /// control flow. It cannot block or fail a run that already
    /// happened. Subsequent snippets still run after a failing one.
    #[serde(default)]
    pub after_run: Vec<String>,

    /// Fires before `release` removes the workspace. Snippet failures
    /// are logged and swallowed; the rmdir proceeds. Useful for
    /// archival snapshots that must not block teardown. Subsequent
    /// snippets still run after a failing one.
    #[serde(default)]
    pub before_remove: Vec<String>,

    /// Per-snippet timeout in milliseconds; defaults to
    /// [`DEFAULT_HOOK_TIMEOUT_MS`].
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
            after_create: Vec::new(),
            before_run: Vec::new(),
            after_run: Vec::new(),
            before_remove: Vec::new(),
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

/// Sanitise `identifier` and reject the three "all-allowlisted but
/// still hostile" survivors (empty, `.`, `..`).
///
/// The sanitiser already maps every disallowed codepoint to `_`, so
/// only the allowlisted-but-reserved components survive: an empty
/// string (no on-disk name), `"."` (the workspace root itself), and
/// `".."` (the root's parent — SPEC §9.5 Invariant 2 explicitly
/// forbids it). Each is rejected with [`WorkspaceError::InvalidRoot`]
/// because they indicate a misconfigured tracker or a hostile input;
/// either way the orchestrator cannot proceed.
///
/// Exposed at module scope (rather than on a single manager) so each
/// strategy claimer ([`LocalFsWorkspace`], [`GitWorktreeClaimer`])
/// shares the same rule and the property tests cover it exactly once.
pub fn safe_component(identifier: &str) -> WorkspaceResult<String> {
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

/// Render a branch-name template using flat `{{key}}` substitution.
///
/// Strict: an unclosed `{{` or a `{{key}}` whose name is not in `vars`
/// returns [`WorkspaceError::InvalidRoot`]. Whitespace inside the
/// braces is trimmed, so `{{ identifier }}` and `{{identifier}}`
/// resolve identically. The renderer never emits a literal `{{...}}`
/// segment in the output; if it cannot fill a variable, the operator
/// hears about it.
///
/// This is the Phase 6 minimum needed for `branch_template` and
/// `child_branch_template` rendering. Phase 7 will replace it with
/// the full `{{path.to.value}}` renderer that walks nested context
/// (role, work item, parent, claim); the flat shape here keeps the
/// commit focused on git-worktree provisioning without pre-empting
/// the richer renderer's design.
pub fn render_branch_template(
    template: &str,
    vars: &BTreeMap<&str, &str>,
) -> WorkspaceResult<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| {
            WorkspaceError::InvalidRoot(format!(
                "branch template {template:?} has unclosed {{{{...}}}}",
            ))
        })?;
        let key = after[..end].trim();
        let value = vars.get(key).ok_or_else(|| {
            WorkspaceError::InvalidRoot(format!(
                "branch template {template:?} references unknown variable {key:?}",
            ))
        })?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
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

    /// A git CLI invocation made by a strategy claimer (e.g. the
    /// [`GitWorktreeClaimer`]) failed. The string carries the failing
    /// command's stderr tail and exit status so the operator can
    /// diagnose without re-running. Distinct from [`Self::Io`] so
    /// callers can branch on git-specific failure handling (retry,
    /// repair, surface to a follow-up issue) once Phase 8 wires the
    /// integration owner through this layer.
    #[error("workspace git failure: {0}")]
    Git(String),
}

/// Convenience alias matching the project-wide convention.
pub type WorkspaceResult<T> = Result<T, WorkspaceError>;

/// The handle [`WorkspaceManager::claim`] returns to the orchestrator.
///
/// SPEC v2 §5.8 (and the v1 §9.2 hook lifecycle it preserves) requires
/// each per-issue dispatch to record more than just a directory: the
/// workspace strategy in force, the base/branch refs the runner should
/// later verify, the owning work item, the cleanup policy that controls
/// terminal reclamation, and the verification report produced by the
/// pre-launch safety checks. `created_now` is preserved because the
/// `after_create` hook firing decision still depends on whether this
/// call materialised the directory or found it already on disk.
///
/// This iteration introduces the *shape* — the fields. Subsequent
/// checklist items (Phase 6) populate `strategy` from workflow config,
/// derive `branch`/`base_ref` from `BranchPolicyConfig` templates, and
/// run the cwd/branch/tree-clean checks that fill `verification`. Until
/// then every claim returns the [`ClaimStrategy::Directory`] strategy
/// with a [`VerificationStatus::Pending`] report — i.e. the v1 plain-
/// directory behaviour, now wrapped in the v2 envelope so that callers
/// (orchestrator, agents, tests) can be written against the target
/// shape rather than rewritten when each strategy lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceClaim {
    /// Absolute path to the per-issue workspace directory.
    pub path: PathBuf,
    /// Workspace strategy this claim was provisioned under. Determines
    /// later cleanup, branch verification, and integration semantics.
    pub strategy: ClaimStrategy,
    /// Base ref the strategy started from (e.g. `main`). `None` for
    /// strategies that do not branch (`Directory`) or have not yet
    /// been wired through to git.
    pub base_ref: Option<String>,
    /// Branch/ref the runner expects this workspace to be on at
    /// agent-launch time. `None` for strategies without git semantics.
    pub branch: Option<String>,
    /// Work item that owns this claim. `None` when the claim was
    /// minted before the orchestrator wired durable work items into
    /// the dispatcher path.
    pub owner: Option<ClaimOwner>,
    /// Cleanup policy applied when the owning work item reaches a
    /// terminal class. Mirrors `WorkspaceCleanupPolicy` in
    /// `symphony-config`; duplicated here so this crate stays free of
    /// a config dep.
    pub cleanup: CleanupPolicy,
    /// Outcome of pre-launch verification (cwd, branch, tree-clean).
    /// Default is [`VerificationStatus::Pending`] until a verifier
    /// from a later checklist item runs against the claim.
    pub verification: VerificationReport,
    /// True iff this call created the directory (vs. found it
    /// existing). Drives the `after_create` hook firing decision.
    pub created_now: bool,
}

/// Workspace strategy in force for a [`WorkspaceClaim`].
///
/// Mirrors `WorkspaceStrategyKind` in `symphony-config` (SPEC v2 §5.8)
/// without taking a config-crate dependency on this layer. The
/// orchestrator translates from config to this enum at claim time;
/// later checklist items add the `GitWorktree`/`ExistingWorktree`/
/// `SharedBranch` claimers and remove the temporary
/// [`Self::Directory`] fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStrategy {
    /// Plain directory under workspace root with no git semantics.
    /// Default until per-strategy claimers land; equivalent to v1
    /// behaviour.
    #[default]
    Directory,
    /// Fresh per-issue git worktree on a branch derived from
    /// `branch_template`.
    GitWorktree,
    /// Reuse a worktree already present on disk, optionally asserting
    /// it is checked out on a required branch.
    ExistingWorktree,
    /// Multiple work items share one branch and worktree (typically
    /// the integration owner's).
    SharedBranch,
}

/// Cleanup policy stamped on a claim.
///
/// Mirrors `WorkspaceCleanupPolicy` in `symphony-config`; duplicated to
/// keep `symphony-workspace` independent of the config crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPolicy {
    /// Retain on disk until the owning work item reaches a terminal
    /// class, then reclaim. SPEC v2 §5.8 default.
    #[default]
    RetainUntilDone,
    /// Remove as soon as the run completes.
    RemoveAfterRun,
    /// Never remove; operator owns cleanup. For shared/long-lived
    /// integration worktrees.
    RetainAlways,
}

/// Identifies which work item owns a claim.
///
/// Two-field shape keeps tracker identifiers (string keys like
/// `ENG-42`) and durable row ids decoupled — the dispatcher knows the
/// identifier before the work item is persisted, while later flow
/// stages know both. Future checklist items (Phase 7) wire this onto
/// the structured agent run request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClaimOwner {
    /// Tracker-side identifier (sanitised input the manager hashes
    /// into a directory name). Always present.
    pub identifier: String,
    /// Durable `WorkItemId` row id when the work item has been
    /// persisted. `None` for early dispatch paths that have not yet
    /// gone through `symphony-state`.
    pub work_item_id: Option<i64>,
}

/// Outcome of pre-launch verification checks bundled onto a claim.
///
/// Default is [`VerificationStatus::Pending`] with an empty `checks`
/// list — the data shape exists but no verifier runs against this
/// crate yet. Subsequent Phase 6 items populate `checks` with cwd,
/// branch, and clean-tree assertions and flip `status` to
/// [`VerificationStatus::Passed`] / [`VerificationStatus::Failed`]
/// before the orchestrator launches an agent against the claim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerificationReport {
    /// Aggregate status across `checks`. `Pending` until a verifier
    /// runs.
    pub status: VerificationStatus,
    /// Individual checks the verifier performed, in declaration order.
    pub checks: Vec<VerificationCheck>,
}

/// One verifier outcome inside a [`VerificationReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationCheck {
    /// Stable check name (e.g. `"cwd_in_workspace"`,
    /// `"branch_matches_required"`). Used for log lines and tests.
    pub name: String,
    /// Per-check verdict.
    pub status: VerificationStatus,
    /// Optional human-readable detail, populated on `Failed` so an
    /// operator tailing logs can diagnose without re-running.
    pub detail: Option<String>,
}

/// Aggregate or per-check verdict on a [`VerificationReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VerificationStatus {
    /// No verification has run yet. Default for newly minted claims.
    #[default]
    Pending,
    /// Verification did not apply (e.g. cwd-policy disabled, or no
    /// branch policy declared for this strategy).
    Skipped,
    /// Every check declared on this report passed.
    Passed,
    /// At least one check declared on this report failed.
    Failed,
}

/// Abstract per-issue workspace lifecycle.
///
/// The orchestrator calls [`Self::claim`] before launching an agent
/// session and [`Self::release`] when the issue reaches a terminal state.
/// Implementations are responsible for SPEC §9.5's safety invariants —
/// the orchestrator trusts whatever path comes back to be containment-
/// safe and ready to run a subprocess in.
///
/// `Send + Sync` so the orchestrator can store the manager as
/// `Arc<dyn WorkspaceManager>` and share it across poll-loop tasks.
#[async_trait]
pub trait WorkspaceManager: Send + Sync {
    /// Claim a workspace for `identifier` and return its handle.
    ///
    /// Idempotent: calling twice with the same identifier returns the
    /// same path; only the first call sets `created_now = true`. When
    /// `created_now` is true, implementations also fire the configured
    /// `after_create` hook before returning; a hook failure is fatal and
    /// the partially-created directory is rolled back.
    ///
    /// Returns a [`WorkspaceClaim`] carrying path plus strategy/branch/
    /// owner/cleanup/verification metadata. The
    /// [`LocalFsWorkspace`] adapter populates `strategy =
    /// ClaimStrategy::Directory`, `verification = Pending`, and leaves
    /// branch/owner empty; subsequent checklist items add the
    /// strategy-aware claimers and the verifier passes.
    async fn claim(&self, identifier: &str) -> WorkspaceResult<WorkspaceClaim>;

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
        // Delegates to the module-level [`safe_component`] so every
        // strategy claimer shares one rule.
        safe_component(identifier)
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
    /// Run every snippet in `scripts` sequentially under the same `cwd`.
    /// Returns `Ok(())` if every snippet succeeded; on the first non-zero
    /// exit (or timeout) returns the error and skips the rest. Empty
    /// `scripts` is a cheap no-op so callers do not need to pre-check.
    async fn run_hooks_fail_fast(
        &self,
        name: &'static str,
        scripts: &[String],
        cwd: &Path,
    ) -> WorkspaceResult<()> {
        for (idx, script) in scripts.iter().enumerate() {
            self.run_hook(name, idx, script, cwd).await?;
        }
        Ok(())
    }

    /// Run every snippet in `scripts` sequentially, logging-and-swallowing
    /// any failures so a flaky archival or observability snippet cannot
    /// block teardown / pin a workspace. Subsequent snippets still run
    /// after a failure — this matches the SPEC v2 §5.17 best-effort
    /// semantics for `after_run` and `before_remove`.
    async fn run_hooks_best_effort(&self, name: &'static str, scripts: &[String], cwd: &Path) {
        for (idx, script) in scripts.iter().enumerate() {
            if let Err(e) = self.run_hook(name, idx, script, cwd).await {
                warn!(error = %e, hook = name, index = idx, "hook snippet failed (ignored)");
            }
        }
    }

    async fn run_hook(
        &self,
        name: &'static str,
        index: usize,
        script: &str,
        cwd: &Path,
    ) -> WorkspaceResult<()> {
        debug!(hook = name, index, cwd = %cwd.display(), "running workspace hook");
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
                return Err(WorkspaceError::Hook(format!(
                    "{name}[{index}]: spawn failed: {e}"
                )));
            }
            Err(_) => {
                return Err(WorkspaceError::Hook(format!(
                    "{name}[{index}]: timed out after {}ms",
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
                "{name}[{index}]: exited {}: {}",
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
    async fn claim(&self, identifier: &str) -> WorkspaceResult<WorkspaceClaim> {
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
        // directory. SPEC v2 §5.17 makes a non-zero snippet fatal; we
        // also remove the just-created directory so a retry sees a
        // fresh slate (otherwise the dir exists but never finished
        // `after_create`, and the next ensure would skip the phase
        // entirely).
        if !existed
            && !self.hooks.after_create.is_empty()
            && let Err(e) = self
                .run_hooks_fail_fast("after_create", &self.hooks.after_create, &path)
                .await
        {
            let _ = fs::remove_dir_all(&path).await;
            return Err(e);
        }

        Ok(WorkspaceClaim {
            path,
            strategy: ClaimStrategy::Directory,
            base_ref: None,
            branch: None,
            owner: None,
            cleanup: CleanupPolicy::default(),
            verification: VerificationReport::default(),
            created_now: !existed,
        })
    }

    async fn before_run(&self, identifier: &str) -> WorkspaceResult<()> {
        if self.hooks.before_run.is_empty() {
            return Ok(());
        }
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
        self.run_hooks_fail_fast("before_run", &self.hooks.before_run, &path)
            .await
    }

    async fn after_run(&self, identifier: &str) {
        if self.hooks.after_run.is_empty() {
            return;
        }
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
        // SPEC v2 §5.17: failures logged and ignored, but later snippets
        // still run.
        self.run_hooks_best_effort("after_run", &self.hooks.after_run, &path)
            .await;
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
        // produce confusing log noise. Failures are logged and ignored;
        // the rmdir proceeds (SPEC v2 §5.17).
        if !self.hooks.before_remove.is_empty() && path.is_dir() {
            self.run_hooks_best_effort("before_remove", &self.hooks.before_remove, &path)
                .await;
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

/// Provisioner for the [`ClaimStrategy::GitWorktree`] strategy.
///
/// Materialises a fresh per-issue git worktree under `root` on a
/// branch derived from `branch_template`. Implementation follows
/// ARCHITECTURE_v2 §4.6: shell out to the `git` CLI rather than
/// pulling in `git2`/`gix`, so the dependency footprint stays small
/// until a concrete need motivates a typed library.
///
/// The claimer is intentionally narrow:
///
/// - it does *not* implement [`WorkspaceManager`] — that trait's
///   single-arg `claim(&str)` signature is too thin for git-worktree
///   provisioning, which needs a base ref and template; later phases
///   add a strategy-dispatching manager that picks claimers per work
///   item;
/// - it does *not* run lifecycle hooks — Phase 6 keeps hook semantics
///   on [`LocalFsWorkspace`] until the strategies converge under a
///   shared dispatcher;
/// - it does *not* yet verify cwd/branch/clean-tree — those are
///   distinct checklist items in Phase 6 and each lands with its own
///   commit and dedicated tests.
///
/// Idempotency rule: if the worktree directory already exists, the
/// claimer returns the existing path without re-running
/// `git worktree add`, mirroring [`LocalFsWorkspace::claim`]'s
/// reuse semantics. `created_now` distinguishes the two cases for
/// callers that want to fire one-shot setup hooks.
#[derive(Debug, Clone)]
pub struct GitWorktreeClaimer {
    repo: PathBuf,
    root: PathBuf,
    base_ref: String,
    branch_template: String,
    cleanup: CleanupPolicy,
}

impl GitWorktreeClaimer {
    /// Construct a claimer rooted at `root`, sourcing branches from
    /// `repo` against `base_ref`.
    ///
    /// `repo` must be an existing directory (the source git
    /// repository whose object database backs every produced
    /// worktree). `root` must also exist; symphony does not create
    /// either side because the operator's choice of layout is a
    /// deployment concern (mount points, volumes) and silently
    /// creating one would mask configuration mistakes — the same
    /// rationale [`LocalFsWorkspace::new`] applies to its root.
    ///
    /// Both paths are canonicalised so downstream containment checks
    /// compare absolute paths and a symlink planted under `root`
    /// cannot smuggle a worktree outside it.
    pub fn new(
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
        base_ref: impl Into<String>,
        branch_template: impl Into<String>,
        cleanup: CleanupPolicy,
    ) -> WorkspaceResult<Self> {
        let repo = repo.as_ref();
        if !repo.is_dir() {
            return Err(WorkspaceError::InvalidRoot(format!(
                "git repo {} does not exist or is not a directory",
                repo.display()
            )));
        }
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(WorkspaceError::InvalidRoot(format!(
                "workspace root {} does not exist or is not a directory",
                root.display()
            )));
        }
        let repo = std::fs::canonicalize(repo)
            .map_err(|e| WorkspaceError::InvalidRoot(format!("canonicalize repo: {e}")))?;
        let root = std::fs::canonicalize(root)
            .map_err(|e| WorkspaceError::InvalidRoot(format!("canonicalize root: {e}")))?;
        Ok(Self {
            repo,
            root,
            base_ref: base_ref.into(),
            branch_template: branch_template.into(),
            cleanup,
        })
    }

    /// Canonical workspace root the claimer mints worktrees under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Source repository whose object database backs each worktree.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Claim a per-issue worktree for `identifier`.
    ///
    /// Behaviour:
    ///
    /// 1. Sanitise `identifier` (`{{identifier}}` resolves to the
    ///    sanitised form so the branch name and the directory name
    ///    stay in lockstep).
    /// 2. Render `branch_template` with `identifier`. An unknown
    ///    variable in the template is fatal.
    /// 3. If the directory already exists, treat the claim as a reuse
    ///    and skip the git invocation. Returned `created_now = false`.
    /// 4. Otherwise run `git -C <repo> worktree add -b <branch>
    ///    <path> <base>`. If git reports the branch already exists
    ///    (a previous claim was rolled back without `git worktree
    ///    remove`), retry as `git worktree add <path> <branch>` so
    ///    the existing branch is checked out instead of being
    ///    overwritten.
    /// 5. Canonicalise the resulting path and assert it is contained
    ///    under `root` (SPEC §9.5 Invariant 2).
    ///
    /// Verification (`cwd`, `branch matches`, `clean tree`) is left
    /// at [`VerificationStatus::Pending`]; a later Phase 6 item adds
    /// the verifier passes that flip it to `Passed`/`Failed` before
    /// the orchestrator launches an agent.
    pub async fn claim(
        &self,
        identifier: &str,
        work_item_id: Option<i64>,
    ) -> WorkspaceResult<WorkspaceClaim> {
        let safe = safe_component(identifier)?;
        let path = self.root.join(&safe);

        let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
        vars.insert("identifier", safe.as_str());
        let branch = render_branch_template(&self.branch_template, &vars)?;

        let existed = fs::try_exists(&path)
            .await
            .map_err(|e| WorkspaceError::Io(format!("stat {}: {e}", path.display())))?;

        if !existed {
            self.git_worktree_add(&path, &branch).await?;
        }

        // Containment: even though the path was sanitised, a symlink
        // planted under `root` could still redirect the worktree
        // outside it. Canonicalise after creation and confirm we
        // landed inside the canonical root.
        let canonical = std::fs::canonicalize(&path)
            .map_err(|e| WorkspaceError::Io(format!("canonicalize {}: {e}", path.display())))?;
        if !canonical.starts_with(&self.root) {
            return Err(WorkspaceError::InvalidRoot(format!(
                "worktree path {} escaped root {}",
                canonical.display(),
                self.root.display()
            )));
        }

        Ok(WorkspaceClaim {
            path,
            strategy: ClaimStrategy::GitWorktree,
            base_ref: Some(self.base_ref.clone()),
            branch: Some(branch),
            owner: Some(ClaimOwner {
                identifier: safe,
                work_item_id,
            }),
            cleanup: self.cleanup,
            verification: VerificationReport::default(),
            created_now: !existed,
        })
    }

    async fn git_worktree_add(&self, path: &Path, branch: &str) -> WorkspaceResult<()> {
        // First attempt: create a new branch at `base_ref` and check
        // it out into `path`. This is the happy path — a fresh issue
        // with no prior branch.
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(branch)
            .arg(path)
            .arg(&self.base_ref)
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git worktree add: {e}")))?;

        if out.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

        // Recover from "branch already exists" by checking the
        // existing branch out instead of re-creating it. A prior run
        // that crashed between branch creation and worktree add (or
        // a manual `git worktree remove` that didn't `branch -D`)
        // leaves the branch behind; force-creating a new branch with
        // `-B` would silently discard whatever was on it. Re-using is
        // the safer default.
        if stderr.contains("already exists") || stderr.contains("already used") {
            let retry = Command::new("git")
                .arg("-C")
                .arg(&self.repo)
                .arg("worktree")
                .arg("add")
                .arg(path)
                .arg(branch)
                .output()
                .await
                .map_err(|e| WorkspaceError::Git(format!("spawn git worktree add (retry): {e}")))?;
            if retry.status.success() {
                return Ok(());
            }
            return Err(WorkspaceError::Git(format!(
                "git worktree add {} {} failed ({}): {}",
                path.display(),
                branch,
                retry.status,
                String::from_utf8_lossy(&retry.stderr).trim()
            )));
        }

        Err(WorkspaceError::Git(format!(
            "git worktree add -b {} {} {} failed ({}): {}",
            branch,
            path.display(),
            self.base_ref,
            out.status,
            stderr.trim()
        )))
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

        let ws = mgr.claim("ENG-42").await.unwrap();

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

        let first = mgr.claim("ENG-42").await.unwrap();
        let second = mgr.claim("ENG-42").await.unwrap();

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

        let ws = mgr.claim("ENG-42").await.unwrap();
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

        let ws = mgr.claim("../etc/passwd").await.unwrap();

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

        let ws = mgr.claim("ORG/repo#7").await.unwrap();
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

        let err = mgr.claim("..").await.unwrap_err();
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

        let err = mgr.claim(".").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// Empty input has no on-disk representation and almost certainly
    /// indicates a bug upstream (a tracker returning a blank identifier).
    /// Surface it loudly rather than materialising the workspace root.
    #[tokio::test]
    async fn ensure_rejects_empty_identifier() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let err = mgr.claim("").await.unwrap_err();
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
            let ws = mgr.claim(hostile).await.unwrap();
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

        let err = mgr.claim("escape").await.unwrap_err();
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
            after_create: vec!["touch ./created.marker".into()],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let ws = mgr.claim("ENG-1").await.unwrap();
        assert!(ws.path.join("created.marker").exists());
    }

    /// SPEC §9.4: `after_create` failure is fatal and the workspace must
    /// be rolled back so the next ensure can retry from scratch.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_create_failure_aborts_ensure_and_removes_dir() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_create: vec!["exit 7".into()],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.claim("ENG-1").await.unwrap_err();
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
            after_create: vec![
                "n=$(cat ./count 2>/dev/null || echo 0); echo $((n+1)) > ./count".into(),
            ],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        mgr.claim("ENG-1").await.unwrap();
        mgr.claim("ENG-1").await.unwrap();
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
            after_create: vec!["sleep 5".into()],
            timeout_ms: 100,
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.claim("ENG-1").await.unwrap_err();
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
            before_run: vec!["exit 1".into()],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        mgr.claim("ENG-1").await.unwrap();
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
        mgr.claim("ENG-1").await.unwrap();
        mgr.before_run("ENG-1").await.unwrap();
    }

    /// Calling `before_run` before `ensure` is a programming error —
    /// surface it loudly rather than silently fabricating a workspace.
    #[tokio::test]
    #[cfg(unix)]
    async fn before_run_without_ensure_errors() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            before_run: vec!["true".into()],
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
            after_run: vec!["exit 99".into()],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();
        mgr.claim("ENG-1").await.unwrap();
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
            before_remove: vec!["exit 1".into()],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let ws = mgr.claim("ENG-1").await.unwrap();
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
            before_remove: vec![format!(
                "touch {}/before-remove.marker",
                archive_dir.path().display()
            )],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        // Releasing a never-created workspace must not invoke the hook.
        mgr.release("ghost").await.unwrap();
        assert!(!archive_dir.path().join("before-remove.marker").exists());

        // After ensure → release the hook fires exactly once.
        mgr.claim("ENG-1").await.unwrap();
        mgr.release("ENG-1").await.unwrap();
        assert!(archive_dir.path().join("before-remove.marker").exists());
    }

    /// SPEC v2 §5.17: snippets in a fail-fast phase run sequentially in
    /// declaration order, and a non-zero exit aborts the phase before
    /// later snippets run. Asserted by appending each snippet's index to
    /// a marker file and inspecting the contents after the failure.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_create_snippets_run_sequentially_and_stop_on_failure() {
        let tmp = TempDir::new().unwrap();
        let hooks = WorkspaceHooks {
            after_create: vec![
                "echo a >> ./trace".into(),
                "echo b >> ./trace; exit 5".into(),
                "echo c >> ./trace".into(),
            ],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        let err = mgr.claim("ENG-1").await.unwrap_err();
        match &err {
            WorkspaceError::Hook(msg) => {
                assert!(
                    msg.contains("after_create[1]"),
                    "expected snippet index in error, got {msg:?}"
                );
            }
            other => panic!("expected Hook, got {other:?}"),
        }
        // Directory is rolled back along with the trace file, so we
        // cannot inspect the file directly. The error message above is
        // the structural witness that snippet 1 (not 0 or 2) failed.
        assert!(!tmp.path().join("ENG-1").exists());
    }

    /// SPEC v2 §5.17: an empty list is equivalent to "no hook
    /// configured" — the lifecycle method must be a cheap no-op,
    /// distinguishable from "hook ran with no snippets" only by the
    /// absence of any subprocess.
    #[tokio::test]
    async fn empty_hook_list_is_a_no_op() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::with_hooks(
            tmp.path(),
            WorkspaceHooks {
                after_create: vec![],
                before_run: vec![],
                after_run: vec![],
                before_remove: vec![],
                ..Default::default()
            },
        )
        .unwrap();

        mgr.claim("ENG-1").await.unwrap();
        mgr.before_run("ENG-1").await.unwrap();
        mgr.after_run("ENG-1").await;
        mgr.release("ENG-1").await.unwrap();
    }

    /// SPEC v2 §5.17: best-effort phases (`after_run`, `before_remove`)
    /// run every snippet even when an earlier one fails. Failures are
    /// logged and ignored; later snippets' side effects must still be
    /// observable.
    #[tokio::test]
    #[cfg(unix)]
    async fn after_run_continues_past_failing_snippet() {
        let tmp = TempDir::new().unwrap();
        let evidence = TempDir::new().unwrap();
        let marker = evidence.path().join("ran.marker");
        let hooks = WorkspaceHooks {
            after_run: vec!["exit 1".into(), format!("touch {}", marker.display())],
            ..Default::default()
        };
        let mgr = LocalFsWorkspace::with_hooks(tmp.path(), hooks).unwrap();

        mgr.claim("ENG-1").await.unwrap();
        mgr.after_run("ENG-1").await;
        assert!(
            marker.exists(),
            "best-effort phase must keep running after a failing snippet"
        );
    }

    /// Default timeout is the SPEC v2 §5.17 value (5 minutes), not the
    /// historical 60s. Pinned so a future drift forces this test to
    /// update alongside the constant.
    #[test]
    fn default_hook_timeout_is_five_minutes() {
        assert_eq!(WorkspaceHooks::default().timeout_ms, 300_000);
        assert_eq!(DEFAULT_HOOK_TIMEOUT_MS, 300_000);
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

        let ws = mgr.claim("ENG-1").await.unwrap();
        assert!(ws.created_now);
    }

    // ----- WorkspaceClaim shape (Phase 6) --------------------------------

    /// Phase 6 step 1: every claim must surface the v2 envelope —
    /// strategy, base ref, branch, owner, cleanup policy, and
    /// verification report — even when the per-strategy claimers have
    /// not yet landed. The defaults pin the "v1 baseline wrapped in v2
    /// shape" contract: `Directory` strategy, no branch info, default
    /// cleanup, and a `Pending` verification report. Subsequent
    /// checklist items override these as each strategy/verifier comes
    /// online; this test fails loudly when one of them silently
    /// regresses the default.
    #[tokio::test]
    async fn claim_returns_v2_envelope_with_defaults() {
        let tmp = TempDir::new().unwrap();
        let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

        let claim = mgr.claim("ENG-7").await.unwrap();

        assert!(claim.path.is_dir());
        assert!(claim.path.starts_with(mgr.root()));
        assert_eq!(claim.strategy, ClaimStrategy::Directory);
        assert_eq!(claim.base_ref, None);
        assert_eq!(claim.branch, None);
        assert_eq!(claim.owner, None);
        assert_eq!(claim.cleanup, CleanupPolicy::RetainUntilDone);
        assert_eq!(claim.verification.status, VerificationStatus::Pending);
        assert!(
            claim.verification.checks.is_empty(),
            "verification.checks should start empty until a verifier runs",
        );
        assert!(claim.created_now);
    }

    /// `WorkspaceClaim` is a plain data struct: assigning a richer
    /// strategy/branch/owner is what the strategy-aware claimers in
    /// later checklist items will do, so the shape must round-trip
    /// every populated field. This test pins the field set as a
    /// regression guard for the v2 envelope.
    #[test]
    fn workspace_claim_carries_every_v2_field() {
        let claim = WorkspaceClaim {
            path: PathBuf::from("/tmp/symphony/ENG-7"),
            strategy: ClaimStrategy::GitWorktree,
            base_ref: Some("main".into()),
            branch: Some("symphony/ENG-7".into()),
            owner: Some(ClaimOwner {
                identifier: "ENG-7".into(),
                work_item_id: Some(42),
            }),
            cleanup: CleanupPolicy::RemoveAfterRun,
            verification: VerificationReport {
                status: VerificationStatus::Passed,
                checks: vec![VerificationCheck {
                    name: "cwd_in_workspace".into(),
                    status: VerificationStatus::Passed,
                    detail: None,
                }],
            },
            created_now: true,
        };

        assert_eq!(claim.strategy, ClaimStrategy::GitWorktree);
        assert_eq!(claim.base_ref.as_deref(), Some("main"));
        assert_eq!(claim.branch.as_deref(), Some("symphony/ENG-7"));
        let owner = claim.owner.as_ref().expect("owner");
        assert_eq!(owner.identifier, "ENG-7");
        assert_eq!(owner.work_item_id, Some(42));
        assert_eq!(claim.cleanup, CleanupPolicy::RemoveAfterRun);
        assert_eq!(claim.verification.status, VerificationStatus::Passed);
        assert_eq!(claim.verification.checks.len(), 1);
        assert_eq!(claim.verification.checks[0].name, "cwd_in_workspace");
    }

    // ----- branch template renderer (Phase 6) -----------------------------

    /// `{{identifier}}` resolves from the supplied vars. The default
    /// branch template (`"symphony/{{identifier}}"`) is the canonical
    /// case driving Phase 6's worktree provisioning.
    #[test]
    fn render_branch_template_substitutes_known_variable() {
        let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
        vars.insert("identifier", "ENG-42");
        let out = render_branch_template("symphony/{{identifier}}", &vars).unwrap();
        assert_eq!(out, "symphony/ENG-42");
    }

    /// Whitespace inside `{{ ... }}` is trimmed so an operator's
    /// `{{ identifier }}` does not silently miss the lookup.
    #[test]
    fn render_branch_template_trims_whitespace_inside_braces() {
        let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
        vars.insert("identifier", "ENG-42");
        let out = render_branch_template("symphony/{{ identifier }}", &vars).unwrap();
        assert_eq!(out, "symphony/ENG-42");
    }

    /// Templates with no placeholders pass through verbatim — the
    /// renderer must not introduce spurious churn on plain literals.
    #[test]
    fn render_branch_template_passes_literals_through() {
        let vars: BTreeMap<&str, &str> = BTreeMap::new();
        assert_eq!(
            render_branch_template("just/a/literal", &vars).unwrap(),
            "just/a/literal",
        );
    }

    /// An unknown variable is fatal: SPEC v2 §5.9 requires unknown
    /// vars to fail render rather than silently emit literal
    /// `{{...}}` segments. A branch named `symphony/{{role}}` would
    /// otherwise be an obvious operator footgun.
    #[test]
    fn render_branch_template_rejects_unknown_variable() {
        let vars: BTreeMap<&str, &str> = BTreeMap::new();
        let err = render_branch_template("symphony/{{identifier}}", &vars).unwrap_err();
        match err {
            WorkspaceError::InvalidRoot(msg) => {
                assert!(
                    msg.contains("identifier"),
                    "error should name the missing variable, got {msg:?}",
                );
            }
            other => panic!("expected InvalidRoot, got {other:?}"),
        }
    }

    /// An unclosed `{{` is a malformed template — surface it loudly
    /// so the operator fixes the typo before any worktree is created.
    #[test]
    fn render_branch_template_rejects_unclosed_placeholder() {
        let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
        vars.insert("identifier", "ENG-42");
        let err = render_branch_template("symphony/{{identifier", &vars).unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// Multiple placeholders in one template are all resolved.
    #[test]
    fn render_branch_template_handles_multiple_variables() {
        let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
        vars.insert("role", "qa");
        vars.insert("identifier", "ENG-42");
        let out = render_branch_template("symphony/{{role}}/{{identifier}}", &vars).unwrap();
        assert_eq!(out, "symphony/qa/ENG-42");
    }

    // ----- GitWorktreeClaimer (Phase 6) -----------------------------------

    /// Initialise an empty git repo with one commit on `main` so
    /// `git worktree add` has a base ref to branch from. Centralised
    /// here to keep the worktree tests focused on the claimer's
    /// behaviour rather than git plumbing.
    #[cfg(unix)]
    async fn init_repo_with_main_commit() -> TempDir {
        let dir = TempDir::new().unwrap();
        let path = dir.path();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@symphony.invalid"],
            vec!["config", "user.name", "test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(&args)
                .output()
                .await
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {:?}", out);
        }
        std::fs::write(path.join("README.md"), "x").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
            let out = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(&args)
                .output()
                .await
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {:?}", out);
        }
        dir
    }

    /// Read the branch a worktree is checked out on so the test can
    /// witness the rendered branch template made it onto disk.
    #[cfg(unix)]
    async fn worktree_branch(path: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .await
            .expect("git rev-parse");
        assert!(out.status.success(), "rev-parse failed: {:?}", out);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Happy path: claim materialises a worktree under `root`, on the
    /// rendered branch, with the v2 envelope populated. This is the
    /// regression guard for the Phase 6 git_worktree strategy.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_creates_branch_and_worktree() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap();

        let claim = claimer.claim("ENG-42", Some(7)).await.unwrap();

        assert!(claim.path.is_dir());
        assert!(claim.path.starts_with(claimer.root()));
        assert_eq!(claim.strategy, ClaimStrategy::GitWorktree);
        assert_eq!(claim.base_ref.as_deref(), Some("main"));
        assert_eq!(claim.branch.as_deref(), Some("symphony/ENG-42"));
        let owner = claim.owner.as_ref().expect("owner present");
        assert_eq!(owner.identifier, "ENG-42");
        assert_eq!(owner.work_item_id, Some(7));
        assert_eq!(claim.cleanup, CleanupPolicy::RetainUntilDone);
        assert_eq!(claim.verification.status, VerificationStatus::Pending);
        assert!(claim.created_now);
        assert_eq!(worktree_branch(&claim.path).await, "symphony/ENG-42");
    }

    /// SPEC §9.2-equivalent reuse semantics: the second claim for
    /// the same identifier returns the existing worktree without
    /// re-running `git worktree add`, and reports `created_now =
    /// false`. The `after_create`-style firing decision (when later
    /// phases unify hooks under a strategy dispatcher) depends on
    /// this flag.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_is_idempotent() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap();

        let first = claimer.claim("ENG-42", None).await.unwrap();
        let second = claimer.claim("ENG-42", None).await.unwrap();

        assert_eq!(first.path, second.path);
        assert!(first.created_now);
        assert!(!second.created_now, "reuse must not report created_now");
        assert_eq!(second.branch.as_deref(), Some("symphony/ENG-42"));
    }

    /// Branch template with a variable the claimer does not provide
    /// is fatal — the operator must hear about a typo (`{{role}}`,
    /// `{{parent}}`) before any worktree is created. The Phase 6
    /// renderer only supplies `{{identifier}}`; richer context is a
    /// Phase 7 concern.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_rejects_unknown_template_variable() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{role}}/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap();

        let err = claimer.claim("ENG-42", None).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
        // No directory should have been created on a render failure.
        assert!(!root.path().join("ENG-42").exists());
    }

    /// Sanitisation rule must apply to git-worktree claims too: a
    /// hostile identifier collapses to a single child component
    /// under `root`, the rendered branch reflects the sanitised
    /// form, and the path stays contained.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_sanitises_identifier() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap();

        let claim = claimer.claim("ORG/repo#7", None).await.unwrap();
        assert_eq!(claim.path.parent().unwrap(), claimer.root());
        assert_eq!(claim.path.file_name().unwrap(), "ORG_repo_7");
        assert_eq!(claim.branch.as_deref(), Some("symphony/ORG_repo_7"));
        assert_eq!(worktree_branch(&claim.path).await, "symphony/ORG_repo_7");
    }

    /// Reserved component (`..`) survives sanitisation but must be
    /// rejected before any git invocation. Mirrors
    /// [`LocalFsWorkspace`]'s containment guarantee.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_rejects_dot_dot_identifier() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap();

        let err = claimer.claim("..", None).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// `cleanup` flows verbatim from claimer config to the produced
    /// claim so later cleanup logic (Phase 11+) can branch on it.
    #[tokio::test]
    #[cfg(unix)]
    async fn git_worktree_claim_propagates_cleanup_policy() {
        let repo = init_repo_with_main_commit().await;
        let root = TempDir::new().unwrap();
        let claimer = GitWorktreeClaimer::new(
            repo.path(),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::RemoveAfterRun,
        )
        .unwrap();

        let claim = claimer.claim("ENG-1", None).await.unwrap();
        assert_eq!(claim.cleanup, CleanupPolicy::RemoveAfterRun);
    }

    /// Constructor validates that `repo` exists. A typo in the
    /// repo path is a deployment-config bug; surface it before any
    /// claim attempt rather than failing every claim with a confusing
    /// `git` error.
    #[test]
    fn git_worktree_claimer_rejects_missing_repo() {
        let root = TempDir::new().unwrap();
        let err = GitWorktreeClaimer::new(
            root.path().join("nope"),
            root.path(),
            "main",
            "symphony/{{identifier}}",
            CleanupPolicy::default(),
        )
        .unwrap_err();
        assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
    }

    /// Defaults document the "no work has been done yet" state of a
    /// verification report. Pinning them here means a future
    /// reorganisation that flipped `Pending` to e.g. `Skipped` would
    /// fail this test loudly rather than quietly mark every claim as
    /// already-verified.
    #[test]
    fn verification_report_defaults_to_pending() {
        let report = VerificationReport::default();
        assert_eq!(report.status, VerificationStatus::Pending);
        assert!(report.checks.is_empty());
    }
}
