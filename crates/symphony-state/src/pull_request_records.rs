//! Persistence for the `pull_request_records` table (SPEC v2 §5.11).
//!
//! One row per material PR change observed by the integration owner:
//! draft open, update, mark-ready, merge, close. Earlier rows are
//! preserved as audit; the parent-closeout gate and the QA brief read
//! the latest row.
//!
//! The storage layer mirrors the convention established by
//! [`crate::integration_records`]:
//!
//! * `provider` and `state` stay stringly-typed even though SQL `CHECK`
//!   constraints reject unknown values. The kernel re-validates against
//!   the typed enums in `symphony-core::pull_request` before insert.
//! * No `serde_json` dependency in the storage crate — the canonical
//!   encoder lives in `symphony-core`.

use rusqlite::{Connection, OptionalExtension, params};

use crate::integration_records::IntegrationRecordId;
use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateResult};

/// Strongly-typed primary key for `pull_request_records`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PullRequestRecordId(pub i64);

/// A `pull_request_records` row as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRecordRow {
    /// Primary key.
    pub id: PullRequestRecordId,
    /// Parent work item the PR is for (typically a decomposed parent).
    pub parent_work_item_id: WorkItemId,
    /// Integration-owner role name (e.g. `platform_lead`).
    pub owner_role: String,
    /// Integration-owner run that produced or is producing the row.
    /// Nullable because the kernel may queue a PR-open task before
    /// attributing it to a concrete run; the FK is `ON DELETE SET NULL`
    /// so the row survives run garbage-collection (mirrors
    /// `integration_records`).
    pub run_id: Option<RunId>,
    /// Integration record this PR consolidates from. Nullable because
    /// rare workflows that bypass the integration owner produce PRs
    /// directly; otherwise this links the PR back to the consolidation
    /// attempt that produced its head branch.
    pub integration_record_id: Option<IntegrationRecordId>,
    /// Forge adapter (only `github` today). Migration enforces the
    /// variant set with a SQL `CHECK`.
    pub provider: String,
    /// Lifecycle state (`draft`, `ready`, `merged`, `closed`).
    /// Migration enforces the variant set with a SQL `CHECK`.
    pub state: String,
    /// Forge-assigned PR number, when known.
    pub number: Option<i64>,
    /// Canonical PR URL on the forge.
    pub url: Option<String>,
    /// Head ref the PR proposes to merge — i.e. the canonical
    /// integration branch.
    pub head_branch: String,
    /// Base ref the PR targets (e.g. `main`).
    pub base_branch: Option<String>,
    /// Head SHA at record time, when known.
    pub head_sha: Option<String>,
    /// Templated PR title rendered at record time.
    pub title: String,
    /// Rendered PR body, when present.
    pub body: Option<String>,
    /// CI/check rollup string captured by the integration owner. Stored
    /// opaquely so the schema does not need to evolve every time CI
    /// vocabulary changes upstream.
    pub ci_status: Option<String>,
    /// RFC3339 row creation timestamp.
    pub created_at: String,
}

/// Insertion payload for [`PullRequestRecordRepository::create_pull_request_record`].
///
/// Mirrors the storage row exactly; the caller is responsible for
/// rendering any templated strings before insert.
#[derive(Debug, Clone)]
pub struct NewPullRequestRecord<'a> {
    /// Parent work item the PR is for.
    pub parent_work_item_id: WorkItemId,
    /// Integration-owner role name.
    pub owner_role: &'a str,
    /// Integration-owner run, if attributed.
    pub run_id: Option<RunId>,
    /// Integration record this PR consolidates from.
    pub integration_record_id: Option<IntegrationRecordId>,
    /// Forge adapter.
    pub provider: &'a str,
    /// Lifecycle state.
    pub state: &'a str,
    /// Forge-assigned PR number, when known.
    pub number: Option<i64>,
    /// Canonical PR URL on the forge.
    pub url: Option<&'a str>,
    /// Head ref the PR proposes to merge.
    pub head_branch: &'a str,
    /// Base ref the PR targets.
    pub base_branch: Option<&'a str>,
    /// Head SHA at record time.
    pub head_sha: Option<&'a str>,
    /// Rendered PR title.
    pub title: &'a str,
    /// Rendered PR body.
    pub body: Option<&'a str>,
    /// CI/check rollup string.
    pub ci_status: Option<&'a str>,
    /// RFC3339 row creation timestamp.
    pub now: &'a str,
}

