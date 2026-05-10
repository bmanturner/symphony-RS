//! Implementation of the `symphony issue` command group (Phase 12 /
//! ARCHITECTURE v2 §6).
//!
//! Today this exposes a single subcommand:
//!
//! * `symphony issue graph <ID>` — print the parent/child/blocker/
//!   follow-up/relates-to graph rooted at a durable work-item id,
//!   reading directly from `work_items` and `work_item_edges`.
//!
//! The command is read-only: it opens the durable SQLite database with
//! [`StateDb::open`] and does not invoke the migration runner. A schema
//! mismatch surfaces as a [`StateError`] rather than a silent upgrade —
//! mutating a database that an operator pointed us at by mistake would
//! be the kind of footgun [`crate::cancel`] is also careful to avoid.
//!
//! ## Why typed edges, not free-form joins
//!
//! The kernel records typed relationships (`EdgeType::ParentChild`,
//! `Blocks`, `RelatesTo`, `FollowupOf`) and the [`WorkItemEdgeRepository`]
//! exposes per-direction listings. The renderer consumes that vocabulary
//! verbatim: for each edge type it prints both the outgoing and incoming
//! views so an operator can answer either "what does this issue affect?"
//! or "what affects this issue?" without reaching for SQL.
//!
//! ## Exit codes
//!
//! - `0` — graph fetched successfully (printed to stdout).
//! - `1` — the requested `<ID>` is not present in `work_items`. Distinct
//!   from a state-DB failure so operators can tell "wrong id" apart from
//!   "wrong DB".
//! - `2` — the durable state database could not be opened or queried.

use std::path::PathBuf;

use symphony_state::edges::{EdgeType, WorkItemEdgeRecord, WorkItemEdgeRepository};
use symphony_state::repository::{WorkItemId, WorkItemRecord, WorkItemRepository};
use symphony_state::{StateDb, StateError};

use crate::cli::{IssueCommand, IssueGraphArgs};

/// Outcome of a `symphony issue graph` invocation.
#[derive(Debug)]
pub enum GraphOutcome {
    /// Root and edges were fetched.
    Ok(Box<GraphSnapshot>),
    /// The requested `<ID>` does not exist in `work_items`.
    NotFound { id: WorkItemId, db_path: PathBuf },
    /// The state database could not be opened or queried.
    StateDbFailed { db_path: PathBuf, error: StateError },
}

impl GraphOutcome {
    /// Stable exit code; see module docs.
    pub fn exit_code(&self) -> i32 {
        match self {
            GraphOutcome::Ok(_) => 0,
            GraphOutcome::NotFound { .. } => 1,
            GraphOutcome::StateDbFailed { .. } => 2,
        }
    }
}

/// Materialised graph snapshot: the root work item plus every edge
/// reachable in one hop, broken down by typed kind and direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSnapshot {
    /// The work item the operator asked about.
    pub root: WorkItemRecord,
    /// Incoming `parent_child` edges — i.e. the parents of `root`.
    /// Normally at most one, but the storage layer does not enforce that
    /// invariant so we surface whatever is there.
    pub parents: Vec<EdgeWithPeer>,
    /// Outgoing `parent_child` edges — i.e. the decomposed children of
    /// `root`.
    pub children: Vec<EdgeWithPeer>,
    /// Incoming `blocks` edges — the items currently blocking `root`.
    /// Entries with `status = "open"` gate parent completion; resolved
    /// rows are kept for audit.
    pub blocked_by: Vec<EdgeWithPeer>,
    /// Outgoing `blocks` edges — the items `root` is blocking.
    pub blocks: Vec<EdgeWithPeer>,
    /// Incoming `followup_of` edges — the issue(s) `root` is a follow-up
    /// of. Typically at most one.
    pub followup_origin: Vec<EdgeWithPeer>,
    /// Outgoing `followup_of` edges — the follow-ups spawned from
    /// `root`.
    pub followups: Vec<EdgeWithPeer>,
    /// Outgoing `relates_to` edges — non-gating links from `root`.
    pub relates_to_out: Vec<EdgeWithPeer>,
    /// Incoming `relates_to` edges — non-gating links to `root`.
    pub relates_to_in: Vec<EdgeWithPeer>,
}

/// One edge plus the peer work item it points at, joined eagerly so the
/// renderer can show a useful label without re-querying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeWithPeer {
    /// The raw edge row.
    pub edge: WorkItemEdgeRecord,
    /// The work item on the *other* end of the edge from the root.
    pub peer: WorkItemRecord,
}

