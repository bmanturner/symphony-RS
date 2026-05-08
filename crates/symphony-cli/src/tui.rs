//! TUI scaffold for `symphony watch`.
//!
//! # Why a separate module
//!
//! [`crate::watch`] already gives us a [`crate::watch::WatchEvent`]
//! [`futures::Stream`] that survives daemon restarts. The TUI's job is
//! purely *presentation*: turn that stream into frames the operator
//! reads, plus a key-press loop that knows how to quit. Keeping the
//! transport layer (SSE + reconnect) and the presentation layer
//! (ratatui) in different modules means each can evolve independently —
//! a panel layout change does not touch SSE parsing, and an SSE reconnect
//! tweak does not touch render code.
//!
//! # Iteration scope
//!
//! This iteration is the **scaffold** only: alternate screen, raw mode,
//! a placeholder layout (header / body / footer), terminal-resize
//! safety, and a `q` hotkey to quit. Subsequent checklist items layer
//! the active-issues table, cost summary, recent-events log, and
//! tandem-activity panel on top of the [`AppState`] defined here.
//! Putting the state struct in place now gives those iterations a stable
//! seam to plug into.
//!
//! # Testing
//!
//! Rendering is split from terminal I/O so unit tests can drive the
//! pure [`render`] function against ratatui's `TestBackend`. The
//! async [`run`] function (which owns `stdout` and the input task) is
//! only exercised end-to-end via the existing [`crate::watch`]
//! integration tests once the panels and snapshot tests land in later
//! iterations — running a real raw-mode terminal in `cargo test` is
//! flaky and not worth the maintenance.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use symphony_core::events::OrchestratorEvent;
use symphony_core::state_machine::ClaimState;
use symphony_core::tracker::IssueId;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::watch::WatchEvent;

/// Connection state surfaced in the header panel. Mirrors the lifecycle
/// arms of [`WatchEvent`] one-to-one so [`AppState::apply`] is a flat
/// `match`. We keep this enum public-but-not-exhaustive: future panels
/// will read it directly when they need to grey out content during a
/// reconnect, and a `Default` of [`ConnectionStatus::Pending`] gives
/// the very first frame (rendered before any SSE bytes arrive) a
/// sensible label.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// First frame, before the SSE client has reported anything.
    #[default]
    Pending,
    /// Stream is live and reading from `url`.
    Connected {
        /// Endpoint we are reading from (echoed back from
        /// [`WatchEvent::Connected`]).
        url: String,
    },
    /// Stream ended; the watch loop will retry shortly.
    Disconnected {
        /// Human-readable reason from [`WatchEvent::Disconnected`].
        reason: String,
    },
    /// Backoff sleep before the next dial.
    Reconnecting {
        /// 1-based attempt counter.
        attempt: u32,
        /// Wall-clock delay until the next dial.
        delay: Duration,
    },
}

/// One row of the active-issues table.
///
/// An "active" issue is one the orchestrator currently holds a claim
/// for. Rows are inserted on the first
/// [`OrchestratorEvent::StateChanged`] for an issue, mutated by
/// subsequent `StateChanged` and [`OrchestratorEvent::Dispatched`]
/// events, and removed on [`OrchestratorEvent::Released`]. We
/// deliberately do **not** drop rows on `Reconciled` because the
/// orchestrator follows up with one `Released` per dropped claim — and
/// listening to both would double-remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveIssue {
    /// Best-effort tracker identifier (`ENG-123`) snapshot from the
    /// most recent event. Carried so the table can render without
    /// joining against the tracker.
    pub identifier: String,
    /// Latest [`ClaimState`] observed for this issue.
    pub state: ClaimState,
    /// Wall-clock instant of the most recent
    /// [`OrchestratorEvent::Dispatched`] for this issue. Used to render
    /// the *elapsed* column. `None` until the first dispatch — between
    /// initial claim and first dispatch the row exists but elapsed is
    /// blank, which is correct: nothing has been "running" yet.
    pub dispatched_at: Option<Instant>,
    /// Agent backend reported by the most recent
    /// [`OrchestratorEvent::Dispatched`] (`"claude"`, `"codex"`,
    /// `"tandem"`, `"mock"`). `None` until the first dispatch.
    pub backend: Option<String>,
    /// 1-based attempt number from the most recent dispatch. `1` until
    /// proven otherwise; updated by `Dispatched`.
    pub attempt: u32,
}

