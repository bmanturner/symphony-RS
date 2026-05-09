//! Git integration operations for the integration owner flow.
//!
//! SPEC v2 §5.10 names three merge strategies — `sequential_cherry_pick`,
//! `merge_commits`, and `shared_branch` — plus a `push` step against the
//! tracker's remote. Phase 8 of the v2 plan needs a single, focused
//! abstraction over those operations so the integration runner can:
//!
//! * cherry-pick the commits exclusive to a child branch onto a canonical
//!   integration branch in the integration owner's worktree;
//! * non-fast-forward merge a child branch into the integration branch
//!   when the workflow prefers branch-shape provenance over linear
//!   history;
//! * verify, before declaring integration done, that a shared-branch
//!   workflow's specialists and integration owner are actually on the
//!   same branch (because `shared_branch` consolidates by convention,
//!   not by merge);
//! * abort a half-applied merge / cherry-pick when the conflict policy
//!   chooses to bail out instead of repairing in place;
//! * push the integration branch to a configured remote so the PR
//!   adapter has something to point at.
//!
//! The abstraction is deliberately a *concrete struct* over the system
//! `git` CLI rather than a trait. Phase 8's later checklist items add
//! conflict / repair-loop coverage; once those land we can re-introduce
//! a trait if a fake is genuinely useful, but every consumer today calls
//! a real worktree on disk and the trait would only obscure the call
//! sites. Tests in this module exercise the real `git` CLI against
//! tempdir repositories, which is the authoritative behaviour the
//! orchestrator depends on.
//!
//! The struct does *not* run the Phase 6 pre-launch verifiers (cwd,
//! branch, clean tree) — those gates live on [`crate::WorkspaceClaim`]
//! and are the integration runner's job to invoke before reaching this
//! layer. Operating on whatever branch is currently checked out is the
//! correct contract: the claim already proved that branch is the right
//! one.

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::{WorkspaceError, WorkspaceResult};

/// Outcome of a [`GitIntegrationOps::merge`] attempt.
///
/// Success is split between [`Self::Clean`] (a real merge / fast-forward
/// produced new history) and [`Self::AlreadyUpToDate`] (git found
/// nothing to merge) so the integration runner can record an accurate
/// integration record per child branch — an "already up to date" merge
/// is a no-op for evidence purposes, not a normal apply.
///
/// [`Self::Conflict`] leaves the worktree in a conflicted state. The
/// caller is expected to either invoke [`GitIntegrationOps::abort_merge`]
/// before handing the work item to the conflict policy, or repair in
/// place when the policy is `integration_owner_repair_turn` (SPEC v2
/// §5.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Merge applied. `head` is the new HEAD commit on the integration
    /// branch (40-character hex SHA from `git rev-parse HEAD`).
    Clean { head: String },
    /// Source branch had no commits not already present in the
    /// integration branch.
    AlreadyUpToDate,
    /// Merge produced unresolved conflicts. `conflicted_paths` is the
    /// list reported by `git diff --name-only --diff-filter=U`.
    /// `stderr` is the trimmed stderr tail from `git merge` for
    /// operator diagnosis.
    Conflict {
        conflicted_paths: Vec<String>,
        stderr: String,
    },
}

/// Outcome of a [`GitIntegrationOps::cherry_pick`] attempt.
///
/// Cherry-pick range semantics match `git rev-list --reverse base..source`
/// — i.e. the commits exclusive to `source`, in chronological order. The
/// runner expects to record one [`MergeOutcome`]/`CherryPickOutcome` per
/// child it consolidates; an empty range therefore reports
/// [`Self::Empty`] rather than [`Self::Clean`] with an empty `picked`
/// list, so an integration record can distinguish "applied zero commits
/// because there was nothing to apply" from "applied N commits".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CherryPickOutcome {
    /// All commits in the range applied cleanly. `picked` lists the
    /// SHAs that were applied, in order.
    Clean { picked: Vec<String> },
    /// `base..source` was empty — nothing to apply.
    Empty,
    /// Cherry-pick stopped at a conflicting commit. `failed_commit` is
    /// the SHA git was applying (from `CHERRY_PICK_HEAD`).
    /// `applied_before_conflict` lists commits that were already
    /// applied before the conflicting one. `conflicted_paths` is the
    /// list from `git diff --name-only --diff-filter=U`.
    Conflict {
        failed_commit: String,
        applied_before_conflict: Vec<String>,
        conflicted_paths: Vec<String>,
        stderr: String,
    },
}

