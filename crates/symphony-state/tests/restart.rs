//! Process-restart durability tests for `symphony-state`.
//!
//! Each test follows the same shape: open a `StateDb` against a file
//! path inside a `TempDir`, mutate it, **drop the handle** to simulate
//! process exit, then reopen the same file and prove the durable state
//! the kernel relies on for recovery is intact.
//!
//! These run as an integration-test crate (separate binary) so the
//! `Drop` at the end of each scope is the real teardown — there is no
//! lingering connection sharing a WAL with the reopened handle.

use std::path::PathBuf;

use symphony_state::events::{EventRepository, NewEvent};
use symphony_state::migrations::migrations;
use symphony_state::repository::{
    NewRun, NewWorkItem, RunRepository, WorkItemId, WorkItemRepository,
};
use symphony_state::{StateDb, StateError};
use tempfile::TempDir;

const NOW: &str = "2026-05-08T00:00:00Z";
const LATER: &str = "2026-05-08T01:00:00Z";

fn db_path(dir: &TempDir) -> PathBuf {
    dir.path().join("state.sqlite3")
}

fn open_migrated(path: &PathBuf) -> StateDb {
    let mut db = StateDb::open(path).expect("open");
    db.migrate(migrations()).expect("migrate");
    db
}

fn sample_work_item<'a>(identifier: &'a str, now: &'a str) -> NewWorkItem<'a> {
    NewWorkItem {
        tracker_id: "github",
        identifier,
        parent_id: None,
        title: "demo issue",
        status_class: "ready",
        tracker_status: "open",
        assigned_role: None,
        assigned_agent: None,
        priority: None,
        workspace_policy: None,
        branch_policy: None,
        now,
    }
}

#[test]
fn migrations_are_recorded_and_idempotent_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    {
        let _db = open_migrated(&path);
    }

    let mut db = StateDb::open(&path).expect("reopen");
    let before = db.applied_versions().expect("versions");
    assert!(!before.is_empty(), "migrations must have been recorded");

    let report = db.migrate(migrations()).expect("re-migrate");
    assert!(
        report.applied.is_empty(),
        "no migrations should re-apply on a previously-migrated db, got {:?}",
        report.applied
    );
    assert_eq!(report.skipped.len(), before.len());
    assert_eq!(db.applied_versions().expect("versions"), before);
}

#[test]
fn work_items_runs_and_parent_edges_survive_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    let (parent_id, child_id, run_id) = {
        let mut db = open_migrated(&path);
        let parent = db
            .create_work_item(sample_work_item("OWNER/REPO#1", NOW))
            .expect("parent");
        let child = db
            .create_work_item(NewWorkItem {
                parent_id: Some(parent.id),
                ..sample_work_item("OWNER/REPO#2", NOW)
            })
            .expect("child");
        let run = db
            .create_run(NewRun {
                work_item_id: parent.id,
                role: "platform_lead",
                agent: "claude",
                status: "queued",
                workspace_claim_id: None,
                now: NOW,
            })
            .expect("run");
        (parent.id, child.id, run.id)
    };

    let db = open_migrated(&path);
    let parent = db
        .get_work_item(parent_id)
        .expect("parent get")
        .expect("present");
    assert_eq!(parent.identifier, "OWNER/REPO#1");

    let children = db.list_children(parent_id).expect("children");
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, child_id);
    assert_eq!(children[0].parent_id, Some(parent_id));

    let run = db.get_run(run_id).expect("run get").expect("present");
    assert_eq!(run.work_item_id, parent_id);
    assert_eq!(run.status, "queued");

    let runs = db.list_runs_for_work_item(parent_id).expect("list");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
}

#[test]
fn event_sequence_continues_monotonically_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    let last_before = {
        let mut db = open_migrated(&path);
        let wi = db.create_work_item(sample_work_item("ENG-1", NOW)).unwrap();
        for kind in ["work_item.created", "work_item.routed", "run.queued"] {
            db.append_event(NewEvent {
                event_type: kind,
                work_item_id: Some(wi.id),
                run_id: None,
                payload: "{}",
                now: NOW,
            })
            .expect("append");
        }
        db.last_event_sequence()
            .expect("last")
            .expect("at least one event")
    };

    let mut db = open_migrated(&path);
    assert_eq!(db.last_event_sequence().unwrap(), Some(last_before));

    let next = db
        .append_event(NewEvent {
            event_type: "run.started",
            work_item_id: None,
            run_id: None,
            payload: "{}",
            now: LATER,
        })
        .expect("append after restart");
    assert_eq!(
        next.sequence.0,
        last_before.0 + 1,
        "post-restart sequence must continue from prior max, not reset"
    );

    // Range query stitches across the restart boundary.
    let all = db.list_events_after(None, 1024).expect("all");
    assert_eq!(all.len() as i64, next.sequence.0);
    let suffixes: Vec<_> = all.iter().map(|e| e.sequence.0).collect();
    let mut expected = (1..=next.sequence.0).collect::<Vec<_>>();
    expected.sort();
    assert_eq!(suffixes, expected);
}

