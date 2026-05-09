//! Forward-only schema migrations for the Symphony-RS v2 durable state.
//!
//! The tables here implement [`ARCHITECTURE_v2.md` §5](../../ARCHITECTURE_v2.md):
//! `work_items`, `work_item_edges`, `runs`, `workspace_claims`, `handoffs`,
//! `qa_verdicts`, `events`, plus the `pending_tracker_syncs` retry queue
//! called out in [`SPEC_v2.md` §5.4](../../SPEC_v2.md) and `budget_pauses`.
//!
//! New schema changes append a new [`Migration`] with a strictly increasing
//! `version` (convention: `YYYYMMDDNN`). Never edit a migration that has
//! shipped — the runner enforces drift detection by name.
//!
//! See `crates/symphony-state/src/lib.rs` for the runner contract.

use crate::Migration;

/// Initial v2 schema. Creates every durable table referenced by the
/// scheduler, integration owner, QA gate, follow-up flow, and the
/// recovery/event-stream surfaces.
const V2_INITIAL_SCHEMA: Migration = Migration {
    version: 2_026_050_801,
    name: "v2_initial_schema",
    sql: r#"
        -- Work item is the kernel's internal record of a tracker issue or
        -- decomposed child. Tracker remains source of truth for product
        -- state; this row is orchestration truth (status_class, assignment,
        -- workspace/branch policy snapshots) per SPEC v2 §5.4.
        CREATE TABLE work_items (
            id                INTEGER PRIMARY KEY,
            tracker_id        TEXT    NOT NULL,
            identifier        TEXT    NOT NULL,
            parent_id         INTEGER REFERENCES work_items(id) ON DELETE SET NULL,
            title             TEXT    NOT NULL,
            status_class      TEXT    NOT NULL,
            tracker_status    TEXT    NOT NULL,
            assigned_role     TEXT,
            assigned_agent    TEXT,
            priority          TEXT,
            workspace_policy  TEXT,
            branch_policy     TEXT,
            created_at        TEXT    NOT NULL,
            updated_at        TEXT    NOT NULL,
            UNIQUE (tracker_id, identifier)
        );

        CREATE INDEX idx_work_items_parent
            ON work_items(parent_id);
        CREATE INDEX idx_work_items_status_class
            ON work_items(status_class);

        -- Workspace claim records the actual on-disk path/branch a run is
        -- about to mutate. Verification report (cwd/branch/clean-tree
        -- checks from SPEC v2 §5.9) is captured as JSON for replay.
        CREATE TABLE workspace_claims (
            id                   INTEGER PRIMARY KEY,
            work_item_id         INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            path                 TEXT    NOT NULL,
            strategy             TEXT    NOT NULL,
            base_ref             TEXT,
            branch               TEXT,
            head_sha             TEXT,
            claim_status         TEXT    NOT NULL,
            verified_at          TEXT,
            verification_report  TEXT,
            created_at           TEXT    NOT NULL
        );

        CREATE INDEX idx_workspace_claims_work_item
            ON workspace_claims(work_item_id);
        CREATE INDEX idx_workspace_claims_status
            ON workspace_claims(claim_status);

        -- A run is one execution of one role/agent against one work item.
        -- `lease_owner` + `lease_expires_at` back the durable leases the
        -- scheduler uses to recover crashed in-flight work.
        CREATE TABLE runs (
            id                   INTEGER PRIMARY KEY,
            work_item_id         INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            role                 TEXT    NOT NULL,
            agent                TEXT    NOT NULL,
            status               TEXT    NOT NULL,
            workspace_claim_id   INTEGER
                REFERENCES workspace_claims(id) ON DELETE SET NULL,
            lease_owner          TEXT,
            lease_expires_at     TEXT,
            started_at           TEXT,
            ended_at             TEXT,
            cost                 REAL,
            tokens               INTEGER,
            result_summary       TEXT,
            error                TEXT,
            created_at           TEXT    NOT NULL
        );

        CREATE INDEX idx_runs_work_item
            ON runs(work_item_id);
        CREATE INDEX idx_runs_status
            ON runs(status);
        CREATE INDEX idx_runs_lease
            ON runs(lease_expires_at)
            WHERE lease_expires_at IS NOT NULL;

        -- Edges between work items: parent/child decomposition, blocker
        -- relationships, plain relates-to references, and follow-up links.
        -- Self-edges are forbidden because they break blocker propagation.
        CREATE TABLE work_item_edges (
            id          INTEGER PRIMARY KEY,
            parent_id   INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            child_id    INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            edge_type   TEXT    NOT NULL
                CHECK (edge_type IN
                    ('parent_child', 'blocks', 'relates_to', 'followup_of')),
            reason      TEXT,
            status      TEXT    NOT NULL,
            created_at  TEXT    NOT NULL,
            CHECK (parent_id <> child_id),
            UNIQUE (parent_id, child_id, edge_type)
        );

        CREATE INDEX idx_work_item_edges_child
            ON work_item_edges(child_id, edge_type);

        -- Structured agent handoff envelope. `ready_for` indicates which
        -- queue the work should land in next (integration, qa, review,
        -- followup_approval, ...). JSON columns are validated at the Rust
        -- layer; SQLite stores them opaquely so schema evolution is cheap.
        CREATE TABLE handoffs (
            id             INTEGER PRIMARY KEY,
            run_id         INTEGER NOT NULL
                REFERENCES runs(id) ON DELETE CASCADE,
            work_item_id   INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            ready_for      TEXT    NOT NULL,
            summary        TEXT    NOT NULL,
            changed_files  TEXT,
            tests_run      TEXT,
            evidence       TEXT,
            known_risks    TEXT,
            created_at     TEXT    NOT NULL
        );

        CREATE INDEX idx_handoffs_work_item
            ON handoffs(work_item_id);
        CREATE INDEX idx_handoffs_ready_for
            ON handoffs(ready_for);

        -- QA verdict ties a run to a pass/fail/inconclusive decision plus
        -- evidence and an acceptance-criteria trace. `blockers_created`
        -- captures the blocker work item ids that QA filed against the
        -- parent so closeout gating can find them later.
        CREATE TABLE qa_verdicts (
            id                INTEGER PRIMARY KEY,
            work_item_id      INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            run_id            INTEGER NOT NULL
                REFERENCES runs(id) ON DELETE CASCADE,
            verdict           TEXT    NOT NULL
                CHECK (verdict IN ('pass', 'fail', 'inconclusive')),
            evidence          TEXT,
            acceptance_trace  TEXT,
            blockers_created  TEXT,
            created_at        TEXT    NOT NULL
        );

        CREATE INDEX idx_qa_verdicts_work_item
            ON qa_verdicts(work_item_id);

        -- Append-only event log. `sequence` is monotonically increasing,
        -- assigned by the kernel inside the same transaction as the
        -- state transition, and is the basis for SSE replay buffers.
        CREATE TABLE events (
            id             INTEGER PRIMARY KEY,
            sequence       INTEGER NOT NULL UNIQUE,
            event_type     TEXT    NOT NULL,
            work_item_id   INTEGER
                REFERENCES work_items(id) ON DELETE SET NULL,
            run_id         INTEGER
                REFERENCES runs(id) ON DELETE SET NULL,
            payload        TEXT    NOT NULL,
            created_at     TEXT    NOT NULL
        );

        CREATE INDEX idx_events_work_item
            ON events(work_item_id);
        CREATE INDEX idx_events_type
            ON events(event_type);

        -- Pending tracker syncs are mutations the kernel committed locally
        -- but has not yet pushed to the upstream tracker (or that failed
        -- and need retry). Required by SPEC v2 §5.4: "SQLite stores ...
        -- pending tracker syncs ... The issue tracker remains the shared
        -- product/work truth and must be synced through tracker mutations."
        CREATE TABLE pending_tracker_syncs (
            id               INTEGER PRIMARY KEY,
            work_item_id     INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            mutation_kind    TEXT    NOT NULL,
            payload          TEXT    NOT NULL,
            attempts         INTEGER NOT NULL DEFAULT 0,
            last_error       TEXT,
            status           TEXT    NOT NULL
                CHECK (status IN
                    ('pending', 'in_flight', 'succeeded', 'failed')),
            next_attempt_at  TEXT,
            created_at       TEXT    NOT NULL,
            updated_at       TEXT    NOT NULL
        );

        CREATE INDEX idx_pending_tracker_syncs_due
            ON pending_tracker_syncs(status, next_attempt_at);

        -- Budget pauses are durable records of cost/token/retry caps the
        -- scheduler hit. They block further runs against a work item
        -- until resolved or waived. `limit_value` avoids the SQL keyword
        -- `limit`.
        CREATE TABLE budget_pauses (
            id            INTEGER PRIMARY KEY,
            work_item_id  INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            budget_kind   TEXT    NOT NULL,
            limit_value   REAL    NOT NULL,
            observed      REAL    NOT NULL,
            status        TEXT    NOT NULL
                CHECK (status IN ('active', 'resolved', 'waived')),
            created_at    TEXT    NOT NULL,
            resolved_at   TEXT
        );

        CREATE INDEX idx_budget_pauses_active
            ON budget_pauses(work_item_id, status);
    "#,
};

