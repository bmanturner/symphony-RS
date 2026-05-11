//! Persistence for durable follow-up issue requests.
//!
//! v5 Phase 7.5 needs a real state-layer home for
//! [`symphony_core::FollowupIssueRequest`]: direct-create follow-ups must
//! stop fabricating immediate tracker work items, and approval-routed
//! follow-ups must survive long enough for a later filing pass to push
//! them upstream.

use rusqlite::{Connection, OptionalExtension, params};
use symphony_core::{
    BlockerOrigin, FollowupId, FollowupIssueRequest, FollowupPolicy, FollowupStatus, RoleName,
    RunRef,
};

use crate::repository::{RunId, WorkItemId};
use crate::{StateDb, StateError, StateResult};

/// Stored `followups` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowupRecord {
    pub id: FollowupId,
    pub source_work_item_id: WorkItemId,
    pub title: String,
    pub reason: String,
    pub scope: String,
    pub acceptance_criteria: String,
    pub blocking: bool,
    pub policy: String,
    pub origin_kind: String,
    pub origin_run_id: Option<RunId>,
    pub origin_role: Option<String>,
    pub origin_agent_profile: Option<String>,
    pub origin_human_actor: Option<String>,
    pub status: String,
    pub created_issue_id: Option<WorkItemId>,
    pub approval_role: Option<String>,
    pub approval_note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl FollowupRecord {
    /// Rebuild the domain record from the stored row.
    pub fn to_domain(&self) -> StateResult<FollowupIssueRequest> {
        let acceptance_criteria: Vec<String> =
            serde_json::from_str(&self.acceptance_criteria).map_err(|err| {
                StateError::Invariant(format!(
                    "followup {} has invalid acceptance_criteria JSON: {err}",
                    self.id
                ))
            })?;
        let policy = match self.policy.as_str() {
            "create_directly" => FollowupPolicy::CreateDirectly,
            "propose_for_approval" => FollowupPolicy::ProposeForApproval,
            other => {
                return Err(StateError::Invariant(format!(
                    "followup {} has unknown policy `{other}`",
                    self.id
                )));
            }
        };
        let status = match self.status.as_str() {
            "proposed" => FollowupStatus::Proposed,
            "approved" => FollowupStatus::Approved,
            "created" => FollowupStatus::Created,
            "rejected" => FollowupStatus::Rejected,
            "cancelled" => FollowupStatus::Cancelled,
            other => {
                return Err(StateError::Invariant(format!(
                    "followup {} has unknown status `{other}`",
                    self.id
                )));
            }
        };
        let origin = match self.origin_kind.as_str() {
            "run" => BlockerOrigin::Run {
                run_id: RunRef::new(
                    self.origin_run_id
                        .ok_or_else(|| {
                            StateError::Invariant(format!(
                                "followup {} origin_kind=run missing origin_run_id",
                                self.id
                            ))
                        })?
                        .0,
                ),
                role: self.origin_role.clone().map(RoleName::new),
            },
            "agent" => BlockerOrigin::Agent {
                role: self
                    .origin_role
                    .as_ref()
                    .cloned()
                    .map(RoleName::new)
                    .ok_or_else(|| {
                        StateError::Invariant(format!(
                            "followup {} origin_kind=agent missing origin_role",
                            self.id
                        ))
                    })?,
                agent_profile: self.origin_agent_profile.clone(),
            },
            "human" => BlockerOrigin::Human {
                actor: self.origin_human_actor.clone().ok_or_else(|| {
                    StateError::Invariant(format!(
                        "followup {} origin_kind=human missing origin_human_actor",
                        self.id
                    ))
                })?,
            },
            other => {
                return Err(StateError::Invariant(format!(
                    "followup {} has unknown origin_kind `{other}`",
                    self.id
                )));
            }
        };

        Ok(FollowupIssueRequest {
            id: self.id,
            source_work_item: symphony_core::WorkItemId::new(self.source_work_item_id.0),
            title: self.title.clone(),
            reason: self.reason.clone(),
            scope: self.scope.clone(),
            acceptance_criteria,
            blocking: self.blocking,
            policy,
            origin,
            status,
            created_issue_id: self
                .created_issue_id
                .map(|id| symphony_core::WorkItemId::new(id.0)),
            approval_role: self.approval_role.clone().map(RoleName::new),
            approval_note: self.approval_note.clone(),
        })
    }
}