/// Top-level `issue` dispatch: pick a subcommand and run it.
pub fn run(cmd: &IssueCommand) -> GraphOutcome {
    match cmd {
        IssueCommand::Graph(args) => run_graph(args),
    }
}

/// Render the outcome to stdout (success) or stderr (failure) and
/// return the caller's exit code.
pub fn render(outcome: &GraphOutcome) -> i32 {
    match outcome {
        GraphOutcome::Ok(snap) => render_snapshot(snap),
        GraphOutcome::NotFound { id, db_path } => {
            eprintln!(
                "error: work item id {} not found in {}",
                id.0,
                db_path.display(),
            );
        }
        GraphOutcome::StateDbFailed { db_path, error } => {
            eprintln!(
                "error: state DB read failed for {}: {error}",
                db_path.display(),
            );
        }
    }
    outcome.exit_code()
}

/// Build a snapshot for a single `graph` invocation.
pub fn run_graph(args: &IssueGraphArgs) -> GraphOutcome {
    let db = match StateDb::open(&args.state_db) {
        Ok(db) => db,
        Err(error) => {
            return GraphOutcome::StateDbFailed {
                db_path: args.state_db.clone(),
                error,
            };
        }
    };
    let id = WorkItemId(args.id);
    match build_snapshot(&db, id) {
        Ok(Some(snap)) => GraphOutcome::Ok(Box::new(snap)),
        Ok(None) => GraphOutcome::NotFound {
            id,
            db_path: args.state_db.clone(),
        },
        Err(error) => GraphOutcome::StateDbFailed {
            db_path: args.state_db.clone(),
            error,
        },
    }
}

/// Pure assembly: read the root, then enumerate each typed edge in both
/// directions and join the peer record. Returns `Ok(None)` when the
/// root id has no `work_items` row — the caller distinguishes that from
/// a transport failure via the typed `StateError` channel.
fn build_snapshot(db: &StateDb, id: WorkItemId) -> Result<Option<GraphSnapshot>, StateError> {
    let Some(root) = db.get_work_item(id)? else {
        return Ok(None);
    };

    let parents = join_peers(
        db,
        db.list_incoming(id, EdgeType::ParentChild)?,
        Direction::Incoming,
    )?;
    let children = join_peers(
        db,
        db.list_outgoing(id, EdgeType::ParentChild)?,
        Direction::Outgoing,
    )?;
    let blocked_by = join_peers(
        db,
        db.list_incoming(id, EdgeType::Blocks)?,
        Direction::Incoming,
    )?;
    let blocks = join_peers(
        db,
        db.list_outgoing(id, EdgeType::Blocks)?,
        Direction::Outgoing,
    )?;
    let followup_origin = join_peers(
        db,
        db.list_incoming(id, EdgeType::FollowupOf)?,
        Direction::Incoming,
    )?;
    let followups = join_peers(
        db,
        db.list_outgoing(id, EdgeType::FollowupOf)?,
        Direction::Outgoing,
    )?;
    let relates_to_out = join_peers(
        db,
        db.list_outgoing(id, EdgeType::RelatesTo)?,
        Direction::Outgoing,
    )?;
    let relates_to_in = join_peers(
        db,
        db.list_incoming(id, EdgeType::RelatesTo)?,
        Direction::Incoming,
    )?;

    Ok(Some(GraphSnapshot {
        root,
        parents,
        children,
        blocked_by,
        blocks,
        followup_origin,
        followups,
        relates_to_out,
        relates_to_in,
    }))
}

#[derive(Copy, Clone)]
enum Direction {
    Outgoing,
    Incoming,
}

fn join_peers(
    db: &StateDb,
    edges: Vec<WorkItemEdgeRecord>,
    direction: Direction,
) -> Result<Vec<EdgeWithPeer>, StateError> {
    let mut out = Vec::with_capacity(edges.len());
    for edge in edges {
        let peer_id = match direction {
            // Outgoing edges from `root`: peer is the destination.
            Direction::Outgoing => edge.child_id,
            // Incoming edges to `root`: peer is the source.
            Direction::Incoming => edge.parent_id,
        };
        // Foreign keys guarantee the peer exists; an absent row would
        // be a corrupted database. We surface that as an explicit
        // panic rather than silently dropping the edge — the caller's
        // `StateDbFailed` outcome is for transport problems, not
        // invariant violations.
        let peer = db
            .get_work_item(peer_id)?
            .expect("FK guarantees peer work item exists");
        out.push(EdgeWithPeer { edge, peer });
    }
    Ok(out)
}

