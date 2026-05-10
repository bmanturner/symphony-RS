//! Phase 13 deterministic dirty/wrong-branch launch-blocking scenario.
//!
//! A specialist workspace is claimed correctly, then drifts before the
//! agent launch point. The composed pre-launch verifier records durable
//! evidence for both branch and tree failures and returns `false`, which
//! is the orchestrator's contract for "do not spawn the mutation-capable
//! agent".

#![cfg(unix)]

use std::path::Path;

use symphony_workspace::{
    CHECK_BRANCH_MATCHES_REQUIRED, CHECK_CLEAN_TREE, CHECK_CWD_IN_WORKSPACE, CleanupPolicy,
    GitWorktreeClaimer, VerificationStatus,
};
use tempfile::TempDir;
use tokio::process::Command;

async fn init_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "test@symphony.invalid"],
        vec!["config", "user.name", "test"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        run_git(path, &args).await;
    }
    std::fs::write(path.join("README.md"), "initial").unwrap();
    run_git(path, &["add", "."]).await;
    run_git(path, &["commit", "-q", "-m", "init"]).await;
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

#[tokio::test]
async fn dirty_wrong_branch_workspace_blocks_agent_launch_with_evidence() {
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

    let mut claim = claimer.claim("ENG-1300", Some(1_300)).await.unwrap();
    assert_eq!(claim.branch.as_deref(), Some("symphony/ENG-1300"));

    run_git(&claim.path, &["checkout", "-q", "--detach"]).await;
    std::fs::write(claim.path.join("stray.txt"), "unowned change").unwrap();

    let launch_allowed = claim.verify_pre_launch(&claim.path.clone(), true).await;

    assert!(!launch_allowed, "agent launch must be blocked");
    assert_eq!(claim.verification.status, VerificationStatus::Failed);
    assert_eq!(
        claim
            .verification
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.status))
            .collect::<Vec<_>>(),
        vec![
            (CHECK_CWD_IN_WORKSPACE, VerificationStatus::Passed),
            (CHECK_BRANCH_MATCHES_REQUIRED, VerificationStatus::Failed),
            (CHECK_CLEAN_TREE, VerificationStatus::Failed),
        ]
    );
    let details = claim
        .verification
        .checks
        .iter()
        .filter_map(|check| check.detail.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(details.contains("symphony/ENG-1300"));
    assert!(details.contains("stray.txt"));
}