/// Insert payload for [`FollowupRepository::create_followup`].
#[derive(Debug, Clone)]
pub struct NewFollowup<'a> {
    pub source_work_item_id: WorkItemId,
    pub title: &'a str,
    pub reason: &'a str,
    pub scope: &'a str,
    pub acceptance_criteria: &'a str,
    pub blocking: bool,
    pub policy: &'a str,
    pub origin_kind: &'a str,
    pub origin_run_id: Option<RunId>,
    pub origin_role: Option<&'a str>,
    pub origin_agent_profile: Option<&'a str>,
    pub origin_human_actor: Option<&'a str>,
    pub status: &'a str,
    pub created_issue_id: Option<WorkItemId>,
    pub approval_role: Option<&'a str>,
    pub approval_note: Option<&'a str>,
    pub now: &'a str,
}

/// Read/write access to the `followups` table.
pub trait FollowupRepository {
    fn create_followup(&mut self, new: NewFollowup<'_>) -> StateResult<FollowupRecord>;
    fn get_followup(&self, id: FollowupId) -> StateResult<Option<FollowupRecord>>;
    fn list_followups_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<FollowupRecord>>;
    fn list_followups_by_status(&self, status: &str) -> StateResult<Vec<FollowupRecord>>;
    fn mark_followup_created(
        &mut self,
        id: FollowupId,
        created_issue_id: WorkItemId,
        now: &str,
    ) -> StateResult<bool>;
}

const FOLLOWUP_COLUMNS: &str = "id, source_work_item_id, title, reason, scope, \
     acceptance_criteria, blocking, policy, origin_kind, origin_run_id, origin_role, \
     origin_agent_profile, origin_human_actor, status, created_issue_id, approval_role, \
     approval_note, created_at, updated_at";

fn map_followup(row: &rusqlite::Row<'_>) -> rusqlite::Result<FollowupRecord> {
    Ok(FollowupRecord {
        id: FollowupId::new(row.get(0)?),
        source_work_item_id: WorkItemId(row.get(1)?),
        title: row.get(2)?,
        reason: row.get(3)?,
        scope: row.get(4)?,
        acceptance_criteria: row.get(5)?,
        blocking: row.get::<_, i64>(6)? != 0,
        policy: row.get(7)?,
        origin_kind: row.get(8)?,
        origin_run_id: row.get::<_, Option<i64>>(9)?.map(RunId),
        origin_role: row.get(10)?,
        origin_agent_profile: row.get(11)?,
        origin_human_actor: row.get(12)?,
        status: row.get(13)?,
        created_issue_id: row.get::<_, Option<i64>>(14)?.map(WorkItemId),
        approval_role: row.get(15)?,
        approval_note: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

pub(crate) fn create_followup_in(
    conn: &Connection,
    new: NewFollowup<'_>,
) -> StateResult<FollowupRecord> {
    conn.execute(
        "INSERT INTO followups \
            (source_work_item_id, title, reason, scope, acceptance_criteria, blocking, \
             policy, origin_kind, origin_run_id, origin_role, origin_agent_profile, \
             origin_human_actor, status, created_issue_id, approval_role, approval_note, \
             created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            new.source_work_item_id.0,
            new.title,
            new.reason,
            new.scope,
            new.acceptance_criteria,
            if new.blocking { 1 } else { 0 },
            new.policy,
            new.origin_kind,
            new.origin_run_id.map(|id| id.0),
            new.origin_role,
            new.origin_agent_profile,
            new.origin_human_actor,
            new.status,
            new.created_issue_id.map(|id| id.0),
            new.approval_role,
            new.approval_note,
            new.now,
            new.now,
        ],
    )?;
    get_followup_in(conn, FollowupId::new(conn.last_insert_rowid()))?
        .ok_or_else(|| StateError::Invariant("freshly inserted followup must be readable".into()))
}

pub(crate) fn get_followup_in(
    conn: &Connection,
    id: FollowupId,
) -> StateResult<Option<FollowupRecord>> {
    let sql = format!("SELECT {FOLLOWUP_COLUMNS} FROM followups WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt.query_row(params![id.get()], map_followup).optional()?)
}

pub(crate) fn list_followups_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Vec<FollowupRecord>> {
    let sql = format!(
        "SELECT {FOLLOWUP_COLUMNS} FROM followups \
         WHERE source_work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![work_item_id.0], map_followup)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn list_followups_by_status_in(
    conn: &Connection,
    status: &str,
) -> StateResult<Vec<FollowupRecord>> {
    let sql = format!(
        "SELECT {FOLLOWUP_COLUMNS} FROM followups \
         WHERE status = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![status], map_followup)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub(crate) fn mark_followup_created_in(
    conn: &Connection,
    id: FollowupId,
    created_issue_id: WorkItemId,
    now: &str,
) -> StateResult<bool> {
    let changed = conn.execute(
        "UPDATE followups
            SET status = 'created',
                created_issue_id = ?2,
                updated_at = ?3
          WHERE id = ?1",
        params![id.get(), created_issue_id.0, now],
    )?;
    Ok(changed > 0)
}

impl FollowupRepository for StateDb {
    fn create_followup(&mut self, new: NewFollowup<'_>) -> StateResult<FollowupRecord> {
        create_followup_in(self.conn(), new)
    }

    fn get_followup(&self, id: FollowupId) -> StateResult<Option<FollowupRecord>> {
        get_followup_in(self.conn(), id)
    }

    fn list_followups_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<FollowupRecord>> {
        list_followups_for_work_item_in(self.conn(), work_item_id)
    }

    fn list_followups_by_status(&self, status: &str) -> StateResult<Vec<FollowupRecord>> {
        list_followups_by_status_in(self.conn(), status)
    }

    fn mark_followup_created(
        &mut self,
        id: FollowupId,
        created_issue_id: WorkItemId,
        now: &str,
    ) -> StateResult<bool> {
        mark_followup_created_in(self.conn(), id, created_issue_id, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrations;
    use crate::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    const NOW: &str = "2026-05-10T02:00:00Z";

    fn open() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn seed_work_item_and_run(db: &mut StateDb) -> (WorkItemId, RunId) {
        let item = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#1",
                parent_id: None,
                title: "source",
                status_class: "running",
                tracker_status: "open",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap();
        let run = db
            .create_run(NewRun {
                work_item_id: item.id,
                role: "backend",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap();
        (item.id, run.id)
    }

    #[test]
    fn create_followup_round_trips_and_materializes_domain() {
        let mut db = open();
        let (source, run) = seed_work_item_and_run(&mut db);
        let row = db
            .create_followup(NewFollowup {
                source_work_item_id: source,
                title: "Add retry coverage",
                reason: "API retries need a regression test",
                scope: "tests only",
                acceptance_criteria: "[\"retry branch has coverage\"]",
                blocking: false,
                policy: "create_directly",
                origin_kind: "run",
                origin_run_id: Some(run),
                origin_role: Some("backend"),
                origin_agent_profile: None,
                origin_human_actor: None,
                status: "approved",
                created_issue_id: None,
                approval_role: None,
                approval_note: None,
                now: NOW,
            })
            .unwrap();

        assert_eq!(row.title, "Add retry coverage");
        let domain = row.to_domain().unwrap();
        assert_eq!(domain.id, row.id);
        assert_eq!(domain.status, FollowupStatus::Approved);
        assert_eq!(domain.policy, FollowupPolicy::CreateDirectly);
        assert_eq!(domain.acceptance_criteria, vec!["retry branch has coverage"]);
    }

    #[test]
    fn list_followups_by_status_filters_rows() {
        let mut db = open();
        let (source, run) = seed_work_item_and_run(&mut db);
        for (title, status) in [("one", "approved"), ("two", "proposed"), ("three", "approved")] {
            db.create_followup(NewFollowup {
                source_work_item_id: source,
                title,
                reason: "reason",
                scope: "scope",
                acceptance_criteria: "[]",
                blocking: false,
                policy: "create_directly",
                origin_kind: "run",
                origin_run_id: Some(run),
                origin_role: Some("backend"),
                origin_agent_profile: None,
                origin_human_actor: None,
                status,
                created_issue_id: None,
                approval_role: None,
                approval_note: None,
                now: NOW,
            })
            .unwrap();
        }

        let approved = db.list_followups_by_status("approved").unwrap();
        let titles: Vec<_> = approved.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(titles, vec!["one", "three"]);
    }

    #[test]
    fn mark_followup_created_updates_status_and_created_issue_id() {
        let mut db = open();
        let (source, run) = seed_work_item_and_run(&mut db);
        let created_issue = db
            .create_work_item(NewWorkItem {
                tracker_id: "github",
                identifier: "OWNER/REPO#2",
                parent_id: None,
                title: "created",
                status_class: "ready",
                tracker_status: "Backlog",
                assigned_role: None,
                assigned_agent: None,
                priority: None,
                workspace_policy: None,
                branch_policy: None,
                now: NOW,
            })
            .unwrap()
            .id;
        let row = db
            .create_followup(NewFollowup {
                source_work_item_id: source,
                title: "one",
                reason: "reason",
                scope: "scope",
                acceptance_criteria: "[]",
                blocking: false,
                policy: "create_directly",
                origin_kind: "run",
                origin_run_id: Some(run),
                origin_role: Some("backend"),
                origin_agent_profile: None,
                origin_human_actor: None,
                status: "approved",
                created_issue_id: None,
                approval_role: None,
                approval_note: None,
                now: NOW,
            })
            .unwrap();

        assert!(db
            .mark_followup_created(row.id, created_issue, "2026-05-10T03:00:00Z")
            .unwrap());

        let updated = db.get_followup(row.id).unwrap().unwrap();
        assert_eq!(updated.status, "created");
        assert_eq!(updated.created_issue_id, Some(created_issue));
    }
}
