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

/// Adds the `pull_request_records` table. The kernel writes one row per
/// material PR change observed by the integration owner (open, update,
/// mark-ready, merge, close) per SPEC v2 §5.11. Earlier rows are
/// preserved as audit; the parent-closeout gate and the QA brief read
/// the latest row.
///
/// `provider` and `state` are constrained to the variants surfaced by
/// `symphony-core::pull_request`. The link back to the originating
/// integration record is `ON DELETE SET NULL` for the same reason as
/// `runs` on `integration_records`: the PR row is durable evidence and
/// must outlive an integration-record purge.
const V2_PULL_REQUEST_RECORDS: Migration = Migration {
    version: 2_026_050_803,
    name: "v2_pull_request_records",
    sql: r#"
        CREATE TABLE pull_request_records (
            id                      INTEGER PRIMARY KEY,
            parent_work_item_id     INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            owner_role              TEXT    NOT NULL,
            run_id                  INTEGER
                REFERENCES runs(id) ON DELETE SET NULL,
            integration_record_id   INTEGER
                REFERENCES integration_records(id) ON DELETE SET NULL,
            provider                TEXT    NOT NULL
                CHECK (provider IN ('github')),
            state                   TEXT    NOT NULL
                CHECK (state IN ('draft', 'ready', 'merged', 'closed')),
            number                  INTEGER,
            url                     TEXT,
            head_branch             TEXT    NOT NULL,
            base_branch             TEXT,
            head_sha                TEXT,
            title                   TEXT    NOT NULL,
            body                    TEXT,
            ci_status               TEXT,
            created_at              TEXT    NOT NULL
        );

        CREATE INDEX idx_pull_request_records_parent
            ON pull_request_records(parent_work_item_id);
        CREATE INDEX idx_pull_request_records_state
            ON pull_request_records(state);
    "#,
};

/// Widens the `qa_verdicts.verdict` CHECK to the five SPEC v2 §4.9 verdicts
/// and persists verdict authorship/waiver metadata required by SPEC v2
/// §5.12: the authoring `role`, an optional `waiver_role`, and an
/// optional operator-facing `reason`.
///
/// SQLite cannot widen a CHECK constraint or add NOT NULL columns in
/// place, so this migration follows the documented "12-step" recreate
/// pattern: build the new table, copy rows with the legacy three-state
/// verdict mapped onto the new five-state vocabulary, drop the old
/// table, rename, and reinstate the index.
///
/// The legacy → SPEC mapping mirrors the docstring in
/// `symphony-core::qa::QaOutcome` (Phase 2 collapsed five verdicts into
/// three storage states):
///
/// * `pass`         → `passed`
/// * `fail`         → `failed_needs_rework` (the safer of the two failure
///   variants — it does not require a referenced blocker, which legacy
///   rows cannot supply)
/// * `inconclusive` → `inconclusive`
///
/// Existing rows are imported with `role = 'qa'` (the SPEC default
/// `qa_gate` role name) and `waiver_role` / `reason` left `NULL`.
const V2_QA_VERDICT_AUTHORSHIP: Migration = Migration {
    version: 2_026_050_804,
    name: "v2_qa_verdict_authorship",
    sql: r#"
        CREATE TABLE qa_verdicts_new (
            id                INTEGER PRIMARY KEY,
            work_item_id      INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            run_id            INTEGER NOT NULL
                REFERENCES runs(id) ON DELETE CASCADE,
            role              TEXT    NOT NULL,
            verdict           TEXT    NOT NULL
                CHECK (verdict IN
                    ('passed', 'failed_with_blockers',
                     'failed_needs_rework', 'inconclusive', 'waived')),
            waiver_role       TEXT,
            reason            TEXT,
            evidence          TEXT,
            acceptance_trace  TEXT,
            blockers_created  TEXT,
            created_at        TEXT    NOT NULL
        );

        INSERT INTO qa_verdicts_new
            (id, work_item_id, run_id, role, verdict,
             waiver_role, reason,
             evidence, acceptance_trace, blockers_created, created_at)
        SELECT id, work_item_id, run_id, 'qa',
               CASE verdict
                   WHEN 'pass'         THEN 'passed'
                   WHEN 'fail'         THEN 'failed_needs_rework'
                   WHEN 'inconclusive' THEN 'inconclusive'
                   ELSE verdict
               END,
               NULL, NULL,
               evidence, acceptance_trace, blockers_created, created_at
          FROM qa_verdicts;

        DROP TABLE qa_verdicts;
        ALTER TABLE qa_verdicts_new RENAME TO qa_verdicts;

        CREATE INDEX idx_qa_verdicts_work_item
            ON qa_verdicts(work_item_id);
        CREATE INDEX idx_qa_verdicts_role
            ON qa_verdicts(role);
    "#,
};

