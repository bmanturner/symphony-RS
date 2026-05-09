//! End-to-end integration coverage for SPEC v2 §8.3 pre-launch safety
//! gates. These tests exercise the public API surface of
//! `symphony-workspace` against the five hostile scenarios the Phase 6
//! checklist roll-up enumerates: path traversal, wrong cwd, wrong
//! branch, dirty tree, and shared branch policy. Unit tests inside
//! `lib.rs` already pin individual verifier branches; this file pins
//! the *composition*: a claim minted by a real claimer and then driven
//! through the verifier methods in the order the orchestrator runs
//! them at launch time.
//!
//! Each test is `#[cfg(unix)]` because the helpers shell out to `git`
//! and rely on POSIX `tempfile` semantics. The CI runner provides git
//! on every supported target; if it does not, the gate is honest about
//! why it cannot verify.

#![cfg(unix)]

use std::path::Path;

use symphony_workspace::{
    CleanupPolicy, ExistingWorktreeClaimer, GitWorktreeClaimer, LocalFsWorkspace,
    SharedBranchClaimer, VerificationStatus, WorkspaceError, WorkspaceManager,
};
use tempfile::TempDir;
use tokio::process::Command;

/// Bootstrap a git repository with one commit on `main`. Mirrors the
/// helper in `lib.rs`'s test module so the integration suite does not
/// have to depend on private items.
async fn init_repo() -> TempDir {
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
        assert!(out.status.success(), "git {args:?}: {out:?}");
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
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    dir
}

async fn run_git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .expect("git spawn");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

// ----- Path traversal --------------------------------------------------------

/// `LocalFsWorkspace::claim` must refuse identifiers that resolve to
/// the workspace root itself or its parent. The on-disk side-effect of
/// either acceptance would be catastrophic (an agent running in `root`
/// would corrupt sibling claims; `..` would let `release` rmdir above
/// the configured root), so the claimer rejects them at the sanitiser.
#[tokio::test]
async fn local_fs_claim_rejects_dot_and_dot_dot() {
    let tmp = TempDir::new().unwrap();
    let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();
    for hostile in [".", ".."] {
        let err = mgr.claim(hostile).await.unwrap_err();
        assert!(
            matches!(err, WorkspaceError::InvalidRoot(_)),
            "claim({hostile:?}) must reject as InvalidRoot, got {err:?}"
        );
    }
    assert!(tmp.path().exists(), "root must remain intact");
}

/// Slash-bearing identifiers cannot smuggle a path component upward.
/// The sanitiser collapses `/` to `_`, the resulting component lands
/// directly under `root`, and the parent of `root` stays untouched.
#[tokio::test]
async fn local_fs_claim_keeps_traversal_inside_root() {
    let tmp = TempDir::new().unwrap();
    let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

    let claim = mgr.claim("../../etc/passwd").await.unwrap();
    assert!(claim.path.starts_with(mgr.root()));
    assert_eq!(claim.path.parent().unwrap(), mgr.root());
    let component = claim.path.file_name().unwrap().to_str().unwrap();
    assert!(!component.contains('/'));
    assert!(!component.contains('\\'));
}

/// Every claimer rejects the same reserved identifiers up front.
/// Pinning this across `GitWorktreeClaimer`, `ExistingWorktreeClaimer`,
/// and `SharedBranchClaimer` ensures a future strategy added without
/// the same guard is caught by this matrix rather than at production.
#[tokio::test]
async fn every_claimer_rejects_dot_dot_identifier() {
    let repo = init_repo().await;
    let root = TempDir::new().unwrap();

    let git = GitWorktreeClaimer::new(
        repo.path(),
        root.path(),
        "main",
        "symphony/{{identifier}}",
        CleanupPolicy::default(),
    )
    .unwrap();
    let err = git.claim("..", None).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::InvalidRoot(_)));

    let existing =
        ExistingWorktreeClaimer::new(repo.path(), Some("main".into()), CleanupPolicy::default())
            .unwrap();
    let err = existing.claim("..", None).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::InvalidRoot(_)));

    let shared =
        SharedBranchClaimer::new(repo.path(), "main", CleanupPolicy::RetainAlways).unwrap();
    let err = shared.claim("..", None).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::InvalidRoot(_)));
}