#[test]
fn committed_transaction_persists_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    let work_item_id: WorkItemId = {
        let mut db = open_migrated(&path);
        db.transaction(|tx| {
            let wi = tx.create_work_item(sample_work_item("ENG-1", NOW))?;
            tx.append_event(NewEvent {
                event_type: "work_item.created",
                work_item_id: Some(wi.id),
                run_id: None,
                payload: r#"{"source":"intake"}"#,
                now: NOW,
            })?;
            Ok(wi.id)
        })
        .expect("commit")
    };

    let db = open_migrated(&path);
    let wi = db.get_work_item(work_item_id).unwrap().unwrap();
    assert_eq!(wi.identifier, "ENG-1");
    let events = db.list_events_for_work_item(work_item_id).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "work_item.created");
}

#[test]
fn rolled_back_transaction_leaves_no_trace_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    {
        let mut db = open_migrated(&path);
        let err = db
            .transaction::<_, ()>(|tx| {
                tx.create_work_item(sample_work_item("ENG-1", NOW))?;
                tx.append_event(NewEvent {
                    event_type: "work_item.created",
                    work_item_id: None,
                    run_id: None,
                    payload: "{}",
                    now: NOW,
                })?;
                // Force a UNIQUE violation to roll the whole tx back.
                tx.create_work_item(sample_work_item("ENG-1", NOW))?;
                Ok(())
            })
            .expect_err("must fail");
        assert!(matches!(err, StateError::Sqlite(_)));
    }

    let db = open_migrated(&path);
    assert!(
        db.find_work_item_by_identifier("github", "ENG-1")
            .unwrap()
            .is_none(),
        "rolled-back work item must not survive restart"
    );
    assert!(
        db.last_event_sequence().unwrap().is_none(),
        "rolled-back event must not survive restart"
    );
}

#[test]
fn expired_leases_are_recoverable_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = db_path(&dir);

    let (stale_run, fresh_run) = {
        let mut db = open_migrated(&path);
        let wi = db.create_work_item(sample_work_item("ENG-1", NOW)).unwrap();

        let stale = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "platform_lead",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap();
        let fresh = db
            .create_run(NewRun {
                work_item_id: wi.id,
                role: "qa",
                agent: "claude",
                status: "running",
                workspace_claim_id: None,
                now: NOW,
            })
            .unwrap();
        (stale.id, fresh.id)
    };

    // Simulate two in-flight leases at process death: one already past
    // its deadline, one still healthy. We bypass the public surface
    // because the kernel-level lease setter lands in Phase 11; here we
    // only need the `lease_expires_at` column populated to validate the
    // recovery query. The raw connection is opened and dropped between
    // the original StateDb drop and the recovery reopen.
    {
        let conn = rusqlite::Connection::open(&path).expect("raw open");
        conn.execute(
            "UPDATE runs SET lease_owner = ?2, lease_expires_at = ?3 WHERE id = ?1",
            rusqlite::params![stale_run.0, "worker-a", "2026-05-08T00:30:00Z"],
        )
        .unwrap();
        conn.execute(
            "UPDATE runs SET lease_owner = ?2, lease_expires_at = ?3 WHERE id = ?1",
            rusqlite::params![fresh_run.0, "worker-b", "2026-05-08T02:00:00Z"],
        )
        .unwrap();
    }

    let db = open_migrated(&path);
    let expired = db.find_expired_leases(LATER).expect("recovery query");
    let ids: Vec<_> = expired.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![stale_run],
        "only the stale lease should be recoverable; fresh={fresh_run:?}"
    );
    assert_eq!(expired[0].lease_owner.as_deref(), Some("worker-a"));
}