/// Outcome of a successful [`GitIntegrationOps::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    /// Remote name pushed to (e.g. `"origin"`).
    pub remote: String,
    /// Branch pushed (refs/heads/&lt;branch&gt; on the remote).
    pub branch: String,
    /// Trimmed stderr tail from `git push`. Captured rather than
    /// discarded because git's progress / "new branch" / "force update"
    /// messages all flow to stderr and are useful evidence for the
    /// integration record.
    pub remote_summary: String,
}

/// Abstraction over the integration-owner-relevant git operations.
///
/// One instance is bound to a single worktree path (the integration
/// owner's workspace claim) and reuses it across operations. The
/// struct holds no other state — every call shells out to `git` with
/// `-C <repo>` so the operating directory is unambiguous and there is
/// no hidden `chdir` behaviour to reason about.
#[derive(Debug, Clone)]
pub struct GitIntegrationOps {
    repo: PathBuf,
}

impl GitIntegrationOps {
    /// Bind operations to the worktree at `repo`.
    ///
    /// Does not validate the path: this struct is constructed from a
    /// [`crate::WorkspaceClaim`] whose claimer already verified the
    /// path is a directory, and re-validating here would just duplicate
    /// errors. A non-git path produces a [`WorkspaceError::Git`] on
    /// the first operation.
    pub fn new(repo: impl Into<PathBuf>) -> Self {
        Self { repo: repo.into() }
    }

