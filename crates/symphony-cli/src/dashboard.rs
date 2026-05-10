//! Operator dashboard TUI for `symphony dashboard` (Phase 12 / SPEC v2 §6).
//!
//! Unlike [`crate::watch`], which streams orchestrator events live over
//! SSE, this dashboard reads the durable SQLite store directly and
//! presents five operator-facing tabs:
//!
//! 1. **queues** — the integration and QA dispatch queues, surfaced
//!    via [`IntegrationQueueRepository::list_ready_for_integration`]
//!    and [`QaQueueRepository::list_ready_for_qa`].
//! 2. **blockers** — every open `blocks` edge across the work-item
//!    graph, derived from
//!    [`WorkItemEdgeRepository::list_open_blockers_for_subtree`].
//! 3. **qa** — the most-recent [`QaVerdictRecord`] per active work item.
//! 4. **integration** — the most-recent [`IntegrationRecordRow`] per
//!    active parent, including the canonical integration branch.
//! 5. **runs** — every [`RunRecord`] attached to an active work item,
//!    newest first.
//!
//! The tab body is a [`ratatui::widgets::Table`]; tabs are switched
//! with `1`–`5`, `Tab` / `Shift+Tab`, or the left/right arrow keys.
//! `q` / `Esc` / `Ctrl+C` quit. There is no live event stream — the
//! data refreshes on a timer (default every 2 s) and on demand via `r`.
//!
//! ## Why a separate command from `watch`
//!
//! `watch` exists to surface *what is happening right now* — a fast,
//! live, operator's heads-up display sourced from the SSE event tail.
//! The dashboard answers *what is the state of the system* from durable
//! storage. The two reads are intentionally distinct: when the daemon
//! is offline, `watch` cannot connect; the dashboard still works.
//!
//! ## Testing
//!
//! Rendering is split from terminal I/O, so unit tests drive the pure
//! [`render`] function against `ratatui::backend::TestBackend` with a
//! fixture [`DashboardData`]. Data loading is tested against an
//! in-memory [`StateDb`] populated through the public repository
//! traits.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs};
use symphony_state::edges::{EdgeType, WorkItemEdgeRecord, WorkItemEdgeRepository};
use symphony_state::integration_queue::{IntegrationQueueEntry, IntegrationQueueRepository};
use symphony_state::integration_records::{IntegrationRecordRepository, IntegrationRecordRow};
use symphony_state::qa_queue::{QaQueueEntry, QaQueueRepository};
use symphony_state::qa_verdicts::{QaVerdictRecord, QaVerdictRepository};
use symphony_state::repository::{RunRecord, RunRepository, WorkItemRecord, WorkItemRepository};
use symphony_state::{StateDb, StateError, StateResult};

/// Top-level outcome of a `symphony dashboard` invocation.
///
/// The TUI proper does not return a snapshot — it owns the terminal
/// until the operator quits — so this enum is dominated by failure
/// modes the caller (and tests) want to discriminate.
#[derive(Debug)]
pub enum DashboardOutcome {
    /// The TUI ran to completion (operator quit cleanly).
    Ok,
    /// The durable SQLite store at the configured path could not be
    /// opened or queried. Mirrors `symphony status`'s exit code 4 so
    /// CI scripts can tell config problems apart from data-plane
    /// problems with one comparison.
    StateDbFailed(StateError),
    /// Terminal I/O failed (raw mode toggle, draw, or input read).
    /// Surfaced separately because it is operationally distinct from a
    /// data-plane failure.
    TerminalFailed(io::Error),
}

impl DashboardOutcome {
    /// Map an outcome to its stable exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            DashboardOutcome::Ok => 0,
            DashboardOutcome::StateDbFailed(_) => 4,
            DashboardOutcome::TerminalFailed(_) => 5,
        }
    }
}

/// One of the five operator tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// The `1` tab: integration + QA queue contents.
    #[default]
    Queues,
    /// The `2` tab: every open `blocks` edge.
    Blockers,
    /// The `3` tab: the latest QA verdict per active work item.
    Qa,
    /// The `4` tab: the latest integration record per active work item.
    Integration,
    /// The `5` tab: durable runs across active work items.
    Runs,
}