/// Returns the full ordered list of v2 schema migrations.
///
/// Callers should pass the slice straight to [`crate::StateDb::migrate`].
/// Adding a new migration means appending to this list with a strictly
/// greater `version`; never reorder or rewrite a shipped entry.
pub fn migrations() -> &'static [Migration] {
    &MIGRATIONS
}

/// Adds the `integration_records` table. The kernel writes one row per
/// integration-owner consolidation attempt against a parent work item
/// (SPEC v2 §5.10, §6.4); the row is the durable proof QA reads to know
/// what artifact it is verifying and the parent closeout gate reads to
/// confirm "integration record exists when required" (ARCH §6.2).
///
/// JSON columns (`children_consolidated`, `conflicts`, `blockers_filed`)
/// match the convention used by `events.payload` and the handoff JSON
/// columns: stored opaquely so the storage crate stays free of
/// `serde_json` and the canonical encoder lives in `symphony-core`.
const V2_INTEGRATION_RECORDS: Migration = Migration {
    version: 2_026_050_802,
    name: "v2_integration_records",
    sql: r#"
        CREATE TABLE integration_records (
            id                     INTEGER PRIMARY KEY,
            parent_work_item_id    INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            owner_role             TEXT    NOT NULL,
            run_id                 INTEGER
                REFERENCES runs(id) ON DELETE SET NULL,
            status                 TEXT    NOT NULL
                CHECK (status IN
                    ('pending', 'in_progress', 'succeeded',
                     'failed', 'blocked', 'cancelled')),
            merge_strategy         TEXT    NOT NULL
                CHECK (merge_strategy IN
                    ('sequential_cherry_pick', 'merge_commits',
                     'shared_branch')),
            integration_branch     TEXT,
            base_ref               TEXT,
            head_sha               TEXT,
            workspace_path         TEXT,
            children_consolidated  TEXT,
            summary                TEXT,
            conflicts              TEXT,
            blockers_filed         TEXT,
            created_at             TEXT    NOT NULL
        );

        CREATE INDEX idx_integration_records_parent
            ON integration_records(parent_work_item_id);
        CREATE INDEX idx_integration_records_status
            ON integration_records(status);
    "#,
};