/// All state the TUI needs to render a frame.
///
/// Future panels (cost summary, recent-events log, tandem activity)
/// push their own fields onto this struct. Keeping a single owning
/// struct (rather than a tangle of `Arc<Mutex<…>>` per panel) means
/// the render pipeline is `(&AppState, Instant) → Frame`, which is
/// trivial to snapshot-test.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// Current SSE connection status, drives the header banner.
    pub connection: ConnectionStatus,
    /// Issues the orchestrator currently has claimed, keyed by the
    /// tracker's stable [`IssueId`]. We use a [`HashMap`] for O(1)
    /// lookup on `apply` and sort by `identifier` at render time so
    /// the visual order is deterministic regardless of insertion
    /// order — important for ratatui snapshot tests.
    pub active_issues: HashMap<IssueId, ActiveIssue>,
    /// One-line summary of the most recently observed event, shown in
    /// the placeholder body until the recent-events panel lands. Once
    /// that panel exists this field becomes redundant and is removed
    /// in the same commit.
    pub last_event_summary: Option<String>,
    /// Set true when the user pressed `q` (or `Ctrl+C`); the run loop
    /// terminates on the next iteration. Tests inspect this directly.
    pub should_quit: bool,
}

impl AppState {
    /// Fold a [`WatchEvent`] into the state. Pure function — no I/O —
    /// so unit tests can build a state up event-by-event without a
    /// running tokio runtime.
    pub fn apply(&mut self, ev: &WatchEvent) {
        match ev {
            WatchEvent::Connected { url } => {
                self.connection = ConnectionStatus::Connected { url: url.clone() };
            }
            WatchEvent::Disconnected { reason } => {
                self.connection = ConnectionStatus::Disconnected {
                    reason: reason.clone(),
                };
            }
            WatchEvent::Reconnecting { attempt, delay } => {
                self.connection = ConnectionStatus::Reconnecting {
                    attempt: *attempt,
                    delay: *delay,
                };
            }
            WatchEvent::Lagged { missed } => {
                self.last_event_summary = Some(format!("lagged: missed {missed} events"));
            }
            WatchEvent::Event(orch) => {
                self.apply_orchestrator_event(orch);
                // Render the event's serde tag rather than its full
                // payload — the body panel is a one-liner today, and the
                // dedicated recent-events panel (next iteration) is
                // where the full payload belongs.
                self.last_event_summary = Some(format!(
                    "event: {}",
                    serde_json::to_value(orch)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                        .unwrap_or_else(|| "(unknown)".into())
                ));
            }
        }
    }

    /// Update [`AppState::active_issues`] from one orchestrator event.
    ///
    /// Folded out of [`AppState::apply`] so unit tests can drive table
    /// state without constructing a [`WatchEvent`] wrapper. Pure: no
    /// I/O, no clock dependency — `dispatched_at` is stamped with
    /// `Instant::now()` only when a `Dispatched` event arrives,
    /// matching the wall-clock semantics of the SSE feed.
    ///
    /// # Routing rules
    ///
    /// - `StateChanged` upserts the row's `identifier` and `state`.
    ///   For the [`ClaimState::RetryQueued`] branch we lift the
    ///   `attempt` onto the row so the table can show "retry(3)" even
    ///   before the next `Dispatched` lands.
    /// - `Dispatched` upserts the row (in case the table missed the
    ///   prior `StateChanged`, which can happen when the TUI starts
    ///   mid-flight) and stamps `dispatched_at`, `backend`, `attempt`.
    /// - `Released` removes the row. We trust `Released` rather than
    ///   the bulk `Reconciled` summary because the orchestrator emits
    ///   one `Released` per dropped claim (see [`symphony_core::events`]),
    ///   and reacting to both would double-remove.
    /// - All other variants are presentation-only here (the table
    ///   panel does not care about agent re-emissions or retry
    ///   schedules — the cost-summary and retry panels in later
    ///   iterations do).
    pub fn apply_orchestrator_event(&mut self, ev: &OrchestratorEvent) {
        match ev {
            OrchestratorEvent::StateChanged {
                issue,
                identifier,
                current,
                ..
            } => {
                let row = self
                    .active_issues
                    .entry(issue.clone())
                    .or_insert_with(|| ActiveIssue {
                        identifier: identifier.clone(),
                        state: *current,
                        dispatched_at: None,
                        backend: None,
                        attempt: 1,
                    });
                row.identifier = identifier.clone();
                row.state = *current;
                if let ClaimState::RetryQueued { attempt } = current {
                    row.attempt = *attempt;
                }
            }
            OrchestratorEvent::Dispatched {
                issue,
                identifier,
                backend,
                attempt,
                ..
            } => {
                let row = self
                    .active_issues
                    .entry(issue.clone())
                    .or_insert_with(|| ActiveIssue {
                        identifier: identifier.clone(),
                        state: ClaimState::Running,
                        dispatched_at: None,
                        backend: None,
                        attempt: *attempt,
                    });
                row.identifier = identifier.clone();
                row.state = ClaimState::Running;
                row.backend = Some(backend.clone());
                row.attempt = *attempt;
                row.dispatched_at = Some(Instant::now());
            }
            OrchestratorEvent::Released { issue, .. } => {
                self.active_issues.remove(issue);
            }
            OrchestratorEvent::Agent { .. }
            | OrchestratorEvent::RetryScheduled { .. }
            | OrchestratorEvent::Reconciled { .. } => {
                // Other variants do not change *which* issues are
                // active or *what state* they are in; later panels
                // hook in here without touching the active-issues
                // table.
            }
        }
    }