impl Tab {
    /// Stable index used for `1`–`5` shortcuts and tab-bar highlight.
    pub fn index(self) -> usize {
        match self {
            Tab::Queues => 0,
            Tab::Blockers => 1,
            Tab::Qa => 2,
            Tab::Integration => 3,
            Tab::Runs => 4,
        }
    }

    /// Inverse of [`Tab::index`]; returns `None` for out-of-range values so
    /// the key handler can ignore unbound digits without panicking.
    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Tab::Queues),
            1 => Some(Tab::Blockers),
            2 => Some(Tab::Qa),
            3 => Some(Tab::Integration),
            4 => Some(Tab::Runs),
            _ => None,
        }
    }

    /// Short label rendered in the tab bar.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Queues => "1 queues",
            Tab::Blockers => "2 blockers",
            Tab::Qa => "3 qa",
            Tab::Integration => "4 integration",
            Tab::Runs => "5 runs",
        }
    }

    /// All tabs in display order.
    pub fn all() -> [Tab; 5] {
        [
            Tab::Queues,
            Tab::Blockers,
            Tab::Qa,
            Tab::Integration,
            Tab::Runs,
        ]
    }

    /// Cycle to the next tab (wrap-around). Used by `Tab` / right arrow.
    pub fn next(self) -> Self {
        Tab::from_index((self.index() + 1) % Tab::all().len()).expect("modulo always in range")
    }

    /// Cycle to the previous tab (wrap-around). Used by `Shift+Tab` /
    /// left arrow.
    pub fn prev(self) -> Self {
        let len = Tab::all().len();
        Tab::from_index((self.index() + len - 1) % len).expect("modulo always in range")
    }
}

/// One blocker row materialized for the blockers panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerRow {
    /// The underlying edge record.
    pub edge: WorkItemEdgeRecord,
    /// Tracker identifier of the blocking work item (`edge.parent_id`).
    pub blocker_identifier: String,
    /// Tracker identifier of the blocked work item (`edge.child_id`).
    pub blocked_identifier: String,
}

/// One QA-tab row pairing a work item with its newest verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QaRow {
    /// The work item the verdict was filed against.
    pub work_item: WorkItemRecord,
    /// The newest verdict on file. `None` means no verdict has been
    /// filed yet — surfaced in the panel as a blank row so the operator
    /// can still see which active items lack QA evidence.
    pub latest: Option<QaVerdictRecord>,
}

/// One integration-tab row pairing a parent with its newest integration
/// record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationRow {
    /// The parent work item being integrated.
    pub work_item: WorkItemRecord,
    /// The newest integration record on file, if any.
    pub latest: Option<IntegrationRecordRow>,
}

/// One runs-tab row pairing a run with its owning work item's
/// identifier.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    /// The persisted run.
    pub run: RunRecord,
    /// Tracker identifier of the owning work item, for display only.
    pub work_item_identifier: String,
}

/// Aggregated read of the durable store, snapshot-style.
///
/// The dashboard re-issues this query on a timer and on demand. Each
/// field is independent — a query failure on one tab does not abort
/// the others; instead the failure is captured in [`DashboardData::errors`]
/// and the affected tab body shows an inline error line.
#[derive(Debug, Default, Clone)]
pub struct DashboardData {
    /// Integration queue entries, FIFO.
    pub integration_queue: Vec<IntegrationQueueEntry>,
    /// QA queue entries, FIFO.
    pub qa_queue: Vec<QaQueueEntry>,
    /// Open blocker edges, deduped by edge id.
    pub blockers: Vec<BlockerRow>,
    /// Active work items + their newest QA verdict.
    pub qa: Vec<QaRow>,
    /// Active parent work items + their newest integration record.
    pub integrations: Vec<IntegrationRow>,
    /// Recent runs across active work items, newest first.
    pub runs: Vec<RunRow>,
    /// Per-tab error captured during the last load. The dashboard does
    /// not abort on a tab failure because the operator may still want
    /// to see the others.
    pub errors: DashboardErrors,
    /// RFC3339-ish best-effort wall-clock the data was loaded at, in
    /// `HH:MM:SS` form. Populated by [`DashboardData::with_loaded_at`].
    pub loaded_at: Option<String>,
}

