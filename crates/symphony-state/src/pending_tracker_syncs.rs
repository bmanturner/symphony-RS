//! Durable retry queue for tracker mutations that must be replayed later.

use rusqlite::{Connection, OptionalExtension, params};

use crate::repository::WorkItemId;
use crate::{StateDb, StateResult};

/// Stored `pending_tracker_syncs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTrackerSyncRecord {
    /// Row id.
    pub id: i64,
    /// Local work item the tracker mutation targets.
    pub work_item_id: WorkItemId,
    /// Mutation discriminator (`create_followup`, `add_comment`, ...).
    pub mutation_kind: String,
    /// Opaque JSON payload for the eventual dispatcher.
    pub payload: String,
    /// Attempt counter.
    pub attempts: i64,
    /// Last tracker error, if any.
    pub last_error: Option<String>,
    /// Queue status.
    pub status: String,
    /// Next retry timestamp, if scheduled.
    pub next_attempt_at: Option<String>,
    /// Row creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
}

/// Insert payload for [`PendingTrackerSyncRepository::enqueue_pending_tracker_sync`].
#[derive(Debug, Clone)]
pub struct NewPendingTrackerSync<'a> {
    /// Local work item the mutation targets.
    pub work_item_id: WorkItemId,
    /// Mutation discriminator.
    pub mutation_kind: &'a str,
    /// Opaque JSON payload.
    pub payload: &'a str,
    /// Last tracker error captured at enqueue time.
    pub last_error: Option<&'a str>,
    /// Optional next-at timestamp.
    pub next_attempt_at: Option<&'a str>,
    /// Timestamp written to both `created_at` and `updated_at`.
    pub now: &'a str,
}

/// Read/write access to `pending_tracker_syncs`.
pub trait PendingTrackerSyncRepository {
    /// Insert a new pending tracker mutation in `pending` status.
    fn enqueue_pending_tracker_sync(
        &mut self,
        new: NewPendingTrackerSync<'_>,
    ) -> StateResult<PendingTrackerSyncRecord>;

    /// List every pending tracker sync for `work_item_id`, oldest first.
    fn list_pending_tracker_syncs_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<PendingTrackerSyncRecord>>;
}

const PENDING_TRACKER_SYNC_COLUMNS: &str = "id, work_item_id, mutation_kind, payload, attempts, \
     last_error, status, next_attempt_at, created_at, updated_at";

fn map_pending_tracker_sync(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingTrackerSyncRecord> {
    Ok(PendingTrackerSyncRecord {
        id: row.get(0)?,
        work_item_id: WorkItemId(row.get(1)?),
        mutation_kind: row.get(2)?,
        payload: row.get(3)?,
        attempts: row.get(4)?,
        last_error: row.get(5)?,
        status: row.get(6)?,
        next_attempt_at: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

pub(crate) fn enqueue_pending_tracker_sync_in(
    conn: &Connection,
    new: NewPendingTrackerSync<'_>,
) -> StateResult<PendingTrackerSyncRecord> {
    conn.execute(
        "INSERT INTO pending_tracker_syncs \
            (work_item_id, mutation_kind, payload, attempts, last_error, status, next_attempt_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, 0, ?4, 'pending', ?5, ?6, ?6)",
        params![
            new.work_item_id.0,
            new.mutation_kind,
            new.payload,
            new.last_error,
            new.next_attempt_at,
            new.now,
        ],
    )?;
    Ok(get_pending_tracker_sync_in(conn, conn.last_insert_rowid())?
        .expect("fresh pending sync readable"))
}

fn get_pending_tracker_sync_in(
    conn: &Connection,
    id: i64,
) -> StateResult<Option<PendingTrackerSyncRecord>> {
    let sql =
        format!("SELECT {PENDING_TRACKER_SYNC_COLUMNS} FROM pending_tracker_syncs WHERE id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    Ok(stmt
        .query_row(params![id], map_pending_tracker_sync)
        .optional()?)
}

pub(crate) fn list_pending_tracker_syncs_for_work_item_in(
    conn: &Connection,
    work_item_id: WorkItemId,
) -> StateResult<Vec<PendingTrackerSyncRecord>> {
    let sql = format!(
        "SELECT {PENDING_TRACKER_SYNC_COLUMNS} FROM pending_tracker_syncs \
         WHERE work_item_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![work_item_id.0])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(map_pending_tracker_sync(row)?);
    }
    Ok(out)
}

impl PendingTrackerSyncRepository for StateDb {
    fn enqueue_pending_tracker_sync(
        &mut self,
        new: NewPendingTrackerSync<'_>,
    ) -> StateResult<PendingTrackerSyncRecord> {
        enqueue_pending_tracker_sync_in(self.conn(), new)
    }

    fn list_pending_tracker_syncs_for_work_item(
        &self,
        work_item_id: WorkItemId,
    ) -> StateResult<Vec<PendingTrackerSyncRecord>> {
        list_pending_tracker_syncs_for_work_item_in(self.conn(), work_item_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::migrations;
    use crate::repository::{NewWorkItem, WorkItemRepository};

    #[test]
    fn enqueue_pending_tracker_sync_round_trips() {
        let mut db = StateDb::open_in_memory().unwrap();
        db.migrate(migrations()).unwrap();
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
                now: "2026-05-10T00:00:00Z",
            })
            .unwrap();

        let queued = db
            .enqueue_pending_tracker_sync(NewPendingTrackerSync {
                work_item_id: item.id,
                mutation_kind: "create_followup",
                payload: "{\"followup_id\":1}",
                last_error: Some("tracker down"),
                next_attempt_at: None,
                now: "2026-05-10T00:00:00Z",
            })
            .unwrap();

        assert_eq!(queued.mutation_kind, "create_followup");
        assert_eq!(queued.status, "pending");
        assert_eq!(queued.last_error.as_deref(), Some("tracker down"));

        let listed = db
            .list_pending_tracker_syncs_for_work_item(item.id)
            .unwrap();
        assert_eq!(listed, vec![queued]);
    }
}