    /// Translate a key press into a state mutation. Returns `true` if
    /// the key was consumed. Encapsulating this here (rather than in
    /// the run loop's `match`) means tests can drive shortcut behaviour
    /// without a real terminal.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Crossterm fires `KeyEventKind::Release` on some platforms.
        // We only react to presses so a held `q` doesn't double-fire,
        // and so a key-up from a previous foreground process doesn't
        // immediately quit on launch.
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                self.should_quit = true;
                true
            }
            // Ctrl+C is an explicit quit signal: SIGINT does not reach
            // a process running in raw mode, so we have to translate
            // the keystroke ourselves.
            (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                true
            }
            _ => false,
        }
    }
}

/// Render the TUI layout against any backend.
///
/// Layout, top to bottom:
///
/// 1. **Header** (3 lines, bordered) — connection status banner.
/// 2. **Active issues table** (flex, bordered) — one row per
///    currently-claimed issue with identifier, state, elapsed,
///    backend.
/// 3. **Recent activity** (3 lines, bordered) — placeholder one-liner
///    until the recent-events log panel lands in the next iteration.
/// 4. **Footer** (1 line, no border) — key bindings.
///
/// Taking `now: Instant` as a parameter (rather than calling
/// [`Instant::now`] internally) lets unit tests pin elapsed-time
/// rendering deterministically.
pub fn render(state: &AppState, now: Instant, frame: &mut Frame<'_>) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let header_text = match &state.connection {
        ConnectionStatus::Pending => "connecting…".to_string(),
        ConnectionStatus::Connected { url } => format!("connected: {url}"),
        ConnectionStatus::Disconnected { reason } => format!("disconnected: {reason}"),
        ConnectionStatus::Reconnecting { attempt, delay } => {
            format!("reconnecting (attempt {attempt}, in {delay:?})")
        }
    };
    frame.render_widget(
        Paragraph::new(header_text).block(
            Block::default()
                .borders(Borders::ALL)
                .title("symphony watch"),
        ),
        chunks[0],
    );

    render_active_issues(state, now, frame, chunks[1]);

    let body = state
        .last_event_summary
        .clone()
        .unwrap_or_else(|| "(awaiting events…)".into());
    frame.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .title("recent activity (placeholder)"),
        ),
        chunks[2],
    );

    let footer = Line::from(vec![
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[3]);
}