/// Per-tab failure captured during a dashboard refresh. Each `Option`
/// carries the `StateError::to_string` of the failed query.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DashboardErrors {
    /// Failure message for the queues tab, if any.
    pub queues: Option<String>,
    /// Failure message for the blockers tab, if any.
    pub blockers: Option<String>,
    /// Failure message for the qa tab, if any.
    pub qa: Option<String>,
    /// Failure message for the integration tab, if any.
    pub integration: Option<String>,
    /// Failure message for the runs tab, if any.
    pub runs: Option<String>,
}

impl DashboardData {
    /// Stamp a load timestamp for the footer. Kept as a separate
    /// builder so [`load`] does not depend on a clock — tests pass a
    /// frozen value, production passes `chrono::Local::now`-formatted
    /// `HH:MM:SS`.
    pub fn with_loaded_at(mut self, hms: impl Into<String>) -> Self {
        self.loaded_at = Some(hms.into());
        self
    }
}

/// Top-level TUI state. `should_quit` is checked at the bottom of the
/// event loop so a `q` press during a refresh is honored on the next
/// iteration without races.
#[derive(Debug, Default, Clone)]
pub struct DashboardState {
    /// Currently active tab.
    pub active: Tab,
    /// Last loaded data snapshot.
    pub data: DashboardData,
    /// Set by [`DashboardState::apply_key`] when the operator presses `q` / `Esc` /
    /// `Ctrl+C`; the run loop checks this after every input event.
    pub should_quit: bool,
}