/// CRUD over `pull_request_records`.
pub trait PullRequestRecordRepository {
    /// Insert a new pull-request record and return its persisted form.
    ///
    /// Foreign-key violations (unknown `parent_work_item_id`, `run_id`,
    /// or `integration_record_id`) and SQL `CHECK` violations (unknown
    /// `provider` / `state`) surface as [`crate::StateError::Sqlite`].
    fn create_pull_request_record(
        &mut self,
        new: NewPullRequestRecord<'_>,
    ) -> StateResult<PullRequestRecordRow>;

    /// Fetch a single record by primary key.
    fn get_pull_request_record(
        &self,
        id: PullRequestRecordId,
    ) -> StateResult<Option<PullRequestRecordRow>>;

    /// List every record filed against `parent_work_item_id`, oldest
    /// first — the same audit-trail order [`crate::integration_records`]
    /// uses.
    fn list_pull_request_records_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Vec<PullRequestRecordRow>>;

    /// Return the most recent record filed against `parent_work_item_id`,
    /// or `None` if none exist. This is what the parent-closeout gate
    /// reads: the latest row's `state` decides whether the PR has merged.
    fn latest_pull_request_record_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Option<PullRequestRecordRow>>;
}

const PULL_REQUEST_RECORD_COLUMNS: &str = "id, parent_work_item_id, owner_role, run_id, \
     integration_record_id, provider, state, number, url, head_branch, \
     base_branch, head_sha, title, body, ci_status, created_at";

fn map_pull_request_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<PullRequestRecordRow> {
    Ok(PullRequestRecordRow {
        id: PullRequestRecordId(row.get(0)?),
        parent_work_item_id: WorkItemId(row.get(1)?),
        owner_role: row.get(2)?,
        run_id: row.get::<_, Option<i64>>(3)?.map(RunId),
        integration_record_id: row.get::<_, Option<i64>>(4)?.map(IntegrationRecordId),
        provider: row.get(5)?,
        state: row.get(6)?,
        number: row.get(7)?,
        url: row.get(8)?,
        head_branch: row.get(9)?,
        base_branch: row.get(10)?,
        head_sha: row.get(11)?,
        title: row.get(12)?,
        body: row.get(13)?,
        ci_status: row.get(14)?,
        created_at: row.get(15)?,
    })
}

pub(crate) fn create_pull_request_record_in(
    conn: &Connection,
    new: NewPullRequestRecord<'_>,
) -> StateResult<PullRequestRecordRow> {
    conn.execute(
        "INSERT INTO pull_request_records \
            (parent_work_item_id, owner_role, run_id, integration_record_id, \
             provider, state, number, url, head_branch, base_branch, \
             head_sha, title, body, ci_status, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            new.parent_work_item_id.0,
            new.owner_role,
            new.run_id.map(|r| r.0),
            new.integration_record_id.map(|i| i.0),
            new.provider,
            new.state,
            new.number,
            new.url,
            new.head_branch,
            new.base_branch,
            new.head_sha,
            new.title,
            new.body,
            new.ci_status,
            new.now,
        ],
    )?;
    let id = PullRequestRecordId(conn.last_insert_rowid());
    Ok(get_pull_request_record_in(conn, id)?
        .expect("freshly inserted pull request record must be readable"))
}