// ----- Wrong cwd -------------------------------------------------------------

/// `WorkspaceClaim::verify_cwd` must reject a cwd that resolves outside
/// the claim. The orchestrator runs this immediately before agent
/// launch; if it returns false the run must abort. A sibling directory
/// adjacent to the workspace root is the most realistic failure (an
/// operator launched symphony from the wrong terminal tab).
#[tokio::test]
async fn wrong_cwd_fails_pre_launch_gate() {
    let tmp = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();

    let mut claim = mgr.claim("ENG-1").await.unwrap();
    assert!(
        !claim.verify_cwd(outside.path()),
        "cwd outside the claim must fail the gate"
    );
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
}

/// A `..`-laden cwd that *resolves* outside the workspace must also be
/// rejected: canonicalisation, not lexical comparison, decides
/// containment.
#[tokio::test]
async fn wrong_cwd_via_dot_dot_fails_pre_launch_gate() {
    let tmp = TempDir::new().unwrap();
    let mgr = LocalFsWorkspace::new(tmp.path()).unwrap();
    let mut claim = mgr.claim("ENG-1").await.unwrap();

    // `<claim>/..` resolves to `root`, which is *not* a descendant of
    // the claim — verifier must reject.
    let escape = claim.path.join("..");
    assert!(!claim.verify_cwd(&escape));
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
}

// ----- Wrong branch ----------------------------------------------------------

/// A worktree minted on branch `symphony/ENG-1` that drifts (operator
/// checkout, detached HEAD after rebase) must fail `verify_branch`
/// even though the original claim succeeded. This is the SPEC §8.3
/// "git branch/ref equals expected branch" gate.
#[tokio::test]
async fn wrong_branch_fails_pre_launch_gate() {
    let repo = init_repo().await;
    let root = TempDir::new().unwrap();
    let claimer = GitWorktreeClaimer::new(
        repo.path(),
        root.path(),
        "main",
        "symphony/{{identifier}}",
        CleanupPolicy::RetainAlways,
    )
    .unwrap();

    let mut claim = claimer.claim("ENG-1", None).await.unwrap();
    assert_eq!(claim.branch.as_deref(), Some("symphony/ENG-1"));

    // Operator detaches HEAD inside the worktree behind our back
    // (e.g. mid-rebase or a stray `git checkout <sha>`). The verifier
    // must catch the drift — `--abbrev-ref HEAD` then reports `HEAD`,
    // which never matches `symphony/ENG-1`.
    run_git(&claim.path, &["checkout", "-q", "--detach"]).await;

    assert!(!claim.verify_branch().await);
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
}

/// Detached HEAD reports as the literal `HEAD`, which never matches a
/// real branch. The verifier must reject it as a wrong-branch failure
/// rather than treating "HEAD" as a sentinel pass.
#[tokio::test]
async fn detached_head_fails_branch_gate() {
    let repo = init_repo().await;
    // Detach HEAD on the source repo and then claim it via the
    // existing-worktree strategy without a required branch (so claim
    // succeeds), then assert the post-hoc branch verifier rejects.
    run_git(repo.path(), &["checkout", "-q", "--detach"]).await;
    let claimer =
        ExistingWorktreeClaimer::new(repo.path(), None, CleanupPolicy::RetainAlways).unwrap();
    let mut claim = claimer.claim("ENG-7", None).await.unwrap();
    // The claim discovered the current "branch" as `HEAD`. Force a
    // mismatch by overwriting the required branch on the claim
    // envelope and re-running the verifier — that exercises the
    // mismatch path the orchestrator hits when it remembers what
    // branch the claim was *minted on* and the worktree has since
    // drifted into detached HEAD.
    claim.branch = Some("main".into());
    assert!(!claim.verify_branch().await);
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
}

// ----- Dirty tree ------------------------------------------------------------