/// Render the active-issues table into `area`.
///
/// Sorted by tracker `identifier` so the visual order is deterministic
/// independent of HashMap iteration order — required for ratatui
/// `TestBackend` snapshot tests in later iterations. When the table is
/// empty we render a placeholder line rather than collapsing the
/// border, so a quiet system still looks like the right kind of UI
/// (an empty table) instead of a missing panel.
fn render_active_issues(
    state: &AppState,
    now: Instant,
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("active issues ({})", state.active_issues.len()));

    if state.active_issues.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "(no active issues)",
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block),
            area,
        );
        return;
    }

    // Sort rows by identifier so two AppStates with identical content
    // render identical frames — the foundation for snapshot testing.
    let mut rows: Vec<(&IssueId, &ActiveIssue)> = state.active_issues.iter().collect();
    rows.sort_by(|a, b| a.1.identifier.cmp(&b.1.identifier));

    let header = Row::new(vec![
        Cell::from("identifier"),
        Cell::from("state"),
        Cell::from("elapsed"),
        Cell::from("backend"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD))
    .height(1);

    let body_rows: Vec<Row> = rows
        .into_iter()
        .map(|(_, ai)| {
            let state_label = format_state(&ai.state);
            let state_cell = Cell::from(state_label).style(state_colour(&ai.state));
            let elapsed = ai
                .dispatched_at
                .map(|d| format_elapsed(now.saturating_duration_since(d)))
                .unwrap_or_else(|| "—".to_string());
            let backend = ai.backend.clone().unwrap_or_else(|| "—".to_string());
            Row::new(vec![
                Cell::from(ai.identifier.clone()),
                state_cell,
                Cell::from(elapsed),
                Cell::from(backend),
            ])
        })
        .collect();

    // Column widths chosen to match the longest realistic value:
    // identifiers are typically `ABC-1234` (≤ 12 chars), states fit
    // in 12 chars including `retry(99)`, elapsed renders as `99h59m`
    // at most (we cap formatting), and backend is one of four short
    // tokens. The remainder flexes via `Min`.
    let widths = [
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Min(8),
    ];
    let table = Table::new(body_rows, widths).header(header).block(block);
    frame.render_widget(table, area);
}

/// Stringify a [`ClaimState`] for the state column.
///
/// Kept separate from `ClaimState`'s serde tag so the *display* form
/// can diverge from the wire form without breaking the SSE contract:
/// today they happen to agree (`"running"`, `"retry(N)"`), but a
/// future variant might serialise differently from how it should
/// render in a 12-column cell.
fn format_state(state: &ClaimState) -> String {
    match state {
        ClaimState::Running => "running".to_string(),
        ClaimState::RetryQueued { attempt } => format!("retry({attempt})"),
    }
}

/// Foreground colour for a state cell.
///
/// Running is green (steady-state happy path), retries are yellow
/// (something went wrong but the orchestrator is handling it). Future
/// states (e.g. a `Stuck` variant) get a colour at the same time they
/// land so the table never silently downgrades to default white.
fn state_colour(state: &ClaimState) -> Style {
    match state {
        ClaimState::Running => Style::default().fg(Color::Green),
        ClaimState::RetryQueued { .. } => Style::default().fg(Color::Yellow),
    }
}

/// Format a [`Duration`] into a tight 1–6-char label.
///
/// We render `Ns`, `NmMs`, `NhMm` rather than `H:MM:SS` because the
/// elapsed column is 8 characters wide and many sessions run into
/// hours. Above 99h we clamp to `99h+` so the column never overflows.
fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    if total < 60 {
        return format!("{total}s");
    }
    if total < 3_600 {
        let m = total / 60;
        let s = total % 60;
        return format!("{m}m{s:02}s");
    }
    let h = total / 3_600;
    if h >= 100 {
        return "99h+".to_string();
    }
    let m = (total % 3_600) / 60;
    format!("{h}h{m:02}m")
}

/// Drive the TUI: own stdout in alternate-screen + raw mode, render on
/// every state change, quit on `q` / `Esc` / `Ctrl+C` / external
/// cancel. Returns when the user quits or `cancel` fires.
///
/// The function is structured so that any `?` after [`ratatui::init()`]
/// must run through the `restore_terminal` helper — leaving the user's
/// terminal in raw mode is the worst possible failure mode for a TUI.
/// We use a guard struct rather than `try {}` because `ratatui::init`'s
/// `DefaultTerminal` already does most of the work, but we still wrap
/// the run body in a closure so any error path drops the terminal
/// before propagating.
pub async fn run<S>(stream: S, cancel: CancellationToken) -> anyhow::Result<()>
where
    S: Stream<Item = WatchEvent> + Send + 'static,
{
    let mut terminal = ratatui::init();
    let outcome = run_inner(&mut terminal, stream, cancel).await;
    // `ratatui::restore` swallows any error printing the terminal back
    // to a sane state — a TUI that exits is not a place to surface
    // I/O errors, and the stderr would land in a now-restored cooked
    // shell anyway.
    ratatui::restore();
    outcome
}