pub(crate) fn get_pull_request_record_in(
    conn: &Connection,
    id: PullRequestRecordId,
) -> StateResult<Option<PullRequestRecordRow>> {
    let sql =
        format!("SELECT {PULL_REQUEST_RECORD_COLUMNS} FROM pull_request_records WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![id.0], map_pull_request_record)
        .optional()?;
    Ok(row)
}

pub(crate) fn list_pull_request_records_for_parent_in(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
) -> StateResult<Vec<PullRequestRecordRow>> {
    let sql = format!(
        "SELECT {PULL_REQUEST_RECORD_COLUMNS} FROM pull_request_records \
         WHERE parent_work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![parent_work_item_id.0], map_pull_request_record)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn latest_pull_request_record_for_parent_in(
    conn: &Connection,
    parent_work_item_id: WorkItemId,
) -> StateResult<Option<PullRequestRecordRow>> {
    let sql = format!(
        "SELECT {PULL_REQUEST_RECORD_COLUMNS} FROM pull_request_records \
         WHERE parent_work_item_id = ?1 ORDER BY id DESC LIMIT 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let row = stmt
        .query_row(params![parent_work_item_id.0], map_pull_request_record)
        .optional()?;
    Ok(row)
}

impl PullRequestRecordRepository for StateDb {
    fn create_pull_request_record(
        &mut self,
        new: NewPullRequestRecord<'_>,
    ) -> StateResult<PullRequestRecordRow> {
        create_pull_request_record_in(self.conn(), new)
    }

    fn get_pull_request_record(
        &self,
        id: PullRequestRecordId,
    ) -> StateResult<Option<PullRequestRecordRow>> {
        get_pull_request_record_in(self.conn(), id)
    }

    fn list_pull_request_records_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Vec<PullRequestRecordRow>> {
        list_pull_request_records_for_parent_in(self.conn(), parent_work_item_id)
    }

    fn latest_pull_request_record_for_parent(
        &self,
        parent_work_item_id: WorkItemId,
    ) -> StateResult<Option<PullRequestRecordRow>> {
        latest_pull_request_record_for_parent_in(self.conn(), parent_work_item_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integration_records::{IntegrationRecordRepository, NewIntegrationRecord};
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed(db: &mut StateDb, identifier: &str) -> (WorkItemId, RunId, IntegrationRecordId) {
        let now = "2026-05-08T00:00:00Z";
        let wi = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier,
                parent_id: None,
                title: "broad parent",
                status_class: "integration",
                tracker_status: "in_progress",
                assigned_role: Some("platform_lead"),
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now,
            })
            .expect("wi");
        let run = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now,
            })
            .expect("run");
        let integration = db
            .create_integration_record(NewIntegrationRecord {
                parent_work_item_id: wi.id,
                owner_role: "platform_lead",
                run_id: Some(run.id),
                status: "succeeded",
                merge_strategy: "merge_commits",
                integration_branch: Some("symphony/integration/X"),
                base_ref: Some("main"),
                head_sha: Some("deadbeef"),
                workspace_path: None,
                children_consolidated: Some("[]"),
                summary: Some("done"),
                conflicts: Some("[]"),
                blockers_filed: Some("[]"),
                now,
            })
            .expect("integration");
        (wi.id, run.id, integration.id)
    }

    fn draft_payload<'a>(parent: WorkItemId, now: &'a str) -> NewPullRequestRecord<'a> {
        NewPullRequestRecord {
            parent_work_item_id: parent,
            owner_role: "platform_lead",
            run_id: None,
            integration_record_id: None,
            provider: "github",
            state: "draft",
            number: None,
            url: None,
            head_branch: "symphony/integration/X",
            base_branch: Some("main"),
            head_sha: None,
            title: "X: integrate consolidation",
            body: None,
            ci_status: None,
            now,
        }
    }

    #[test]
    fn create_then_get_round_trips_all_columns() {
        let mut db = open();
        let (parent, run, integration) = seed(&mut db, "OWNER/REPO#42");

        let inserted = db
            .create_pull_request_record(NewPullRequestRecord {
                run_id: Some(run),
                integration_record_id: Some(integration),
                state: "merged",
                number: Some(42),
                url: Some("https://github.com/o/r/pull/42"),
                head_sha: Some("cafef00d"),
                body: Some("body"),
                ci_status: Some("success"),
                ..draft_payload(parent, "2026-05-08T00:01:00Z")
            })
            .expect("create");

        let fetched = db
            .get_pull_request_record(inserted.id)
            .expect("get")
            .expect("present");
        assert_eq!(fetched, inserted);
        assert_eq!(fetched.parent_work_item_id, parent);
        assert_eq!(fetched.run_id, Some(run));
        assert_eq!(fetched.integration_record_id, Some(integration));
        assert_eq!(fetched.state, "merged");
        assert_eq!(fetched.number, Some(42));
        assert_eq!(
            fetched.url.as_deref(),
            Some("https://github.com/o/r/pull/42")
        );
        assert_eq!(fetched.created_at, "2026-05-08T00:01:00Z");
    }

    #[test]
    fn draft_record_allows_null_optional_columns() {
        let mut db = open();
        let (parent, _, _) = seed(&mut db, "OWNER/REPO#1");
        let inserted = db
            .create_pull_request_record(draft_payload(parent, "2026-05-08T00:00:01Z"))
            .expect("create draft");
        assert!(inserted.run_id.is_none());
        assert!(inserted.integration_record_id.is_none());
        assert!(inserted.number.is_none());
        assert!(inserted.url.is_none());
        assert!(inserted.head_sha.is_none());
        assert!(inserted.body.is_none());
        assert!(inserted.ci_status.is_none());
    }

    #[test]
    fn create_rejects_orphan_parent_work_item() {
        let mut db = open();
        let err = db
            .create_pull_request_record(draft_payload(WorkItemId(9_999), "2026-05-08T00:00:00Z"))
            .expect_err("orphan parent must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_orphan_run_reference() {
        let mut db = open();
        let (parent, _, _) = seed(&mut db, "OWNER/REPO#2");
        let err = db
            .create_pull_request_record(NewPullRequestRecord {
                run_id: Some(RunId(9_999)),
                ..draft_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("orphan run must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_orphan_integration_reference() {
        let mut db = open();
        let (parent, _, _) = seed(&mut db, "OWNER/REPO#3");
        let err = db
            .create_pull_request_record(NewPullRequestRecord {
                integration_record_id: Some(IntegrationRecordId(9_999)),
                ..draft_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("orphan integration must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_unknown_state() {
        let mut db = open();
        let (parent, _, _) = seed(&mut db, "OWNER/REPO#4");
        let err = db
            .create_pull_request_record(NewPullRequestRecord {
                state: "maybe",
                ..draft_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("unknown state must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn create_rejects_unknown_provider() {
        let mut db = open();
        let (parent, _, _) = seed(&mut db, "OWNER/REPO#5");
        let err = db
            .create_pull_request_record(NewPullRequestRecord {
                provider: "gitlab",
                ..draft_payload(parent, "2026-05-08T00:00:00Z")
            })
            .expect_err("unknown provider must fail");
        assert!(matches!(err, crate::StateError::Sqlite(_)));
    }

    #[test]
    fn list_returns_records_for_parent_in_creation_order() {
        let mut db = open();
        let (parent, run, _) = seed(&mut db, "ENG-1");
        let (other_parent, _, _) = seed(&mut db, "ENG-2");

        let first = db
            .create_pull_request_record(draft_payload(parent, "2026-05-08T00:00:01Z"))
            .unwrap();
        let _other = db
            .create_pull_request_record(draft_payload(other_parent, "2026-05-08T00:00:02Z"))
            .unwrap();
        let second = db
            .create_pull_request_record(NewPullRequestRecord {
                run_id: Some(run),
                state: "ready",
                number: Some(1),
                url: Some("https://github.com/o/r/pull/1"),
                ..draft_payload(parent, "2026-05-08T00:00:03Z")
            })
            .unwrap();

        let listed = db
            .list_pull_request_records_for_parent(parent)
            .expect("list");
        let ids: Vec<_> = listed.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![first.id, second.id]);
    }

    #[test]
    fn list_returns_empty_for_unknown_parent() {
        let db = open();
        assert!(
            db.list_pull_request_records_for_parent(WorkItemId(404))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn latest_returns_most_recent_record() {
        let mut db = open();
        let (parent, run, _) = seed(&mut db, "ENG-3");
        assert!(
            db.latest_pull_request_record_for_parent(parent)
                .unwrap()
                .is_none()
        );

        let _first = db
            .create_pull_request_record(draft_payload(parent, "2026-05-08T00:00:01Z"))
            .unwrap();
        let last = db
            .create_pull_request_record(NewPullRequestRecord {
                run_id: Some(run),
                state: "merged",
                number: Some(7),
                url: Some("https://github.com/o/r/pull/7"),
                ..draft_payload(parent, "2026-05-08T00:00:02Z")
            })
            .unwrap();

        let latest = db
            .latest_pull_request_record_for_parent(parent)
            .expect("latest")
            .expect("present");
        assert_eq!(latest.id, last.id);
        assert_eq!(latest.state, "merged");
    }

    #[test]
    fn record_run_and_integration_clear_when_referenced_rows_deleted() {
        // Both `run_id` and `integration_record_id` use ON DELETE SET NULL
        // so the durable PR record outlives reaped runs and recycled
        // integration attempts.
        let mut db = open();
        let (parent, run, integration) = seed(&mut db, "ENG-4");
        let inserted = db
            .create_pull_request_record(NewPullRequestRecord {
                run_id: Some(run),
                integration_record_id: Some(integration),
                state: "ready",
                number: Some(2),
                url: Some("https://github.com/o/r/pull/2"),
                ..draft_payload(parent, "2026-05-08T00:00:01Z")
            })
            .unwrap();

        db.conn()
            .execute("DELETE FROM runs WHERE id = ?1", params![run.0])
            .expect("delete run");
        db.conn()
            .execute(
                "DELETE FROM integration_records WHERE id = ?1",
                params![integration.0],
            )
            .expect("delete integration");

        let after = db
            .get_pull_request_record(inserted.id)
            .unwrap()
            .expect("record survives");
        assert!(after.run_id.is_none());
        assert!(after.integration_record_id.is_none());
    }
}