/// Adds a CHECK constraint to `runs.status` covering the six SPEC v2 §4.5
/// run-status labels (`queued`, `running`, `completed`, `failed`,
/// `cancelled`, `cancel_requested`) and backfills any rows whose stored
/// label predates the canonical vocabulary.
///
/// Backfill rules:
///
/// * `'succeeded'` — the legacy alias for the terminal-success state — is
///   rewritten to `'completed'` so it survives the new CHECK.
/// * Any other out-of-set value is rewritten to `'failed'` defensively.
///   The kernel never produced such labels, but a tampered or
///   externally-edited DB should still load instead of refusing every
///   subsequent migration.
///
/// SQLite cannot add a CHECK to an existing column in place. The migration
/// follows the documented "12-step" recreate pattern: build a new table
/// with the constraint, copy every row, drop the old table, rename the
/// new table, and reinstate every index from `V2_INITIAL_SCHEMA` (the
/// `idx_runs_lease` partial index in particular is load-bearing for
/// `RunRepository::find_expired_leases`). `PRAGMA defer_foreign_keys = ON`
/// is set inside the migration so inbound FKs from `handoffs`,
/// `qa_verdicts`, `events`, `integration_records`, and
/// `pull_request_records` survive the table swap and are re-checked at
/// COMMIT.
const V2_RUNS_STATUS_CHECK: Migration = Migration {
    version: 2_026_050_805,
    name: "v2_runs_status_check",
    sql: r#"
        PRAGMA defer_foreign_keys = ON;

        UPDATE runs SET status = 'completed' WHERE status = 'succeeded';
        UPDATE runs SET status = 'failed'
            WHERE status NOT IN
                ('queued', 'running', 'completed', 'failed',
                 'cancelled', 'cancel_requested');

        CREATE TABLE runs_new (
            id                   INTEGER PRIMARY KEY,
            work_item_id         INTEGER NOT NULL
                REFERENCES work_items(id) ON DELETE CASCADE,
            role                 TEXT    NOT NULL,
            agent                TEXT    NOT NULL,
            status               TEXT    NOT NULL
                CHECK (status IN
                    ('queued', 'running', 'completed', 'failed',
                     'cancelled', 'cancel_requested')),
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

        INSERT INTO runs_new
            (id, work_item_id, role, agent, status, workspace_claim_id,
             lease_owner, lease_expires_at, started_at, ended_at,
             cost, tokens, result_summary, error, created_at)
        SELECT
            id, work_item_id, role, agent, status, workspace_claim_id,
            lease_owner, lease_expires_at, started_at, ended_at,
            cost, tokens, result_summary, error, created_at
        FROM runs;

        DROP TABLE runs;
        ALTER TABLE runs_new RENAME TO runs;

        CREATE INDEX idx_runs_work_item
            ON runs(work_item_id);
        CREATE INDEX idx_runs_status
            ON runs(status);
        CREATE INDEX idx_runs_lease
            ON runs(lease_expires_at)
            WHERE lease_expires_at IS NOT NULL;
    "#,
};

/// Adds the `cancel_requests` table backing the durable side of the
/// cooperative-cancellation primitive (SPEC v2 §4.5 / Phase 11.5).
///
/// Each row is one operator-issued (or cascade-issued) cancel intent
/// against a [`symphony_core::cancellation::CancelSubject`]. Subjects
/// live in two disjoint keyspaces — runs and work items — so the
/// `subject_kind` discriminator is part of every uniqueness check. The
/// in-memory [`symphony_core::cancellation::CancellationQueue`] enforces
/// "at most one pending request per subject"; the durable table mirrors
/// that invariant via a partial unique index on
/// `(subject_kind, subject_id) WHERE state = 'pending'` so the two
/// layers cannot disagree about pending count after a restart.
///
/// `state = 'pending'` is the live entry consulted by runners pre-lease;
/// `state = 'drained'` is the audit row left behind once a runner has
/// observed the cancel and transitioned the run/work item to its
/// terminal state. Drained rows survive for replay/observability —
/// purging is a later operability concern.
///
/// FK choice: `subject_id` is *not* a foreign key because work items
/// and runs share the same column. Surface integrity is enforced at
/// write time by the kernel, which only enqueues against ids it
/// already knows.
const V2_CANCEL_REQUESTS: Migration = Migration {
    version: 2_026_050_806,
    name: "v2_cancel_requests",
    sql: r#"
        CREATE TABLE cancel_requests (
            id             INTEGER PRIMARY KEY,
            subject_kind   TEXT    NOT NULL
                CHECK (subject_kind IN ('run', 'work_item')),
            subject_id     INTEGER NOT NULL,
            reason         TEXT    NOT NULL,
            requested_by   TEXT    NOT NULL,
            requested_at   TEXT    NOT NULL,
            state          TEXT    NOT NULL DEFAULT 'pending'
                CHECK (state IN ('pending', 'drained')),
            created_at     TEXT    NOT NULL,
            drained_at     TEXT
        );

        CREATE UNIQUE INDEX idx_cancel_requests_pending_subject
            ON cancel_requests(subject_kind, subject_id)
            WHERE state = 'pending';

        CREATE INDEX idx_cancel_requests_state
            ON cancel_requests(state);
    "#,
};

const V3_EDGE_PROVENANCE: Migration = Migration {
    version: 2_026_051_001,
    name: "v3_edge_provenance",
    sql: r#"
        ALTER TABLE work_item_edges
            ADD COLUMN source TEXT NOT NULL DEFAULT 'unknown'
                CHECK (source IN
                    ('unknown', 'decomposition', 'qa', 'followup', 'human'));
    "#,
};

const V3_EDGE_TRACKER_SYNC_METADATA: Migration = Migration {
    version: 2_026_051_002,
    name: "v3_edge_tracker_sync_metadata",
    sql: r#"
        ALTER TABLE work_item_edges
            ADD COLUMN tracker_edge_id TEXT;
        ALTER TABLE work_item_edges
            ADD COLUMN tracker_sync_status TEXT;
        ALTER TABLE work_item_edges
            ADD COLUMN tracker_sync_last_error TEXT;
        ALTER TABLE work_item_edges
            ADD COLUMN tracker_sync_attempts INTEGER NOT NULL DEFAULT 0;
        ALTER TABLE work_item_edges
            ADD COLUMN tracker_sync_last_attempt_at TEXT;

        CREATE INDEX idx_work_item_edges_blocked_open
            ON work_item_edges(child_id, edge_type, status)
            WHERE edge_type = 'blocks' AND status = 'open';
    "#,
};

const MIGRATIONS: [Migration; 8] = [
    V2_INITIAL_SCHEMA,
    V2_INTEGRATION_RECORDS,
    V2_PULL_REQUEST_RECORDS,
    V2_QA_VERDICT_AUTHORSHIP,
    V2_RUNS_STATUS_CHECK,
    V2_CANCEL_REQUESTS,
    V3_EDGE_PROVENANCE,
    V3_EDGE_TRACKER_SYNC_METADATA,
];

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
            "cancel_requests",
        ] {
            assert!(table_exists(&db, table), "missing table `{table}`");
        }
    }

    #[test]
    fn v2_migration_is_recorded_under_its_version() {
        let db = open_migrated();
        assert_eq!(
            db.applied_versions().unwrap(),
            vec![
                2_026_050_801,
                2_026_050_802,
                2_026_050_803,
                2_026_050_804,
                2_026_050_805,
                2_026_050_806,
                2_026_051_001,
                2_026_051_002,
            ]
        );
    }

    #[test]
    fn rerunning_migrations_is_idempotent() {
        let mut db = StateDb::open_in_memory().expect("open");
        let first = db.migrate(migrations()).expect("first apply");
        let second = db.migrate(migrations()).expect("second apply");
        assert_eq!(
            first.applied,
            vec![
                2_026_050_801,
                2_026_050_802,
                2_026_050_803,
                2_026_050_804,
                2_026_050_805,
                2_026_050_806,
                2_026_051_001,
                2_026_051_002,
            ]
        );
        assert!(first.skipped.is_empty());
        assert!(second.applied.is_empty());
        assert_eq!(
            second.skipped,
            vec![
                2_026_050_801,
                2_026_050_802,
                2_026_050_803,
                2_026_050_804,
                2_026_050_805,
                2_026_050_806,
                2_026_051_001,
                2_026_051_002,
            ]
        );
    }

    fn table_columns(db: &StateDb, table: &str) -> Vec<String> {
        let mut stmt = db
            .conn()
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare pragma");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query pragma");
        rows.collect::<Result<Vec<_>, _>>().expect("read columns")
    }

    #[test]
    fn v3_edge_tracker_sync_metadata_columns_exist() {
        let db = open_migrated();
        let columns = table_columns(&db, "work_item_edges");
        for column in [
            "tracker_edge_id",
            "tracker_sync_status",
            "tracker_sync_last_error",
            "tracker_sync_attempts",
            "tracker_sync_last_attempt_at",
        ] {
            assert!(columns.iter().any(|c| c == column), "missing `{column}`");
        }
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
                (work_item_id, run_id, role, verdict, created_at)
             VALUES (?1, 1, 'qa', 'maybe', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown verdict");
    }

    #[test]
    fn qa_verdict_check_accepts_all_five_spec_verdicts() {
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
        for verdict in [
            "passed",
            "failed_with_blockers",
            "failed_needs_rework",
            "inconclusive",
            "waived",
        ] {
            db.conn()
                .execute(
                    "INSERT INTO qa_verdicts
                        (work_item_id, run_id, role, verdict, created_at)
                     VALUES (?1, 1, 'qa', ?2, '2026-05-08T00:00:00Z')",
                    params![wi, verdict],
                )
                .unwrap_or_else(|err| panic!("CHECK should accept verdict `{verdict}`: {err}"));
        }
    }

    #[test]
    fn qa_verdict_persists_role_waiver_role_and_reason() {
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
        db.conn()
            .execute(
                "INSERT INTO qa_verdicts
                    (work_item_id, run_id, role, verdict,
                     waiver_role, reason, created_at)
                 VALUES (?1, 1, 'qa', 'waived',
                         'platform_lead', 'accepted risk',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("insert waived verdict");
        let (role, verdict, waiver_role, reason): (String, String, Option<String>, Option<String>) =
            db.conn()
                .query_row(
                    "SELECT role, verdict, waiver_role, reason
                       FROM qa_verdicts
                      WHERE work_item_id = ?1",
                    params![wi],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("read back row");
        assert_eq!(role, "qa");
        assert_eq!(verdict, "waived");
        assert_eq!(waiver_role.as_deref(), Some("platform_lead"));
        assert_eq!(reason.as_deref(), Some("accepted risk"));
    }

    #[test]
    fn qa_verdict_role_column_is_not_null() {
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
             VALUES (?1, 1, 'passed', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "role NOT NULL should reject missing role");
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
    fn pull_request_state_check_rejects_unknown() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO pull_request_records
                (parent_work_item_id, owner_role, provider, state,
                 head_branch, title, created_at)
             VALUES (?1, 'platform_lead', 'github', 'maybe',
                     'feat/x', 'title', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown PR state");
    }

    #[test]
    fn pull_request_provider_check_rejects_unknown() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO pull_request_records
                (parent_work_item_id, owner_role, provider, state,
                 head_branch, title, created_at)
             VALUES (?1, 'platform_lead', 'gitlab', 'draft',
                     'feat/x', 'title', '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(err.is_err(), "CHECK should reject unknown PR provider");
    }

    #[test]
    fn pull_request_records_cascade_when_parent_deleted() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO pull_request_records
                    (parent_work_item_id, owner_role, provider, state,
                     head_branch, title, created_at)
                 VALUES (?1, 'platform_lead', 'github', 'draft',
                         'feat/x', 'title', '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("insert pull_request_records row");
        db.conn()
            .execute("DELETE FROM work_items WHERE id = ?1", params![wi])
            .expect("delete work_item");
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM pull_request_records", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 0, "FK cascade should clear pull_request_records");
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

    /// Apply only the schemas that predate `V2_RUNS_STATUS_CHECK` so tests
    /// can stage rows under the legacy (no-CHECK) `runs.status` shape and
    /// then run the new migration on top.
    fn open_pre_runs_status_check() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open in-memory db");
        db.migrate(&[
            V2_INITIAL_SCHEMA,
            V2_INTEGRATION_RECORDS,
            V2_PULL_REQUEST_RECORDS,
            V2_QA_VERDICT_AUTHORSHIP,
        ])
        .expect("apply pre-CHECK migrations");
        db
    }

    fn run_status(db: &StateDb, run_id: i64) -> String {
        db.conn()
            .query_row(
                "SELECT status FROM runs WHERE id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .expect("read runs.status")
    }

    #[test]
    fn runs_status_check_accepts_every_spec_label() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        for (idx, label) in [
            "queued",
            "running",
            "completed",
            "failed",
            "cancelled",
            "cancel_requested",
        ]
        .iter()
        .enumerate()
        {
            db.conn()
                .execute(
                    "INSERT INTO runs
                        (id, work_item_id, role, agent, status, created_at)
                     VALUES (?1, ?2, 'specialist', 'codex', ?3,
                             '2026-05-08T00:00:00Z')",
                    params![idx as i64 + 1, wi, label],
                )
                .unwrap_or_else(|err| panic!("CHECK should accept run status `{label}`: {err}"));
        }
    }

    #[test]
    fn runs_status_check_rejects_unknown_label() {
        let db = open_migrated();
        let wi = insert_work_item(&db, "ISSUE-1");
        let err = db.conn().execute(
            "INSERT INTO runs
                (work_item_id, role, agent, status, created_at)
             VALUES (?1, 'specialist', 'codex', 'succeeded',
                     '2026-05-08T00:00:00Z')",
            params![wi],
        );
        assert!(
            err.is_err(),
            "CHECK should reject the legacy `succeeded` label after migration"
        );
    }

    #[test]
    fn runs_status_check_backfills_legacy_succeeded_to_completed() {
        let mut db = open_pre_runs_status_check();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO runs
                    (id, work_item_id, role, agent, status,
                     lease_owner, lease_expires_at, created_at)
                 VALUES (1, ?1, 'specialist', 'codex', 'succeeded',
                         'host/sched/0', '2026-05-08T00:01:00Z',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("seed legacy succeeded run");

        db.migrate(&[V2_RUNS_STATUS_CHECK])
            .expect("apply runs status CHECK migration");

        assert_eq!(run_status(&db, 1), "completed");
        let (work_item_id, role, agent, lease_owner, lease_expires_at): (
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT work_item_id, role, agent, lease_owner, lease_expires_at
                   FROM runs WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("read back row");
        assert_eq!(work_item_id, wi);
        assert_eq!(role, "specialist");
        assert_eq!(agent, "codex");
        assert_eq!(lease_owner.as_deref(), Some("host/sched/0"));
        assert_eq!(lease_expires_at.as_deref(), Some("2026-05-08T00:01:00Z"));
    }

    #[test]
    fn runs_status_check_backfills_unknown_label_to_failed() {
        let mut db = open_pre_runs_status_check();
        let wi = insert_work_item(&db, "ISSUE-1");
        db.conn()
            .execute(
                "INSERT INTO runs
                    (id, work_item_id, role, agent, status, created_at)
                 VALUES (1, ?1, 'specialist', 'codex', 'mystery',
                         '2026-05-08T00:00:00Z')",
                params![wi],
            )
            .expect("seed legacy unknown run");

        db.migrate(&[V2_RUNS_STATUS_CHECK])
            .expect("apply runs status CHECK migration");

        assert_eq!(run_status(&db, 1), "failed");
    }

    #[test]
    fn runs_status_check_is_idempotent_on_clean_db() {
        let mut db = open_migrated();
        let report = db
            .migrate(&[V2_RUNS_STATUS_CHECK])
            .expect("re-applying recorded migration is a no-op");
        assert!(report.applied.is_empty());
        assert_eq!(report.skipped, vec![2_026_050_805]);
    }

    #[test]
    fn runs_indexes_survive_runs_status_check_recreate() {
        let db = open_migrated();
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT name FROM sqlite_master \
                  WHERE type='index' AND tbl_name='runs' \
                  ORDER BY name",
            )
            .expect("prepare index lookup");
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query indexes")
            .map(|r| r.expect("row"))
            .collect();
        for expected in ["idx_runs_lease", "idx_runs_status", "idx_runs_work_item"] {
            assert!(
                names.iter().any(|n| n == expected),
                "missing index `{expected}` after recreate; got {names:?}"
            );
        }
    }
}