fn render_snapshot(snap: &GraphSnapshot) {
    let r = &snap.root;
    println!(
        "issue {}  [{}]  {}/{}  {}",
        r.id.0, r.status_class, r.tracker_id, r.identifier, r.title,
    );

    section(
        "parents (parent_child, incoming)",
        &snap.parents,
        ArrowDir::From,
    );
    section(
        "children (parent_child, outgoing)",
        &snap.children,
        ArrowDir::To,
    );
    section(
        "blocked_by (blocks, incoming)",
        &snap.blocked_by,
        ArrowDir::From,
    );
    section("blocks (blocks, outgoing)", &snap.blocks, ArrowDir::To);
    section(
        "followup_of (incoming — root is a follow-up of)",
        &snap.followup_origin,
        ArrowDir::From,
    );
    section(
        "followups (outgoing — spawned from root)",
        &snap.followups,
        ArrowDir::To,
    );
    section("relates_to (outgoing)", &snap.relates_to_out, ArrowDir::To);
    section("relates_to (incoming)", &snap.relates_to_in, ArrowDir::From);
}

#[derive(Copy, Clone)]
enum ArrowDir {
    /// Edge points away from the root (`root → peer`).
    To,
    /// Edge points at the root (`peer → root`).
    From,
}

impl ArrowDir {
    fn glyph(self) -> &'static str {
        match self {
            ArrowDir::To => "→",
            ArrowDir::From => "←",
        }
    }
}