/// The TUI body, separated from `run` so the alt-screen restore
/// always runs. Tests do not exercise this directly (we'd need a real
/// pty); state-update paths are covered via [`AppState::apply`] and
/// [`AppState::handle_key`].
async fn run_inner<S>(
    terminal: &mut ratatui::DefaultTerminal,
    stream: S,
    cancel: CancellationToken,
) -> anyhow::Result<()>
where
    S: Stream<Item = WatchEvent> + Send + 'static,
{
    let mut state = AppState::default();
    let mut stream = Box::pin(stream);
    let mut keys = spawn_key_reader(cancel.clone());

    // Initial frame so the operator sees *something* before the first
    // SSE byte arrives — the SSE dial may take a few hundred ms over
    // a slow network and a blank terminal looks broken.
    terminal.draw(|f| render(&state, Instant::now(), f))?;

    loop {
        tokio::select! {
            // Bias toward input so a `q` press during a flood of events
            // still feels responsive: tokio::select! is random by
            // default, but a key press should never wait behind an
            // unbounded SSE stream.
            biased;

            _ = cancel.cancelled() => {
                debug!("tui: external cancel");
                break;
            }
            maybe_input = keys.recv() => {
                match maybe_input {
                    Some(Event::Key(k)) => {
                        state.handle_key(k);
                    }
                    Some(Event::Resize(_, _)) => {
                        // Ratatui auto-detects the new size on the next
                        // `draw`; we just need to make sure a draw
                        // happens. Falling through to the redraw at the
                        // bottom of the loop accomplishes that.
                    }
                    Some(_) => {
                        // Mouse / paste / focus events are not bound in
                        // the scaffold. Future iterations may add panel
                        // hover; for now ignore so they don't show up
                        // as random redraws.
                    }
                    None => {
                        // Reader task exited (cancelled or terminal
                        // closed). Treat as a quit so we don't spin.
                        debug!("tui: input reader closed");
                        state.should_quit = true;
                    }
                }
            }
            maybe_event = stream.next() => {
                match maybe_event {
                    Some(ev) => state.apply(&ev),
                    None => {
                        // The watch stream is supposed to be
                        // infinite, so seeing `None` here means cancel
                        // already fired in the SSE side. Quit cleanly.
                        debug!("tui: watch stream ended");
                        state.should_quit = true;
                    }
                }
            }
        }

        if state.should_quit {
            break;
        }
        terminal.draw(|f| render(&state, Instant::now(), f))?;
    }
    Ok(())
}