    /// Worktree this instance operates against.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Non-fast-forward merge `source_branch` into the branch currently
    /// checked out in [`Self::repo`].
    ///
    /// Uses `--no-ff` so the integration owner's history records each
    /// child consolidation as a distinct merge commit, even when the
    /// source could have fast-forwarded. SPEC v2 §5.10 — the
    /// `merge_commits` strategy values branch-shape provenance.
    ///
    /// `message` is the merge commit subject when `Some`; when `None`,
    /// `--no-edit` lets git pick its default `Merge branch '...'`
    /// subject. The caller (integration runner) is encouraged to pass
    /// a templated subject that names the child issue so the resulting
    /// commit is greppable from the tracker side.
    ///
    /// On a successful merge, returns [`MergeOutcome::Clean`] with the
    /// new HEAD SHA. On `Already up to date`, returns
    /// [`MergeOutcome::AlreadyUpToDate`]. On conflict, leaves the
    /// worktree in conflicted state and returns
    /// [`MergeOutcome::Conflict`]; the caller must invoke
    /// [`Self::abort_merge`] or repair the conflict before further
    /// operations.
    pub async fn merge(
        &self,
        source_branch: &str,
        message: Option<&str>,
    ) -> WorkspaceResult<MergeOutcome> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.repo).args(["merge", "--no-ff"]);
        match message {
            Some(m) => {
                cmd.arg("-m").arg(m);
            }
            None => {
                cmd.arg("--no-edit");
            }
        }
        cmd.arg(source_branch);
        let out = cmd
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git merge: {e}")))?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        if out.status.success() {
            // git emits "Already up to date." (modern) or "Already
            // up-to-date." (older) on stdout when there is nothing to
            // merge. Pattern-match both spellings so we don't record a
            // bogus head SHA for a no-op.
            if stdout.contains("Already up to date") || stdout.contains("Already up-to-date") {
                return Ok(MergeOutcome::AlreadyUpToDate);
            }
            let head = self.rev_parse_head().await?;
            return Ok(MergeOutcome::Clean { head });
        }

        // Non-zero exit: conflict (most common), bad ref, or no such
        // branch. Distinguish conflict by the presence of unmerged
        // paths — that is the failure mode the conflict policy cares
        // about. Anything else is a hard error.
        let conflicted = self.unmerged_paths().await.unwrap_or_default();
        if conflicted.is_empty() {
            return Err(WorkspaceError::Git(format!(
                "git merge {source_branch} in {} failed ({}): {}",
                self.repo.display(),
                out.status,
                stderr.trim()
            )));
        }
        // git merge writes "CONFLICT (...)" lines to stdout, not
        // stderr. Combine both so the operator-facing detail isn't
        // empty when the conflict surface lives entirely on stdout.
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            format!("{}\n{}", stdout.trim(), stderr.trim())
        };
        Ok(MergeOutcome::Conflict {
            conflicted_paths: conflicted,
            stderr: detail,
        })
    }

    /// Cherry-pick the commits in `base_ref..source_branch` onto the
    /// branch currently checked out in [`Self::repo`], in chronological
    /// order.
    ///
    /// Resolution: `git rev-list --reverse <base_ref>..<source_branch>`
    /// determines the commits to apply. An empty range returns
    /// [`CherryPickOutcome::Empty`]. The cherry-pick itself uses
    /// `git cherry-pick --allow-empty <c1> <c2> …`; `--allow-empty`
    /// keeps a child whose merge commit is empty against the
    /// integration branch from poisoning the run.
    ///
    /// On conflict, identifies the conflicting commit via
    /// `git rev-parse CHERRY_PICK_HEAD` and the unmerged paths via
    /// `git diff --name-only --diff-filter=U`. The caller is expected
    /// to invoke [`Self::abort_cherry_pick`] or hand the worktree to a
    /// repair turn.
    pub async fn cherry_pick(
        &self,
        source_branch: &str,
        base_ref: &str,
    ) -> WorkspaceResult<CherryPickOutcome> {
        let range = format!("{base_ref}..{source_branch}");
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-list", "--reverse", &range])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git rev-list {range}: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git rev-list {range} in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let commits: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if commits.is_empty() {
            return Ok(CherryPickOutcome::Empty);
        }

        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo)
            .args(["cherry-pick", "--allow-empty"]);
        for c in &commits {
            cmd.arg(c);
        }
        let out = cmd
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git cherry-pick: {e}")))?;

        if out.status.success() {
            return Ok(CherryPickOutcome::Clean { picked: commits });
        }

        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stdout_msg = String::from_utf8_lossy(&out.stdout).to_string();
        let conflicted = self.unmerged_paths().await.unwrap_or_default();
        let failed_commit = self
            .rev_parse_ref("CHERRY_PICK_HEAD")
            .await
            .ok()
            .unwrap_or_default();

        if conflicted.is_empty() && failed_commit.is_empty() {
            return Err(WorkspaceError::Git(format!(
                "git cherry-pick {} in {} failed ({}): {}",
                commits.join(" "),
                self.repo.display(),
                out.status,
                stderr.trim()
            )));
        }

        // Commits applied before the conflict are everything strictly
        // before `failed_commit` in `commits`. If we couldn't read
        // CHERRY_PICK_HEAD, conservatively report none.
        let applied_before_conflict: Vec<String> = if failed_commit.is_empty() {
            Vec::new()
        } else {
            commits
                .iter()
                .take_while(|c| **c != failed_commit)
                .cloned()
                .collect()
        };

        // As with merge, cherry-pick writes "CONFLICT (...)" lines to
        // stdout, so combine both streams for the diagnostic detail.
        let detail = if stderr.trim().is_empty() {
            stdout_msg.trim().to_string()
        } else if stdout_msg.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            format!("{}\n{}", stdout_msg.trim(), stderr.trim())
        };
        Ok(CherryPickOutcome::Conflict {
            failed_commit,
            applied_before_conflict,
            conflicted_paths: conflicted,
            stderr: detail,
        })
    }

    /// Verify that the worktree at [`Self::repo`] is currently checked
    /// out on `branch` — the SPEC v2 §5.10 `shared_branch` precondition.
    ///
    /// `shared_branch` consolidates by convention: specialists and the
    /// integration owner all run on the same branch, so there is
    /// nothing to merge. The integration runner still needs a *check*
    /// that everyone actually landed there, otherwise the workflow's
    /// invariant is silently broken. This is that check.
    ///
    /// Returns `Ok(())` on match; [`WorkspaceError::Git`] on mismatch
    /// (including detached HEAD, which reports as `"HEAD"`).
    pub async fn verify_shared_branch(&self, branch: &str) -> WorkspaceResult<()> {
        let head = self.current_branch().await?;
        if head == branch {
            Ok(())
        } else {
            Err(WorkspaceError::Git(format!(
                "shared-branch verification: worktree {} is on branch {:?} but expected {:?}",
                self.repo.display(),
                head,
                branch
            )))
        }
    }

    /// Push `branch` from [`Self::repo`] to `remote`.
    ///
    /// Plain `git push <remote> <branch>` — no `--force`, no
    /// `--force-with-lease`. The integration runner is expected to
    /// pre-resolve any rewrite hazards (e.g. a re-cherry-pick after a
    /// repair turn) before this call; pushing is the final step that
    /// makes integration visible to the PR adapter, and it should
    /// fail loudly if the remote refuses the update.
    pub async fn push(&self, remote: &str, branch: &str) -> WorkspaceResult<PushOutcome> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["push", remote, branch])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git push: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git push {remote} {branch} from {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(PushOutcome {
            remote: remote.to_string(),
            branch: branch.to_string(),
            remote_summary: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }

    /// Abort an in-progress merge. Idempotent-ish: if there is no
    /// merge in progress git exits non-zero and we surface that as
    /// [`WorkspaceError::Git`] so the caller learns its expectation
    /// was wrong rather than silently swallowing.
    pub async fn abort_merge(&self) -> WorkspaceResult<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["merge", "--abort"])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git merge --abort: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git merge --abort in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Abort an in-progress cherry-pick. Same loud-failure semantics
    /// as [`Self::abort_merge`].
    pub async fn abort_cherry_pick(&self) -> WorkspaceResult<()> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["cherry-pick", "--abort"])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git cherry-pick --abort: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git cherry-pick --abort in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    async fn rev_parse_head(&self) -> WorkspaceResult<String> {
        self.rev_parse_ref("HEAD").await
    }

    async fn rev_parse_ref(&self, refname: &str) -> WorkspaceResult<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-parse", refname])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git rev-parse {refname}: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git rev-parse {refname} in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn current_branch(&self) -> WorkspaceResult<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git rev-parse: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git rev-parse --abbrev-ref HEAD in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    async fn unmerged_paths(&self) -> WorkspaceResult<Vec<String>> {
        let out = Command::new("git")
            .arg("-C")
            .arg(&self.repo)
            .args(["diff", "--name-only", "--diff-filter=U"])
            .output()
            .await
            .map_err(|e| WorkspaceError::Git(format!("spawn git diff: {e}")))?;
        if !out.status.success() {
            return Err(WorkspaceError::Git(format!(
                "git diff --diff-filter=U in {} exited {}: {}",
                self.repo.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    async fn run(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }

    async fn run_in(dir: &Path, prog: &str, args: &[&str]) {
        let out = Command::new(prog)
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("spawn");
        assert!(out.status.success(), "{prog} {args:?} failed: {out:?}");
    }

    async fn init_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@symphony.invalid"],
            vec!["config", "user.name", "test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            run(dir.path(), &args).await;
        }
        std::fs::write(dir.path().join("README.md"), "x").unwrap();
        run(dir.path(), &["add", "."]).await;
        run(dir.path(), &["commit", "-q", "-m", "init"]).await;
        dir
    }

    async fn commit_file(repo: &Path, path: &str, content: &str, msg: &str) {
        std::fs::write(repo.join(path), content).unwrap();
        run(repo, &["add", path]).await;
        run(repo, &["commit", "-q", "-m", msg]).await;
    }

    async fn checkout_new(repo: &Path, branch: &str, from: &str) {
        run(repo, &["checkout", "-q", "-b", branch, from]).await;
    }

    async fn checkout(repo: &Path, branch: &str) {
        run(repo, &["checkout", "-q", branch]).await;
    }

    async fn current_branch(repo: &Path) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .await
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    // ----- merge ------------------------------------------------------------

    #[tokio::test]
    async fn merge_clean_records_new_head() {
        let dir = init_repo().await;
        let repo = dir.path();
        checkout_new(repo, "feature", "main").await;
        commit_file(repo, "a.txt", "a", "feat a").await;
        checkout(repo, "main").await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops
            .merge("feature", Some("merge feature into main"))
            .await
            .unwrap();
        match outcome {
            MergeOutcome::Clean { head } => {
                assert_eq!(head.len(), 40, "expected 40-char SHA, got {head:?}");
                // Working tree must contain the merged file.
                assert!(repo.join("a.txt").exists());
            }
            other => panic!("expected Clean, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_already_up_to_date_when_source_in_target() {
        let dir = init_repo().await;
        let repo = dir.path();
        // Branch with no extra commits beyond main.
        checkout_new(repo, "feature", "main").await;
        checkout(repo, "main").await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.merge("feature", None).await.unwrap();
        assert_eq!(outcome, MergeOutcome::AlreadyUpToDate);
    }

    #[tokio::test]
    async fn merge_conflict_lists_unmerged_paths() {
        let dir = init_repo().await;
        let repo = dir.path();
        // main edits shared.txt one way.
        commit_file(repo, "shared.txt", "main\n", "main side").await;
        // feature edits shared.txt another way from the same parent.
        run(repo, &["checkout", "-q", "-b", "feature", "HEAD~1"]).await;
        commit_file(repo, "shared.txt", "feature\n", "feature side").await;
        checkout(repo, "main").await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.merge("feature", None).await.unwrap();
        match outcome {
            MergeOutcome::Conflict {
                conflicted_paths,
                stderr,
            } => {
                assert_eq!(conflicted_paths, vec!["shared.txt".to_string()]);
                assert!(!stderr.is_empty(), "stderr should carry conflict detail");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        // Abort cleans up so subsequent ops can run.
        ops.abort_merge().await.unwrap();
        assert_eq!(current_branch(repo).await, "main");
    }

    #[tokio::test]
    async fn merge_unknown_branch_returns_git_error() {
        let dir = init_repo().await;
        let ops = GitIntegrationOps::new(dir.path());
        let err = ops.merge("does-not-exist", None).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }

    // ----- cherry_pick ------------------------------------------------------

    #[tokio::test]
    async fn cherry_pick_applies_range_in_order() {
        let dir = init_repo().await;
        let repo = dir.path();
        checkout_new(repo, "feature", "main").await;
        commit_file(repo, "a.txt", "a", "feat a").await;
        commit_file(repo, "b.txt", "b", "feat b").await;
        checkout(repo, "main").await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.cherry_pick("feature", "main").await.unwrap();
        match outcome {
            CherryPickOutcome::Clean { picked } => {
                assert_eq!(picked.len(), 2, "expected two commits, got {picked:?}");
                assert!(repo.join("a.txt").exists());
                assert!(repo.join("b.txt").exists());
            }
            other => panic!("expected Clean, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cherry_pick_empty_range_reports_empty() {
        let dir = init_repo().await;
        let repo = dir.path();
        checkout_new(repo, "feature", "main").await;
        checkout(repo, "main").await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.cherry_pick("feature", "main").await.unwrap();
        assert_eq!(outcome, CherryPickOutcome::Empty);
    }

    #[tokio::test]
    async fn cherry_pick_conflict_identifies_failing_commit_and_paths() {
        let dir = init_repo().await;
        let repo = dir.path();
        // main writes shared.txt one way.
        commit_file(repo, "shared.txt", "main\n", "main side").await;
        // feature: branch from main's parent so the cherry-pick's
        // commits conflict with main's "main side" commit.
        run(repo, &["checkout", "-q", "-b", "feature", "HEAD~1"]).await;
        commit_file(repo, "ok.txt", "ok", "ok").await; // applies cleanly
        commit_file(repo, "shared.txt", "feature\n", "feature side").await; // conflicts

        // Capture the SHA of the conflicting commit.
        let sha_out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        let conflict_sha = String::from_utf8(sha_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        checkout(repo, "main").await;
        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.cherry_pick("feature", "main").await.unwrap();
        match outcome {
            CherryPickOutcome::Conflict {
                failed_commit,
                applied_before_conflict,
                conflicted_paths,
                stderr,
            } => {
                assert_eq!(failed_commit, conflict_sha);
                assert_eq!(
                    applied_before_conflict.len(),
                    1,
                    "the ok.txt commit should have applied before conflict"
                );
                assert_eq!(conflicted_paths, vec!["shared.txt".to_string()]);
                assert!(!stderr.is_empty());
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        ops.abort_cherry_pick().await.unwrap();
        assert_eq!(current_branch(repo).await, "main");
    }

    // ----- shared-branch verification --------------------------------------

    #[tokio::test]
    async fn verify_shared_branch_passes_when_head_matches() {
        let dir = init_repo().await;
        let ops = GitIntegrationOps::new(dir.path());
        ops.verify_shared_branch("main").await.unwrap();
    }

    #[tokio::test]
    async fn verify_shared_branch_fails_on_mismatch() {
        let dir = init_repo().await;
        let repo = dir.path();
        checkout_new(repo, "feature", "main").await;
        let ops = GitIntegrationOps::new(repo);
        let err = ops.verify_shared_branch("integration").await.unwrap_err();
        match err {
            WorkspaceError::Git(msg) => {
                assert!(
                    msg.contains("feature") && msg.contains("integration"),
                    "{msg}"
                );
            }
            _ => panic!("expected Git error"),
        }
    }

    #[tokio::test]
    async fn verify_shared_branch_fails_on_detached_head() {
        let dir = init_repo().await;
        let repo = dir.path();
        let sha = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        let sha = String::from_utf8(sha.stdout).unwrap().trim().to_string();
        run(repo, &["checkout", "-q", "--detach", &sha]).await;

        let ops = GitIntegrationOps::new(repo);
        let err = ops.verify_shared_branch("main").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }

    // ----- push -------------------------------------------------------------

    #[tokio::test]
    async fn push_updates_bare_remote() {
        let dir = init_repo().await;
        let repo = dir.path();
        let remote_dir = TempDir::new().unwrap();
        // Create a bare repo to act as the remote.
        run_in(
            remote_dir.path(),
            "git",
            &["init", "-q", "--bare", "-b", "main"],
        )
        .await;
        run(
            repo,
            &[
                "remote",
                "add",
                "origin",
                remote_dir.path().to_str().unwrap(),
            ],
        )
        .await;

        let ops = GitIntegrationOps::new(repo);
        let outcome = ops.push("origin", "main").await.unwrap();
        assert_eq!(outcome.remote, "origin");
        assert_eq!(outcome.branch, "main");

        // Remote now has main pointing somewhere — verify by
        // rev-parsing the remote ref.
        let out = Command::new("git")
            .arg("-C")
            .arg(remote_dir.path())
            .args(["rev-parse", "main"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "remote main should exist: {out:?}");
    }

    #[tokio::test]
    async fn push_to_unknown_remote_returns_git_error() {
        let dir = init_repo().await;
        let ops = GitIntegrationOps::new(dir.path());
        let err = ops.push("nope", "main").await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }

    // ----- abort idempotence ------------------------------------------------

    #[tokio::test]
    async fn abort_merge_without_in_progress_merge_errors() {
        let dir = init_repo().await;
        let ops = GitIntegrationOps::new(dir.path());
        let err = ops.abort_merge().await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }

    #[tokio::test]
    async fn abort_cherry_pick_without_in_progress_pick_errors() {
        let dir = init_repo().await;
        let ops = GitIntegrationOps::new(dir.path());
        let err = ops.abort_cherry_pick().await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }

    #[tokio::test]
    async fn ops_on_non_git_path_errors() {
        let tmp = TempDir::new().unwrap();
        let ops = GitIntegrationOps::new(tmp.path());
        let err = ops.merge("main", None).await.unwrap_err();
        assert!(matches!(err, WorkspaceError::Git(_)));
    }
}