impl DashboardState {
    /// Apply one keyboard event. Returns `true` when the input mutated
    /// state in a way that requires a redraw — used by the run loop to
    /// avoid redundant frame calls when the input is a no-op (mouse
    /// motion, paste, etc).
    pub fn apply_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                self.should_quit = true;
                true
            }
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                true
            }
            (KeyCode::Tab, _) | (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                self.active = self.active.next();
                true
            }
            (KeyCode::BackTab, _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                self.active = self.active.prev();
                true
            }
            (KeyCode::Char(c), _) if c.is_ascii_digit() => {
                if let Some(n) = c.to_digit(10)
                    && let Some(t) = Tab::from_index((n as usize).saturating_sub(1))
                {
                    self.active = t;
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}

/// Load every tab's data from the durable store in one pass.
///
/// Each query is independent: a failure on one populates the matching
/// [`DashboardErrors`] field and leaves the others intact. The function
/// itself returns [`StateResult`] for the rare case where the work-item
/// list itself fails — without that anchor the QA, integration, and
/// runs tabs have nothing to iterate.
pub fn load(db: &StateDb) -> StateResult<DashboardData> {
    let active_items = db.list_active_work_items()?;
    let mut data = DashboardData::default();

    match db.list_ready_for_integration() {
        Ok(rows) => data.integration_queue = rows,
        Err(err) => data.errors.queues = Some(format!("integration queue: {err}")),
    }
    match db.list_ready_for_qa() {
        Ok(rows) => data.qa_queue = rows,
        Err(err) => {
            // Append rather than overwrite so a double-failure surfaces
            // both messages to the operator.
            let suffix = format!("qa queue: {err}");
            data.errors.queues = Some(match data.errors.queues.take() {
                Some(prev) => format!("{prev}; {suffix}"),
                None => suffix,
            });
        }
    }

    let mut blockers: Vec<BlockerRow> = Vec::new();
    let mut blocker_err: Option<String> = None;
    let mut seen_edge_ids = std::collections::HashSet::new();
    for wi in &active_items {
        match db.list_open_blockers_for_subtree(wi.id) {
            Ok(rows) => {
                for edge in rows {
                    if !seen_edge_ids.insert(edge.id.0) {
                        continue;
                    }
                    blockers.push(BlockerRow {
                        blocker_identifier: identifier_for(db, edge.parent_id.0),
                        blocked_identifier: identifier_for(db, edge.child_id.0),
                        edge,
                    });
                }
            }
            Err(err) => {
                blocker_err = Some(format!("blockers: {err}"));
                break;
            }
        }
    }
    data.blockers = blockers;
    data.errors.blockers = blocker_err;

    let mut qa_rows: Vec<QaRow> = Vec::new();
    let mut qa_err: Option<String> = None;
    for wi in &active_items {
        match db.list_qa_verdicts_for_work_item(wi.id) {
            Ok(mut v) => {
                let latest = if v.is_empty() {
                    None
                } else {
                    Some(v.remove(0))
                };
                qa_rows.push(QaRow {
                    work_item: wi.clone(),
                    latest,
                });
            }
            Err(err) => {
                qa_err = Some(format!("qa: {err}"));
                break;
            }
        }
    }
    data.qa = qa_rows;
    data.errors.qa = qa_err;

    let mut integ_rows: Vec<IntegrationRow> = Vec::new();
    let mut integ_err: Option<String> = None;
    for wi in &active_items {
        match db.list_integration_records_for_parent(wi.id) {
            Ok(v) => {
                let latest = v.into_iter().last();
                integ_rows.push(IntegrationRow {
                    work_item: wi.clone(),
                    latest,
                });
            }
            Err(err) => {
                integ_err = Some(format!("integration: {err}"));
                break;
            }
        }
    }
    data.integrations = integ_rows;
    data.errors.integration = integ_err;

    let mut runs: Vec<RunRow> = Vec::new();
    let mut runs_err: Option<String> = None;
    for wi in &active_items {
        match db.list_runs_for_work_item(wi.id) {
            Ok(v) => {
                for r in v {
                    runs.push(RunRow {
                        run: r,
                        work_item_identifier: wi.identifier.clone(),
                    });
                }
            }
            Err(err) => {
                runs_err = Some(format!("runs: {err}"));
                break;
            }
        }
    }
    // Newest first by created_at, then by id desc for determinism.
    runs.sort_by(|a, b| {
        b.run
            .created_at
            .cmp(&a.run.created_at)
            .then(b.run.id.0.cmp(&a.run.id.0))
    });
    data.runs = runs;
    data.errors.runs = runs_err;

    Ok(data)
}

/// Best-effort identifier lookup for a work item id. Used by the
/// blockers panel to surface tracker identifiers (`ENG-101`) instead of
/// raw numeric ids — the durable edge row only carries ids. On query
/// failure we fall back to `#<id>` so the row still renders.
fn identifier_for(db: &StateDb, id: i64) -> String {
    match db.get_work_item(symphony_state::repository::WorkItemId(id)) {
        Ok(Some(wi)) => wi.identifier,
        _ => format!("#{id}"),
    }
}

/// Render one frame. Pure function; tests drive this with
/// `ratatui::backend::TestBackend`.
pub fn render(state: &DashboardState, frame: &mut Frame<'_>) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // tab bar
            Constraint::Min(5),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_tab_bar(state, frame, chunks[0]);
    render_body(state, frame, chunks[1]);
    render_footer(state, frame, chunks[2]);
}

fn render_tab_bar(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let titles: Vec<Line<'_>> = Tab::all()
        .into_iter()
        .map(|t| Line::from(t.label()))
        .collect();
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("symphony dashboard"),
        )
        .select(state.active.index())
        .style(Style::default().fg(Color::Gray))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn render_body(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    match state.active {
        Tab::Queues => render_queues(state, frame, area),
        Tab::Blockers => render_blockers(state, frame, area),
        Tab::Qa => render_qa(state, frame, area),
        Tab::Integration => render_integration(state, frame, area),
        Tab::Runs => render_runs(state, frame, area),
    }
}

fn render_queues(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    if let Some(err) = &state.data.errors.queues {
        return render_error(frame, area, "queues", err);
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    for entry in &state.data.integration_queue {
        rows.push(Row::new(vec![
            Cell::from("integration"),
            Cell::from(entry.work_item.identifier.clone()),
            Cell::from(entry.work_item.title.clone()),
            Cell::from(entry.cause.as_str()),
        ]));
    }
    for entry in &state.data.qa_queue {
        rows.push(Row::new(vec![
            Cell::from("qa"),
            Cell::from(entry.work_item.identifier.clone()),
            Cell::from(entry.work_item.title.clone()),
            Cell::from(entry.cause.as_str()),
        ]));
    }
    if rows.is_empty() {
        return render_empty(
            frame,
            area,
            &format!(
                "queues — integration:{} qa:{}",
                state.data.integration_queue.len(),
                state.data.qa_queue.len()
            ),
            "no items ready for integration or qa",
        );
    }
    let header = Row::new(vec!["queue", "id", "title", "cause"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(12),
        Constraint::Length(20),
        Constraint::Min(30),
        Constraint::Length(28),
    ];
    let block = Block::default().borders(Borders::ALL).title(format!(
        "queues — integration:{} qa:{}",
        state.data.integration_queue.len(),
        state.data.qa_queue.len()
    ));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_blockers(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    if let Some(err) = &state.data.errors.blockers {
        return render_error(frame, area, "blockers", err);
    }
    let rows: Vec<Row<'_>> = state
        .data
        .blockers
        .iter()
        .map(|b| {
            Row::new(vec![
                Cell::from(b.edge.id.0.to_string()),
                Cell::from(b.blocker_identifier.clone()),
                Cell::from(b.blocked_identifier.clone()),
                Cell::from(b.edge.status.clone()),
                Cell::from(b.edge.reason.clone().unwrap_or_default()),
            ])
        })
        .collect();
    if rows.is_empty() {
        return render_empty(frame, area, "blockers — open:0", "no open blockers");
    }
    let header = Row::new(vec!["edge", "blocker", "blocks", "status", "reason"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(6),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(8),
        Constraint::Min(20),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("blockers — open:{}", state.data.blockers.len()));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_qa(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    if let Some(err) = &state.data.errors.qa {
        return render_error(frame, area, "qa", err);
    }
    let rows: Vec<Row<'_>> = state
        .data
        .qa
        .iter()
        .map(|q| {
            let (verdict, role, when) = match &q.latest {
                Some(v) => (v.verdict.clone(), v.role.clone(), v.created_at.clone()),
                None => ("—".to_string(), String::new(), String::new()),
            };
            Row::new(vec![
                Cell::from(q.work_item.identifier.clone()),
                Cell::from(q.work_item.status_class.clone()),
                Cell::from(verdict),
                Cell::from(role),
                Cell::from(when),
            ])
        })
        .collect();
    if rows.is_empty() {
        return render_empty(frame, area, "qa — items:0", "no active work items");
    }
    let header = Row::new(vec!["id", "status", "verdict", "role", "at"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(22),
        Constraint::Length(14),
        Constraint::Min(20),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("qa — items:{}", state.data.qa.len()));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_integration(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    if let Some(err) = &state.data.errors.integration {
        return render_error(frame, area, "integration", err);
    }
    let rows: Vec<Row<'_>> = state
        .data
        .integrations
        .iter()
        .map(|i| {
            let (status, branch, summary) = match &i.latest {
                Some(r) => (
                    r.status.clone(),
                    r.integration_branch.clone().unwrap_or_default(),
                    r.summary.clone().unwrap_or_default(),
                ),
                None => (String::new(), String::new(), String::new()),
            };
            Row::new(vec![
                Cell::from(i.work_item.identifier.clone()),
                Cell::from(status),
                Cell::from(branch),
                Cell::from(summary),
            ])
        })
        .collect();
    if rows.is_empty() {
        return render_empty(
            frame,
            area,
            "integration — items:0",
            "no active parent work items",
        );
    }
    let header = Row::new(vec!["parent", "status", "branch", "summary"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(20),
        Constraint::Length(14),
        Constraint::Length(28),
        Constraint::Min(20),
    ];
    let block = Block::default().borders(Borders::ALL).title(format!(
        "integration — items:{}",
        state.data.integrations.len()
    ));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_runs(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    if let Some(err) = &state.data.errors.runs {
        return render_error(frame, area, "runs", err);
    }
    let rows: Vec<Row<'_>> = state
        .data
        .runs
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.run.id.0.to_string()),
                Cell::from(r.work_item_identifier.clone()),
                Cell::from(r.run.role.clone()),
                Cell::from(r.run.agent.clone()),
                Cell::from(r.run.status.clone()),
                Cell::from(r.run.created_at.clone()),
            ])
        })
        .collect();
    if rows.is_empty() {
        return render_empty(
            frame,
            area,
            "runs — total:0",
            "no runs against active work items",
        );
    }
    let header = Row::new(vec![
        "id",
        "work item",
        "role",
        "agent",
        "status",
        "created",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let widths = [
        Constraint::Length(6),
        Constraint::Length(18),
        Constraint::Length(16),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(20),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("runs — total:{}", state.data.runs.len()));
    let table = Table::new(rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

fn render_footer(state: &DashboardState, frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let when = state.data.loaded_at.as_deref().unwrap_or("—");
    let line = format!(" loaded {when}  ·  1-5 tab  Tab/⇆ cycle  r refresh  q quit");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            line,
            Style::default().fg(Color::DarkGray),
        ))),
        area,
    );
}

fn render_error(frame: &mut Frame<'_>, area: ratatui::layout::Rect, tab: &str, err: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("{tab} — error"));
    let p = Paragraph::new(format!("{tab} query failed: {err}"))
        .style(Style::default().fg(Color::Red))
        .block(block);
    frame.render_widget(p, area);
}

fn render_empty(frame: &mut Frame<'_>, area: ratatui::layout::Rect, title: &str, msg: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let p = Paragraph::new(msg.to_string())
        .style(Style::default().fg(Color::DarkGray))
        .block(block);
    frame.render_widget(p, area);
}

/// Synchronous entry point. Owns the terminal until the operator quits.
pub fn run(state_db: &Path) -> DashboardOutcome {
    // Open and migrate-check up front so a bad DB path fails before the
    // alternate screen is entered. Mirrors `symphony recover`'s policy.
    let db = match StateDb::open(state_db) {
        Ok(db) => db,
        Err(err) => return DashboardOutcome::StateDbFailed(err),
    };

    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(err) => return DashboardOutcome::TerminalFailed(err),
    };
    let outcome = run_inner(&db, &mut terminal);
    ratatui::restore();
    match outcome {
        Ok(()) => DashboardOutcome::Ok,
        Err(RunError::Db(e)) => DashboardOutcome::StateDbFailed(e),
        Err(RunError::Io(e)) => DashboardOutcome::TerminalFailed(e),
    }
}

#[derive(Debug)]
enum RunError {
    Db(StateError),
    Io(io::Error),
}

impl From<io::Error> for RunError {
    fn from(e: io::Error) -> Self {
        RunError::Io(e)
    }
}

fn run_inner(db: &StateDb, terminal: &mut ratatui::DefaultTerminal) -> Result<(), RunError> {
    const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    let mut state = DashboardState {
        data: load(db).map_err(RunError::Db)?.with_loaded_at(now_hms()),
        ..Default::default()
    };
    let mut last_refresh = Instant::now();

    terminal.draw(|f| render(&state, f))?;

    loop {
        if crossterm::event::poll(POLL_INTERVAL)? {
            match crossterm::event::read()? {
                Event::Key(k) => {
                    if k.kind == KeyEventKind::Press && matches!(k.code, KeyCode::Char('r')) {
                        match load(db) {
                            Ok(d) => state.data = d.with_loaded_at(now_hms()),
                            Err(e) => return Err(RunError::Db(e)),
                        }
                        last_refresh = Instant::now();
                        terminal.draw(|f| render(&state, f))?;
                        continue;
                    }
                    if state.apply_key(k) {
                        terminal.draw(|f| render(&state, f))?;
                    }
                }
                Event::Resize(_, _) => {
                    terminal.draw(|f| render(&state, f))?;
                }
                _ => {}
            }
        }

        if state.should_quit {
            break;
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match load(db) {
                Ok(d) => state.data = d.with_loaded_at(now_hms()),
                Err(e) => return Err(RunError::Db(e)),
            }
            last_refresh = Instant::now();
            terminal.draw(|f| render(&state, f))?;
        }
    }

    Ok(())
}

fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Best-effort wall-clock formatting without pulling in `chrono`.
    // Wall-clock is for the footer label only; it is not a security
    // boundary or an audit timestamp, so an approximate `HH:MM:SS`
    // computed from the unix-epoch second count is sufficient.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

// `EdgeType` is imported for future use by render passes that label
// blocker rows by kind; the current renderer only needs the variants
// indirectly via `WorkItemEdgeRecord::edge_type`.
#[allow(dead_code)]
fn _edge_kind_label(t: EdgeType) -> &'static str {
    t.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use symphony_state::edges::{EdgeSource, EdgeType, NewWorkItemEdge, WorkItemEdgeRepository};
    use symphony_state::migrations::migrations;
    use symphony_state::repository::{NewRun, NewWorkItem, RunRepository, WorkItemRepository};

    fn open_db() -> StateDb {
        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn tab_cycle_wraps_forward_and_backward() {
        assert_eq!(Tab::Queues.next(), Tab::Blockers);
        assert_eq!(Tab::Runs.next(), Tab::Queues);
        assert_eq!(Tab::Queues.prev(), Tab::Runs);
        assert_eq!(Tab::Blockers.prev(), Tab::Queues);
    }

    #[test]
    fn apply_key_q_sets_quit() {
        let mut s = DashboardState::default();
        s.apply_key(key(KeyCode::Char('q')));
        assert!(s.should_quit);
    }

    #[test]
    fn apply_key_digits_select_tab() {
        let mut s = DashboardState::default();
        s.apply_key(key(KeyCode::Char('3')));
        assert_eq!(s.active, Tab::Qa);
        s.apply_key(key(KeyCode::Char('5')));
        assert_eq!(s.active, Tab::Runs);
        // Out-of-range digit is a no-op.
        s.apply_key(key(KeyCode::Char('9')));
        assert_eq!(s.active, Tab::Runs);
    }

    #[test]
    fn apply_key_tab_cycles_forward() {
        let mut s = DashboardState::default();
        assert_eq!(s.active, Tab::Queues);
        s.apply_key(key(KeyCode::Tab));
        assert_eq!(s.active, Tab::Blockers);
        s.apply_key(key(KeyCode::BackTab));
        assert_eq!(s.active, Tab::Queues);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut s = DashboardState::default();
        s.apply_key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        assert!(s.should_quit);
    }

    fn seed_work_item(db: &mut StateDb, identifier: &str, status_class: &str) -> WorkItemRecord {
        db.create_work_item(NewWorkItem {
            tracker_id: "github",
            identifier,
            parent_id: None,
            title: &format!("title for {identifier}"),
            status_class,
            tracker_status: "open",
            assigned_role: None,
            assigned_agent: None,
            priority: None,
            workspace_policy: None,
            branch_policy: None,
            now: "2026-01-01T00:00:00Z",
        })
        .expect("create work item")
    }

    #[test]
    fn load_returns_empty_data_for_empty_db() {
        let db = open_db();
        let data = load(&db).expect("load");
        assert!(data.integration_queue.is_empty());
        assert!(data.qa_queue.is_empty());
        assert!(data.blockers.is_empty());
        assert!(data.qa.is_empty());
        assert!(data.integrations.is_empty());
        assert!(data.runs.is_empty());
        assert_eq!(data.errors, DashboardErrors::default());
    }

    #[test]
    fn load_surfaces_integration_queue_entries() {
        let mut db = open_db();
        seed_work_item(&mut db, "ENG-100", "integration");
        let data = load(&db).expect("load");
        assert_eq!(data.integration_queue.len(), 1);
        assert_eq!(data.integration_queue[0].work_item.identifier, "ENG-100");
    }

    #[test]
    fn load_surfaces_open_blockers_with_identifiers() {
        let mut db = open_db();
        let blocker = seed_work_item(&mut db, "ENG-200", "in_progress");
        let blocked = seed_work_item(&mut db, "ENG-201", "in_progress");
        db.create_edge(NewWorkItemEdge {
            parent_id: blocker.id,
            child_id: blocked.id,
            edge_type: EdgeType::Blocks,
            reason: Some("smoke test failed"),
            status: "open",
            source: EdgeSource::Unknown,
            now: "2026-01-01T00:00:00Z",
        })
        .expect("edge");

        let data = load(&db).expect("load");
        assert_eq!(data.blockers.len(), 1);
        assert_eq!(data.blockers[0].blocker_identifier, "ENG-200");
        assert_eq!(data.blockers[0].blocked_identifier, "ENG-201");
        assert_eq!(data.blockers[0].edge.status, "open");
    }

    #[test]
    fn load_surfaces_runs_for_active_work_items() {
        let mut db = open_db();
        let wi = seed_work_item(&mut db, "ENG-300", "in_progress");
        db.create_run(NewRun {
            work_item_id: wi.id,
            role: "specialist:ui",
            agent: "claude",
            status: "queued",
            workspace_claim_id: None,
            now: "2026-01-01T00:00:01Z",
        })
        .expect("run");
        db.create_run(NewRun {
            work_item_id: wi.id,
            role: "qa",
            agent: "codex",
            status: "running",
            workspace_claim_id: None,
            now: "2026-01-01T00:00:02Z",
        })
        .expect("run");

        let data = load(&db).expect("load");
        assert_eq!(data.runs.len(), 2);
        // Newest first.
        assert_eq!(data.runs[0].run.role, "qa");
        assert_eq!(data.runs[1].run.role, "specialist:ui");
        assert_eq!(data.runs[0].work_item_identifier, "ENG-300");
    }

    #[test]
    fn render_default_state_shows_queues_tab_chrome() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = DashboardState {
            data: DashboardData::default().with_loaded_at("12:00:00"),
            ..Default::default()
        };
        terminal.draw(|f| render(&state, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("symphony dashboard"), "{buf}");
        assert!(buf.contains("queues"), "{buf}");
        assert!(buf.contains("blockers"), "{buf}");
        assert!(buf.contains("integration"), "{buf}");
        assert!(buf.contains("runs"), "{buf}");
        assert!(buf.contains("loaded 12:00:00"), "{buf}");
        assert!(buf.contains("no items ready"), "{buf}");
    }

    #[test]
    fn render_blockers_tab_shows_open_edges() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = DashboardState {
            active: Tab::Blockers,
            ..Default::default()
        };
        state.data.blockers.push(BlockerRow {
            blocker_identifier: "ENG-1".into(),
            blocked_identifier: "ENG-2".into(),
            edge: WorkItemEdgeRecord {
                id: symphony_state::edges::EdgeId(7),
                parent_id: symphony_state::repository::WorkItemId(1),
                child_id: symphony_state::repository::WorkItemId(2),
                edge_type: EdgeType::Blocks,
                reason: Some("ci red".into()),
                status: "open".into(),
                source: EdgeSource::Unknown,
                tracker_edge_id: None,
                tracker_sync_status: None,
                tracker_sync_last_error: None,
                tracker_sync_attempts: 0,
                tracker_sync_last_attempt_at: None,
                created_at: "2026-01-01T00:00:00Z".into(),
            },
        });
        terminal.draw(|f| render(&state, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("ENG-1"), "{buf}");
        assert!(buf.contains("ENG-2"), "{buf}");
        assert!(buf.contains("ci red"), "{buf}");
        assert!(buf.contains("open:1"), "{buf}");
    }

    #[test]
    fn render_runs_tab_shows_run_rows() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = DashboardState {
            active: Tab::Runs,
            ..Default::default()
        };
        state.data.runs.push(RunRow {
            run: RunRecord {
                id: symphony_state::repository::RunId(11),
                work_item_id: symphony_state::repository::WorkItemId(1),
                role: "qa".into(),
                agent: "codex".into(),
                status: "running".into(),
                workspace_claim_id: None,
                lease_owner: None,
                lease_expires_at: None,
                started_at: None,
                ended_at: None,
                cost: None,
                tokens: None,
                result_summary: None,
                error: None,
                created_at: "2026-01-02T00:00:00Z".into(),
            },
            work_item_identifier: "ENG-9".into(),
        });
        terminal.draw(|f| render(&state, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("ENG-9"), "{buf}");
        assert!(buf.contains("qa"), "{buf}");
        assert!(buf.contains("running"), "{buf}");
        assert!(buf.contains("total:1"), "{buf}");
    }

    #[test]
    fn render_handles_resize_without_panicking() {
        let mut t1 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut t2 = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let state = DashboardState::default();
        t1.draw(|f| render(&state, f)).unwrap();
        t2.draw(|f| render(&state, f)).unwrap();
    }

    #[test]
    fn error_field_renders_inline_message() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = DashboardState {
            active: Tab::Qa,
            ..Default::default()
        };
        state.data.errors.qa = Some("disk full".into());
        terminal.draw(|f| render(&state, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("qa query failed"), "{buf}");
        assert!(buf.contains("disk full"), "{buf}");
    }
}
