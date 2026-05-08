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

use std::io;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::{Stream, StreamExt};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
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

/// All state the TUI needs to render a frame.
///
/// Future panels (active issues, cost summary, recent-events log) push
/// their own fields onto this struct. Keeping a single owning struct
/// (rather than a tangle of `Arc<Mutex<…>>` per panel) means the render
/// pipeline is `&AppState → Frame`, which is trivial to snapshot-test.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    /// Current SSE connection status, drives the header banner.
    pub connection: ConnectionStatus,
    /// One-line summary of the most recently observed event, shown in
    /// the placeholder body until panels land. Once the recent-events
    /// log panel exists this field becomes redundant and is removed in
    /// the same commit.
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

/// Render the TUI's placeholder layout against any backend.
///
/// Layout: a 3-line header showing connection status, a flexible body
/// containing the most-recent-event summary (a placeholder until the
/// panel iterations land), and a 1-line footer listing key bindings.
/// The signature takes `&mut Frame` so this same function works for
/// both the live `CrosstermBackend` and `TestBackend` in unit tests.
pub fn render(state: &AppState, frame: &mut Frame<'_>) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
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
        chunks[2 - 1],
    );

    let footer = Line::from(vec![
        Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ]);
    frame.render_widget(Paragraph::new(footer), chunks[2]);
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
    terminal.draw(|f| render(&state, f))?;

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
        terminal.draw(|f| render(&state, f))?;
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
    use symphony_core::events::OrchestratorEvent;
    use symphony_core::state_machine::ClaimState;
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
        terminal.draw(|f| render(&state, f)).unwrap();
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
        terminal.draw(|f| render(&state, f)).unwrap();
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
        t1.draw(|f| render(&state, f)).unwrap();
        t2.draw(|f| render(&state, f)).unwrap();
    }
}