/// Spawn a blocking task that pumps crossterm input events onto an mpsc
/// channel. Crossterm's `event::read` is synchronous and blocks the
/// whole thread; running it on the tokio runtime would starve every
/// other task.
fn spawn_key_reader(cancel: CancellationToken) -> tokio::sync::mpsc::Receiver<Event> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::task::spawn_blocking(move || {
        // 100ms polling is a compromise: low enough that `q` feels
        // instant, high enough that an idle TUI uses near-zero CPU.
        let tick = Duration::from_millis(100);
        loop {
            if cancel.is_cancelled() {
                break;
            }
            match crossterm::event::poll(tick) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "tui: crossterm read failed");
                        break;
                    }
                },
                Ok(false) => continue,
                Err(err) => {
                    warn!(error = %err, "tui: crossterm poll failed");
                    break;
                }
            }
        }
        // Best-effort drop: the channel may already be closed if the
        // run loop exited first.
        drop(tx);
        io::Result::<()>::Ok(())
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use symphony_core::agent::{SessionId, ThreadId, TurnId};
    use symphony_core::events::OrchestratorEvent;
    use symphony_core::state_machine::{ClaimState, ReleaseReason};
    use symphony_core::tracker::IssueId;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn apply_connected_sets_status() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Connected {
            url: "http://x/events".into(),
        });
        assert_eq!(
            s.connection,
            ConnectionStatus::Connected {
                url: "http://x/events".into()
            }
        );
    }

    #[test]
    fn apply_reconnecting_carries_attempt_and_delay() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Reconnecting {
            attempt: 4,
            delay: Duration::from_millis(250),
        });
        assert_eq!(
            s.connection,
            ConnectionStatus::Reconnecting {
                attempt: 4,
                delay: Duration::from_millis(250)
            }
        );
    }

    #[test]
    fn apply_event_summarises_by_serde_tag() {
        let mut s = AppState::default();
        s.apply(&WatchEvent::Event(OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        }));
        let summary = s.last_event_summary.expect("summary set");
        assert!(
            summary.contains("state_changed") || summary.contains("StateChanged"),
            "unexpected summary: {summary}"
        );
    }

    #[test]
    fn handle_key_q_quits() {
        let mut s = AppState::default();
        assert!(s.handle_key(key(KeyCode::Char('q'))));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_esc_quits() {
        let mut s = AppState::default();
        assert!(s.handle_key(key(KeyCode::Esc)));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_ctrl_c_quits() {
        let mut s = AppState::default();
        let k = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(s.handle_key(k));
        assert!(s.should_quit);
    }

    #[test]
    fn handle_key_ignores_release_events() {
        // A release of `q` from a previous foreground app must not quit
        // the TUI on launch.
        let mut s = AppState::default();
        let release = KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert!(!s.handle_key(release));
        assert!(!s.should_quit);
    }

    #[test]
    fn handle_key_ignores_unbound_keys() {
        let mut s = AppState::default();
        assert!(!s.handle_key(key(KeyCode::Char('x'))));
        assert!(!s.should_quit);
    }

    #[test]
    fn render_pending_state_shows_connecting_label() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let state = AppState::default();
        terminal
            .draw(|f| render(&state, Instant::now(), f))
            .unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("connecting"), "buffer was: {buf}");
        assert!(buf.contains("symphony watch"), "buffer was: {buf}");
        assert!(buf.contains("q quit"), "footer missing in: {buf}");
    }

    #[test]
    fn render_connected_state_shows_url() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = AppState::default();
        state.apply(&WatchEvent::Connected {
            url: "http://h/e".into(),
        });
        terminal
            .draw(|f| render(&state, Instant::now(), f))
            .unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("http://h/e"), "buffer: {buf}");
    }

    #[test]
    fn render_handles_resize() {
        // Successive draws against differently-sized backends model a
        // terminal resize. Both must succeed without panicking — that
        // is the entirety of the resize-safety contract for the
        // scaffold (no fixed-size assumptions in the layout).
        let mut t1 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let mut t2 = Terminal::new(TestBackend::new(20, 5)).unwrap();
        let state = AppState::default();
        t1.draw(|f| render(&state, Instant::now(), f)).unwrap();
        t2.draw(|f| render(&state, Instant::now(), f)).unwrap();
    }

    fn session(t: &str, u: &str) -> SessionId {
        SessionId::new(&ThreadId::new(t), &TurnId::new(u))
    }

    #[test]
    fn state_changed_inserts_active_issue_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });
        let row = s.active_issues.get(&IssueId::new("ENG-1")).unwrap();
        assert_eq!(row.identifier, "ENG-1");
        assert_eq!(row.state, ClaimState::Running);
        assert!(row.dispatched_at.is_none());
        assert!(row.backend.is_none());
    }

    #[test]
    fn dispatched_stamps_backend_and_attempt() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-2"),
            identifier: "ENG-2".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-2"),
            identifier: "ENG-2".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 2,
        });
        let row = s.active_issues.get(&IssueId::new("ENG-2")).unwrap();
        assert_eq!(row.backend.as_deref(), Some("claude"));
        assert_eq!(row.attempt, 2);
        assert!(row.dispatched_at.is_some());
        assert_eq!(row.state, ClaimState::Running);
    }

    #[test]
    fn dispatched_without_prior_state_change_still_inserts_row() {
        // The TUI may attach mid-flight, after the orchestrator has
        // already moved past the initial StateChanged. The table still
        // needs a row so the operator sees the work-in-progress.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-7"),
            identifier: "ENG-7".into(),
            session: session("t", "u"),
            backend: "tandem".into(),
            attempt: 1,
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-7")));
    }

    #[test]
    fn retry_queued_state_change_lifts_attempt() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-3"),
            identifier: "ENG-3".into(),
            previous: Some(ClaimState::Running),
            current: ClaimState::RetryQueued { attempt: 4 },
        });
        let row = s.active_issues.get(&IssueId::new("ENG-3")).unwrap();
        assert_eq!(row.attempt, 4);
        assert_eq!(row.state, ClaimState::RetryQueued { attempt: 4 });
    }

    #[test]
    fn released_removes_active_issue_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-4"),
            identifier: "ENG-4".into(),
            previous: None,
            current: ClaimState::Running,
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-4")));
        s.apply_orchestrator_event(&OrchestratorEvent::Released {
            issue: IssueId::new("ENG-4"),
            identifier: "ENG-4".into(),
            reason: ReleaseReason::Completed,
            final_state: None,
        });
        assert!(!s.active_issues.contains_key(&IssueId::new("ENG-4")));
    }

    #[test]
    fn reconciled_does_not_remove_rows() {
        // Released does the per-issue cleanup; if Reconciled also did,
        // the orchestrator's emission of both would double-remove.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-5"),
            identifier: "ENG-5".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Reconciled {
            dropped: vec![IssueId::new("ENG-5")],
        });
        assert!(s.active_issues.contains_key(&IssueId::new("ENG-5")));
    }

    #[test]
    fn format_elapsed_compact_branches() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed(Duration::from_secs(60)), "1m00s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m05s");
        assert_eq!(format_elapsed(Duration::from_secs(3_600)), "1h00m");
        assert_eq!(format_elapsed(Duration::from_secs(3_660)), "1h01m");
        // Cap so an 8-char elapsed column never overflows.
        assert_eq!(format_elapsed(Duration::from_secs(100 * 3_600)), "99h+");
    }

    #[test]
    fn format_state_renders_running_and_retry() {
        assert_eq!(format_state(&ClaimState::Running), "running");
        assert_eq!(
            format_state(&ClaimState::RetryQueued { attempt: 7 }),
            "retry(7)"
        );
    }

    #[test]
    fn render_active_issues_panel_shows_rows_sorted_by_identifier() {
        // Insert ENG-2 first so HashMap iteration order is unlikely to
        // match identifier order without our explicit sort.
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-2"),
            identifier: "ENG-2".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::StateChanged {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("id-1"),
            identifier: "ENG-1".into(),
            session: session("t", "u"),
            backend: "claude".into(),
            attempt: 1,
        });

        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).unwrap();
        let now = Instant::now();
        terminal.draw(|f| render(&s, now, f)).unwrap();
        let buf = terminal.backend().to_string();

        assert!(buf.contains("active issues (2)"), "buf: {buf}");
        assert!(buf.contains("identifier"), "buf: {buf}");
        assert!(buf.contains("ENG-1"), "buf: {buf}");
        assert!(buf.contains("ENG-2"), "buf: {buf}");
        assert!(buf.contains("running"), "buf: {buf}");
        assert!(buf.contains("claude"), "buf: {buf}");
        // Sorted: ENG-1 must appear earlier in the buffer than ENG-2.
        let p1 = buf.find("ENG-1").unwrap();
        let p2 = buf.find("ENG-2").unwrap();
        assert!(p1 < p2, "rows not sorted by identifier: {buf}");
    }

    #[test]
    fn render_active_issues_panel_shows_placeholder_when_empty() {
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let s = AppState::default();
        terminal.draw(|f| render(&s, Instant::now(), f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("active issues (0)"), "buf: {buf}");
        assert!(buf.contains("no active issues"), "buf: {buf}");
    }

    #[test]
    fn render_active_issues_shows_elapsed_for_dispatched_row() {
        let mut s = AppState::default();
        s.apply_orchestrator_event(&OrchestratorEvent::Dispatched {
            issue: IssueId::new("ENG-9"),
            identifier: "ENG-9".into(),
            session: session("t", "u"),
            backend: "codex".into(),
            attempt: 1,
        });
        // Simulate a 90-second-old dispatch by rewinding the row's
        // timestamp before we render against a fixed `now`.
        let dispatched = Instant::now();
        s.active_issues
            .get_mut(&IssueId::new("ENG-9"))
            .unwrap()
            .dispatched_at = Some(dispatched);
        let now = dispatched + Duration::from_secs(90);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&s, now, f)).unwrap();
        let buf = terminal.backend().to_string();
        assert!(buf.contains("1m30s"), "elapsed missing in buf: {buf}");
    }
}