fn section(title: &str, edges: &[EdgeWithPeer], dir: ArrowDir) {
    if edges.is_empty() {
        return;
    }
    println!("  {title}:");
    let arrow = dir.glyph();
    for e in edges {
        let peer = &e.peer;
        let reason = e
            .edge
            .reason
            .as_deref()
            .map(|r| format!("  ({r})"))
            .unwrap_or_default();
        println!(
            "    {arrow} {}  [{}]  [{}]  {}/{}  {}{}",
            peer.id.0,
            e.edge.status,
            peer.status_class,
            peer.tracker_id,
            peer.identifier,
            peer.title,
            reason,
        );
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_state::edges::{EdgeSource, EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewWorkItem, WorkItemRepository};

    fn fresh_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut db = StateDb::open(&path).unwrap();
        db.migrate(migrations()).unwrap();
        (dir, path)
    }

    fn seed(db: &mut StateDb, identifier: &str, title: &str, status_class: &str) -> WorkItemId {
        db.create_work_item(NewWorkItem {
            tracker_id: "linear",
            identifier,
            parent_id: None,
            title,
            status_class,
            tracker_status: "todo",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-05-08T00:00:00Z",
        })
        .unwrap()
        .id
    }

    fn link(
        db: &mut StateDb,
        p: WorkItemId,
        c: WorkItemId,
        kind: EdgeType,
        status: &str,
        reason: Option<&str>,
    ) {
        db.create_edge(NewWorkItemEdge {
            parent_id: p,
            child_id: c,
            edge_type: kind,
            reason,
            status,
            source: EdgeSource::Unknown,
            now: "2026-05-08T00:00:00Z",
        })
        .unwrap();
    }

    #[test]
    fn missing_root_yields_not_found_with_exit_code_one() {
        let (_dir, path) = fresh_db();
        let outcome = run_graph(&IssueGraphArgs {
            id: 99_999,
            state_db: path.clone(),
        });
        match outcome {
            GraphOutcome::NotFound { id, db_path } => {
                assert_eq!(id, WorkItemId(99_999));
                assert_eq!(db_path, path);
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn unreadable_state_db_yields_state_db_failed() {
        let outcome = run_graph(&IssueGraphArgs {
            id: 1,
            state_db: PathBuf::from("/no/such/dir/state.db"),
        });
        match outcome {
            GraphOutcome::StateDbFailed { .. } => {}
            other => panic!("expected StateDbFailed, got {other:?}"),
        }
        assert_eq!(outcome.exit_code(), 2);
    }

    #[test]
    fn isolated_root_returns_empty_sections_and_exit_zero() {
        let (_dir, path) = fresh_db();
        let id = {
            let mut db = StateDb::open(&path).unwrap();
            seed(&mut db, "ENG-1", "lonely", "ready")
        };
        let outcome = run_graph(&IssueGraphArgs {
            id: id.0,
            state_db: path,
        });
        let snap = match outcome {
            GraphOutcome::Ok(s) => s,
            other => panic!("expected Ok, got {other:?}"),
        };
        assert_eq!(snap.root.identifier, "ENG-1");
        assert!(snap.parents.is_empty());
        assert!(snap.children.is_empty());
        assert!(snap.blocked_by.is_empty());
        assert!(snap.blocks.is_empty());
        assert!(snap.followup_origin.is_empty());
        assert!(snap.followups.is_empty());
        assert!(snap.relates_to_in.is_empty());
        assert!(snap.relates_to_out.is_empty());
    }

    /// Every edge kind in both directions surfaces in its proper bucket
    /// with the peer joined in. This is the "shape contract" test — if
    /// future refactors reroute edges through a different traversal,
    /// the buckets must still match.
    #[test]
    fn graph_buckets_every_edge_kind_in_correct_direction() {
        let (_dir, path) = fresh_db();
        let (
            root,
            parent,
            child,
            blocker,
            blocked,
            followup_origin,
            followup,
            related_out,
            related_in,
        );
        {
            let mut db = StateDb::open(&path).unwrap();
            root = seed(&mut db, "ENG-1", "root", "ready");
            parent = seed(&mut db, "ENG-2", "parent", "ready");
            child = seed(&mut db, "ENG-3", "child", "in_progress");
            blocker = seed(&mut db, "ENG-4", "blocker", "ready");
            blocked = seed(&mut db, "ENG-5", "blocked", "ready");
            followup_origin = seed(&mut db, "ENG-6", "origin", "done");
            followup = seed(&mut db, "ENG-7", "followup", "ready");
            related_out = seed(&mut db, "ENG-8", "related-out", "ready");
            related_in = seed(&mut db, "ENG-9", "related-in", "ready");

            link(&mut db, parent, root, EdgeType::ParentChild, "linked", None);
            link(&mut db, root, child, EdgeType::ParentChild, "linked", None);
            link(
                &mut db,
                blocker,
                root,
                EdgeType::Blocks,
                "open",
                Some("schema first"),
            );
            link(
                &mut db,
                root,
                blocked,
                EdgeType::Blocks,
                "resolved",
                Some("done"),
            );
            link(
                &mut db,
                followup_origin,
                root,
                EdgeType::FollowupOf,
                "linked",
                None,
            );
            link(
                &mut db,
                root,
                followup,
                EdgeType::FollowupOf,
                "linked",
                Some("logging gap"),
            );
            link(
                &mut db,
                root,
                related_out,
                EdgeType::RelatesTo,
                "linked",
                None,
            );
            link(
                &mut db,
                related_in,
                root,
                EdgeType::RelatesTo,
                "linked",
                None,
            );
        }

        let outcome = run_graph(&IssueGraphArgs {
            id: root.0,
            state_db: path,
        });
        let snap = match outcome {
            GraphOutcome::Ok(s) => s,
            other => panic!("expected Ok, got {other:?}"),
        };

        let peer_ids = |v: &Vec<EdgeWithPeer>| v.iter().map(|e| e.peer.id).collect::<Vec<_>>();
        assert_eq!(peer_ids(&snap.parents), vec![parent]);
        assert_eq!(peer_ids(&snap.children), vec![child]);
        assert_eq!(peer_ids(&snap.blocked_by), vec![blocker]);
        assert_eq!(peer_ids(&snap.blocks), vec![blocked]);
        assert_eq!(peer_ids(&snap.followup_origin), vec![followup_origin]);
        assert_eq!(peer_ids(&snap.followups), vec![followup]);
        assert_eq!(peer_ids(&snap.relates_to_out), vec![related_out]);
        assert_eq!(peer_ids(&snap.relates_to_in), vec![related_in]);

        // Spot-check: the resolved blocker is preserved in the bucket
        // (the renderer surfaces it for audit) and reasons round-trip.
        let resolved = &snap.blocks[0];
        assert_eq!(resolved.edge.status, "resolved");
        assert_eq!(resolved.edge.reason.as_deref(), Some("done"));
        let open_blocker = &snap.blocked_by[0];
        assert_eq!(open_blocker.edge.status, "open");
        assert_eq!(open_blocker.edge.reason.as_deref(), Some("schema first"));
    }

    #[test]
    fn render_returns_zero_on_ok_outcome() {
        let (_dir, path) = fresh_db();
        let id = {
            let mut db = StateDb::open(&path).unwrap();
            seed(&mut db, "ENG-1", "lonely", "ready")
        };
        let outcome = run_graph(&IssueGraphArgs {
            id: id.0,
            state_db: path,
        });
        assert_eq!(render(&outcome), 0);
    }
}