/// A worktree with an untracked file must fail `verify_clean_tree(true)`.
/// The orchestrator MUST NOT launch a mutation-capable agent after this.
#[tokio::test]
async fn dirty_tree_fails_pre_launch_gate() {
    let repo = init_repo().await;
    let root = TempDir::new().unwrap();
    let claimer = GitWorktreeClaimer::new(
        repo.path(),
        root.path(),
        "main",
        "symphony/{{identifier}}",
        CleanupPolicy::RetainAlways,
    )
    .unwrap();
    let mut claim = claimer.claim("ENG-9", None).await.unwrap();
    std::fs::write(claim.path.join("stray.txt"), "drift").unwrap();

    assert!(!claim.verify_clean_tree(true).await);
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
}

/// Policy disabled (`require_clean_tree_before_run = false`) records a
/// `Skipped` check and returns `true` even when the tree is dirty. SPEC
/// §8.3: skip is neither a pass nor a fail.
#[tokio::test]
async fn dirty_tree_skipped_when_policy_disabled() {
    let repo = init_repo().await;
    let root = TempDir::new().unwrap();
    let claimer = GitWorktreeClaimer::new(
        repo.path(),
        root.path(),
        "main",
        "symphony/{{identifier}}",
        CleanupPolicy::RetainAlways,
    )
    .unwrap();
    let mut claim = claimer.claim("ENG-9", None).await.unwrap();
    std::fs::write(claim.path.join("stray.txt"), "drift").unwrap();

    // Dirty tree, but policy off. Verifier short-circuits to Skipped
    // without shelling out to git.
    assert!(claim.verify_clean_tree(false).await);
    assert_ne!(claim.verification.status, VerificationStatus::Failed);
    assert_eq!(
        claim.verification.checks.last().unwrap().status,
        VerificationStatus::Skipped
    );
}

// ----- Shared branch policy --------------------------------------------------

/// `SharedBranchClaimer` rejects `RemoveAfterRun` cleanup at construction
/// because reclaiming the directory at one run's end would clobber
/// sibling work items still operating on the shared branch. The error
/// must mention the offending policy so the operator can fix the
/// workflow config without grepping source.
#[test]
fn shared_branch_rejects_remove_after_run_policy() {
    let tmp = TempDir::new().unwrap();
    let err = SharedBranchClaimer::new(
        tmp.path(),
        "symphony/integration",
        CleanupPolicy::RemoveAfterRun,
    )
    .unwrap_err();
    match err {
        WorkspaceError::InvalidRoot(detail) => assert!(detail.contains("remove_after_run")),
        other => panic!("expected InvalidRoot, got {other:?}"),
    }
}

/// Happy path: a worktree on the configured shared branch yields a
/// claim, and the full pre-launch verifier sequence (cwd, branch,
/// clean-tree) composes to `Passed`. Pinning this guards against a
/// regression where the shared strategy silently bypasses one of the
/// gates.
#[tokio::test]
async fn shared_branch_full_pre_launch_sequence_passes() {
    let repo = init_repo().await;
    run_git(
        repo.path(),
        &["checkout", "-q", "-b", "symphony/integration"],
    )
    .await;
    let claimer = SharedBranchClaimer::new(
        repo.path(),
        "symphony/integration",
        CleanupPolicy::RetainAlways,
    )
    .unwrap();

    let mut claim = claimer.claim("INT-1", Some(1)).await.unwrap();
    assert!(claim.verify_cwd(&claim.path.clone()));
    assert!(claim.verify_branch().await);
    assert!(claim.verify_clean_tree(true).await);
    assert_eq!(claim.verification.status, VerificationStatus::Passed);
    assert_eq!(claim.verification.checks.len(), 3);
}

/// Branch mismatch is unconditionally fatal at claim time for the
/// shared strategy — there is no "match anything" escape hatch. An
/// operator who renames the integration branch out from under symphony
/// must see the failure before any mutation runs.
#[tokio::test]
async fn shared_branch_rejects_claim_on_wrong_branch() {
    let repo = init_repo().await;
    run_git(repo.path(), &["checkout", "-q", "-b", "feature/x"]).await;
    let claimer = SharedBranchClaimer::new(
        repo.path(),
        "symphony/integration",
        CleanupPolicy::RetainAlways,
    )
    .unwrap();
    let err = claimer.claim("INT-1", None).await.unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Git(msg)
            if msg.contains("feature/x") && msg.contains("symphony/integration")),
        "expected Git error naming both branches, got {err:?}"
    );
}