const MIGRATIONS: [Migration; 2] = [V2_INITIAL_SCHEMA, V2_INTEGRATION_RECORDS];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateDb;
    use rusqlite::params;

    fn open_migrated() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open in-memory db");
        db.migrate(migrations()).expect("apply v2 migrations");
        db
    }

    fn table_exists(db: &StateDb, name: &str) -> bool {
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                params![name],
                |row| row.get(0),
            )
            .expect("query sqlite_master");
        count == 1
    }

    #[test]
    fn v2_migration_creates_every_documented_table() {
        let db = open_migrated();
        for table in [
            "work_items",
            "work_item_edges",
            "runs",
            "workspace_claims",
            "handoffs",
            "qa_verdicts",
            "events",
            "pending_tracker_syncs",
            "budget_pauses",
            "integration_records",
        ] {
            assert!(table_exists(&db, table), "missing table `{table}`");
        }
    }

    #[test]
    fn v2_migration_is_recorded_under_its_version() {
        let db = open_migrated();
        assert_eq!(
            db.applied_versions().unwrap(),
            vec![2_026_050_801, 2_026_050_802]
        );
    }

    #[test]
    fn rerunning_migrations_is_idempotent() {
        let mut db = StateDb::open_in_memory().expect("open");
        let first = db.migrate(migrations()).expect("first apply");
        let second = db.migrate(migrations()).expect("second apply");
        assert_eq!(first.applied, vec![2_026_050_801, 2_026_050_802]);
        assert!(first.skipped.is_empty());
        assert!(second.applied.is_empty());
        assert_eq!(second.skipped, vec![2_026_050_801, 2_026_050_802]);
    }

    fn insert_work_item(db: &StateDb, identifier: &str) -> i64 {
        db.conn()
            .execute(
                "INSERT INTO work_items
                    (tracker_id, identifier, title, status_class,
                     tracker_status, created_at, updated_at)
                 VALUES
                    ('github', ?1, 'demo', 'ready', 'open',
                     '2026-05-08T00:00:00Z', '2026-05-08T00:00:00Z')",
                params![identifier],
            )
            .expect("insert work_item");
        db.conn().last_insert_rowid()
    }

    #[test]
    fn foreign_keys_reject_orphan_runs() {
        let db = open_migrated();
        let err = db.conn().execute(
            "INSERT INTO runs
                (work_item_id, role, agent, status, created_at)
             VALUES (9999, 'specialist', 'codex', 'queued', '2026-05-08T00:00:00Z')",
            [],
        );
        assert!(err.is_err(), "FK should reject orphan runs row");
    }

    #[test]
    fn edge_type_check_rejects_unknown_kinds() {
        let db = open_migrated();
        let parent = insert_work_item(&db, "ISSUE-1");
        let child = insert_work_item(&db, "ISSUE-2");
        let err = db.conn().execute(
            "INSERT INTO work_item_edges
                (parent_id, child_id, edge_type, status, created_at)
             VALUES (?1, ?2, 'totally_made_up', 'open', '2026-05-08T00:00:00Z')",
            params![parent, child],
        );
        assert!(err.is_err(), "CHECK should reject unknown edge_type");
    }

    #[test]
    fn edge_type_check_rejects_self_edges() {
        let db = open_migrated();
        let only = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO work_item_edges
                (parent_id, child_id, edge_type, status, created_at)
             VALUES (?1, ?1, 'parent_child', 'open', '2026-05-08T00:00:00Z')",
            params![only],
        );
        assert!(err.is_err(), "CHECK should reject self-edges");
    }

    #[test]
    fn qa_verdict_check_rejects_unknown_verdict() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO runs
                    (id, work_item_id, role, agent, status, created_at)
                 VALUES (1, ?1, 'qa_gate', 'claude', 'completed',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("insert run");
        let err = db.conn().execute(
            "INSERT INTO qa_verdicts
                (work_item_id, run_id, verdict, created_at)
             VALUES (?1, 1, 'maybe', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown verdict");
    }

    #[test]
    fn events_sequence_is_unique() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO events
                    (sequence, event_type, work_item_id, payload, created_at)
                 VALUES (1, 'work_item.created', ?1, '{}',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("first event");
        let err = db.conn().execute(
            "INSERT INTO events
                (sequence, event_type, work_item_id, payload, created_at)
             VALUES (1, 'work_item.updated', ?1, '{}', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "sequence collisions must be rejected");
    }

    #[test]
    fn pending_tracker_syncs_round_trip() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO pending_tracker_syncs
                    (work_item_id, mutation_kind, payload, status,
                     created_at, updated_at)
                 VALUES (?1, 'add_comment', '{\"body\":\"hi\"}', 'pending',
                         '2026-05-08T00:00:00Z', '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("insert pending sync");
        let (kind, status): (String, String) = db
            .conn()
            .query_row(
                "SELECT mutation_kind, status FROM pending_tracker_syncs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read back");
        assert_eq!(kind, "add_comment");
        assert_eq!(status, "pending");
    }

    #[test]
    fn integration_record_status_check_rejects_unknown_status() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO integration_records
                (parent_work_item_id, owner_role, status, merge_strategy,
                 created_at)
             VALUES (?1, 'platform_lead', 'maybe', 'merge_commits',
                     '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown status");
    }

    #[test]
    fn integration_record_merge_strategy_check_rejects_unknown_strategy() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO integration_records
                (parent_work_item_id, owner_role, status, merge_strategy,
                 created_at)
             VALUES (?1, 'platform_lead', 'pending', 'rebase_n_pray',
                     '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown merge_strategy");
    }

    #[test]
    fn integration_records_cascade_when_parent_deleted() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO integration_records
                    (parent_work_item_id, owner_role, status, merge_strategy,
                     created_at)
                 VALUES (?1, 'platform_lead', 'pending', 'merge_commits',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("insert integration_records row");
        db.conn()
            .execute("DELETE FROM work_items WHERE id = ?1", params![wi])
            .expect("delete work_item");
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM integration_records", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 0, "FK cascade should clear integration_records");
    }

    #[test]
    fn budget_pause_status_check_rejects_unknown() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO budget_pauses
                (work_item_id, budget_kind, limit_value, observed,
                 status, created_at)
             VALUES (?1, 'cost', 5.0, 7.0, 'unknown',
                     '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown budget status");
    }
}
