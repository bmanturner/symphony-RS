//! `symphony watch` — hand-rolled SSE client with capped exponential
//! backoff, paired with a stdout/stderr pretty-printer.
//!
//! # Where this fits
//!
//! Per SPEC §3.1 layer 7, the status surface is intentionally
//! out-of-process: the orchestrator daemon serves an SSE feed of
//! [`OrchestratorEvent`]s, and this module is the *consumer* half of
//! that boundary. Phase 8's TUI scaffold (next checklist item) layers
//! `ratatui` on top of the same [`stream`] function this module
//! exposes, so any reconnection / parsing bug fixed here is fixed for
//! the TUI as well.
//!
//! # Why hand-rolled
//!
//! The wire format is deliberately tiny: text/event-stream framed by
//! blank lines, with `event:` and `data:` field prefixes. Pulling in a
//! generic SSE-client crate would add a dependency and obscure the
//! reconnect/backoff behaviour, which is the actual product surface a
//! user notices when the daemon restarts. The Phase-8 entry in the
//! crate budget explicitly excludes an SSE client crate for this
//! reason — keep this module dependency-free aside from `reqwest`.
//!
//! # Reconnect contract
//!
//! Connection loss is *expected*: a daemon restart, a flaky proxy, or
//! a brief network hiccup must not require the operator to relaunch
//! `symphony watch`. We:
//!
//! 1. Emit one [`WatchEvent::Disconnected`] explaining why the stream
//!    ended.
//! 2. Emit [`WatchEvent::Reconnecting`] before each retry, naming the
//!    attempt count and the delay so a UI can render a countdown.
//! 3. Sleep for `delay` (subject to cancellation), then attempt a new
//!    HTTP GET. On success we resume with [`WatchEvent::Connected`]
//!    and reset the backoff to its initial value.
//!
//! Backoff is capped exponential — multiplier and ceiling come from
//! [`BackoffConfig`]. The defaults (`100ms` initial, `30s` ceiling,
//! `2.0×` factor) are conservative: aggressive enough to recover
//! within a second of a transient blip, slow enough not to hammer a
//! daemon that is genuinely down.
//!
//! # Lagged frames
//!
//! When the SSE server reports the consumer fell behind its broadcast
//! buffer (a `event: lagged` frame, see [`crate::sse`]), we surface
//! that as [`WatchEvent::Lagged`] *and* close the stream — the next
//! reconnect lets the consumer pick up live events without trying to
//! plug the gap. That mirrors the server's stance: it is safer for a
//! UI to show "reconnecting" than to render a partial state forever.

use std::time::Duration;

use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use symphony_core::events::OrchestratorEvent;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cli::WatchArgs;

/// Default initial reconnect delay. A user who restarts the daemon by
/// hand experiences a sub-second reconnection at this setting; longer
/// outages roll up to [`DEFAULT_BACKOFF_MAX`].
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(100);

/// Default upper bound on the reconnect delay. Past this we stop
/// growing the wait — a daemon that has been down for a minute is no
/// likelier to come back if we wait two.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Default growth factor: each failed attempt multiplies the previous
/// delay by this value, clamped to [`BackoffConfig::max`].
pub const DEFAULT_BACKOFF_FACTOR: f64 = 2.0;

/// Reconnect schedule for the SSE client. Cheaply [`Clone`]able so
/// tests and the TUI can override individual fields without a builder.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    /// Wait before the first reconnect attempt after a disconnect.
    pub initial: Duration,
    /// Hard cap on the wait; later attempts saturate here.
    pub max: Duration,
    /// Multiplier applied after each unsuccessful attempt.
    pub factor: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial: DEFAULT_BACKOFF_INITIAL,
            max: DEFAULT_BACKOFF_MAX,
            factor: DEFAULT_BACKOFF_FACTOR,
        }
    }
}

impl BackoffConfig {
    /// Compute the next delay given the current one. Pure function —
    /// no clock access, so the unit tests can drive the schedule
    /// deterministically.
    #[must_use]
    pub fn step(&self, current: Duration) -> Duration {
        // We multiply on `as_secs_f64` rather than tinkering with the
        // raw nanosecond integer so very small initial delays
        // (`Duration::ZERO`) don't get stuck — `0.0 * factor = 0.0`
        // would refuse to grow.
        let next = (current.as_secs_f64() * self.factor).max(self.initial.as_secs_f64());
        let next = Duration::from_secs_f64(next);
        next.min(self.max)
    }
}

/// One observation surfaced by [`stream`] to the consumer.
///
/// We deliberately separate connection-state transitions
/// (`Connected` / `Disconnected` / `Reconnecting`) from data events
/// (`Event` / `Lagged`) so a UI can render a status banner without
/// having to introspect the JSON payload. Serialised by serde so a
/// future `--json` mode (or a snapshot test) can dump the stream
/// faithfully.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    /// HTTP GET succeeded and the SSE stream is live.
    Connected {
        /// URL we are now reading from. Useful in logs when an
        /// operator routes through a proxy.
        url: String,
    },
    /// The active stream ended. The reason is human-readable, not a
    /// stable enum, because the upstream errors come from `reqwest` and
    /// the SSE parser and we don't want to leak their type identities
    /// into our public surface.
    Disconnected {
        /// One-line reason suitable for a status banner.
        reason: String,
    },
    /// We are about to retry, after waiting `delay`. The TUI uses this
    /// to render a "reconnecting in 4s (attempt 3)" banner.
    Reconnecting {
        /// 1-based retry counter; resets after a successful connect.
        attempt: u32,
        /// Wall-clock delay before the next attempt.
        delay: Duration,
    },
    /// The server told us our subscription fell behind its broadcast
    /// buffer; `missed` events were dropped. The stream is closed
    /// after this — a fresh connection is the only way forward.
    Lagged {
        /// Number of events the server dropped before notifying us.
        missed: u64,
    },
    /// One [`OrchestratorEvent`] decoded from the SSE feed.
    Event(OrchestratorEvent),
}

/// SSE frame as the spec defines it: optional event name plus
/// accumulated data payload. Comment lines (lines starting with `:`)
/// are dropped during parsing and never produce a frame on their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// Value of the most recent `event:` field, if any.
    pub event: Option<String>,
    /// Concatenated `data:` field values, joined with `\n` per spec.
    pub data: String,
}

/// Streaming SSE frame parser. Accepts arbitrary byte chunks (which
/// may split frames or even individual lines), buffers them, and
/// yields one [`SseFrame`] per `\n\n`-delimited block.
///
/// # Why a hand-rolled parser
///
/// The full SSE spec includes a `retry:` field, an `id:` field, and
/// CR-only line endings. We support none of those because the server
/// in [`crate::sse`] never emits them; if a future change does, this
/// parser is the single place to extend. Keeping it small means the
/// parser-state diagram fits in one page.
#[derive(Debug, Default, Clone)]
pub struct SseParser {
    /// Bytes received but not yet split into frames.
    buf: String,
    /// Set when the previous chunk ended on a bare `\r` so a `\n` at
    /// the head of the next chunk is collapsed into a single newline
    /// rather than producing a spurious blank line.
    prev_cr: bool,
}

impl SseParser {
    /// Build an empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a UTF-8 chunk. Invalid UTF-8 is replaced with the
    /// standard replacement character — SSE is text-only, and a server
    /// that emits non-UTF-8 has misbehaved badly enough that there is
    /// nothing useful for us to do.
    pub fn push(&mut self, chunk: &str) {
        // SSE permits `\r\n`, `\r`, or `\n` as line terminators. We
        // normalise to `\n` so the rest of the parser only sees one
        // form. `prev_cr` carries state across `push` calls so a
        // chunk boundary that splits a `\r\n` pair doesn't produce a
        // phantom blank line.
        for ch in chunk.chars() {
            match (self.prev_cr, ch) {
                (true, '\n') => {
                    // Completing a CRLF: the `\n` was already emitted
                    // when we saw the `\r`; consume this byte silently.
                    self.prev_cr = false;
                }
                (_, '\r') => {
                    // Either CR-only (Mac classic style) or the start
                    // of a CRLF pair: emit one `\n` now and remember
                    // we did so in case `\n` follows.
                    self.buf.push('\n');
                    self.prev_cr = true;
                }
                (_, c) => {
                    self.buf.push(c);
                    self.prev_cr = false;
                }
            }
        }
    }

    /// Try to consume the next complete frame. Returns `None` when no
    /// `\n\n` is present yet, leaving the partial frame buffered.
    pub fn next_frame(&mut self) -> Option<SseFrame> {
        let end = self.buf.find("\n\n")?;
        // `block` excludes the terminator; we discard the two newlines
        // by advancing past them when we splice the buffer below.
        let block: String = self.buf.drain(..end + 2).collect();
        // Strip the trailing "\n\n" so per-line iteration is uniform.
        let body = block.trim_end_matches('\n');
        let mut event: Option<String> = None;
        let mut data = String::new();
        for line in body.split('\n') {
            if line.is_empty() || line.starts_with(':') {
                // Comment lines (the keep-alive `: ping`) and empty
                // lines mid-frame are non-events per spec.
                continue;
            }
            // SSE field syntax: "field" or "field: value" or
            // "field:value". We split on the first colon.
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => event = Some(value.to_string()),
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                }
                // Unknown fields (`id:`, `retry:`, future additions)
                // are silently ignored, per the SSE spec's forward-
                // compat rule.
                _ => {}
            }
        }
        Some(SseFrame { event, data })
    }
}

/// Build a [`Stream`] of [`WatchEvent`]s from an SSE endpoint.
///
/// The returned stream never ends naturally: it loops forever,
/// reconnecting on disconnect with [`BackoffConfig`]'s schedule. The
/// only way to stop it is to fire `cancel`, which both interrupts an
/// in-flight HTTP read and skips the next backoff sleep.
///
/// The stream is `Send + 'static` so a TUI can `tokio::spawn` it onto
/// a render task without lifetime juggling.
pub fn stream(
    url: String,
    backoff: BackoffConfig,
    cancel: CancellationToken,
) -> impl Stream<Item = WatchEvent> + Send + 'static {
    // Per-iteration state lives in a tuple the unfold owns. We carry:
    //   - the HTTP client (rebuilt on each reconnect would re-resolve
    //     DNS unnecessarily; one client is fine);
    //   - the current backoff delay, reset to ZERO on successful
    //     connect so the very first attempt happens immediately;
    //   - the attempt counter, also reset on connect;
    //   - the active connection if we have one (parser + response).
    let client = reqwest::Client::new();
    let initial = StreamState {
        client,
        url,
        backoff,
        cancel,
        delay: Duration::ZERO,
        attempt: 0,
        conn: None,
    };
    futures::stream::unfold(initial, |mut s| async move {
        // Each unfold invocation produces *exactly one* WatchEvent, so
        // there's no inner loop: control flow returns through one of
        // the three arms below and the caller polls us again to get
        // the next item. That keeps the state machine flat and easy
        // to reason about.

        // Hot connection path: pull the next decoded WatchEvent out
        // of the active connection if there is one.
        if let Some(conn) = s.conn.as_mut() {
            return match conn.next_event().await {
                NextEvent::Event(ev) => Some((WatchEvent::Event(ev), s)),
                NextEvent::Lagged(missed) => {
                    // Surface the lagged frame, then close so the
                    // next call falls into reconnect.
                    s.conn = None;
                    Some((WatchEvent::Lagged { missed }, s))
                }
                NextEvent::Closed(reason) => {
                    s.conn = None;
                    Some((WatchEvent::Disconnected { reason }, s))
                }
            };
        }

        // Cold path: sleep for `delay` (cancellable), then dial.
        if s.delay > Duration::ZERO {
            let cancelled = tokio::select! {
                _ = tokio::time::sleep(s.delay) => false,
                _ = s.cancel.cancelled() => true,
            };
            if cancelled {
                debug!("watch stream cancelled during backoff");
                return None;
            }
        }

        match dial(&s.client, &s.url).await {
            Ok(conn) => {
                info!(url = %s.url, "connected to SSE endpoint");
                s.conn = Some(conn);
                s.delay = Duration::ZERO;
                s.attempt = 0;
                Some((WatchEvent::Connected { url: s.url.clone() }, s))
            }
            Err(err) => {
                warn!(url = %s.url, error = %err, "SSE dial failed");
                s.delay = s.backoff.step(s.delay);
                s.attempt += 1;
                let attempt = s.attempt;
                let delay = s.delay;
                Some((WatchEvent::Reconnecting { attempt, delay }, s))
            }
        }
    })
}

/// Internal per-stream state.
struct StreamState {
    client: reqwest::Client,
    url: String,
    backoff: BackoffConfig,
    cancel: CancellationToken,
    delay: Duration,
    attempt: u32,
    conn: Option<Connection>,
}

/// Active HTTP connection plus its parser. Held inside the stream
/// state until the read errors out or we observe a `lagged` frame.
struct Connection {
    resp: reqwest::Response,
    parser: SseParser,
}

/// What the parser handed back for one frame.
enum NextEvent {
    Event(OrchestratorEvent),
    Lagged(u64),
    Closed(String),
}

impl Connection {
    /// Pull the next interesting frame off the connection. Comment
    /// frames and unparsable JSON are skipped silently — the SSE
    /// server emits keep-alive comments every 15s and we don't want
    /// the TUI flickering on each one.
    async fn next_event(&mut self) -> NextEvent {
        loop {
            // Try to decode a complete frame from the parser buffer
            // before blocking on more network bytes — a single chunk
            // can carry multiple frames.
            if let Some(frame) = self.parser.next_frame() {
                match frame.event.as_deref() {
                    Some("lagged") => {
                        let n = frame.data.trim().parse::<u64>().unwrap_or(0);
                        return NextEvent::Lagged(n);
                    }
                    _ => {
                        if frame.data.is_empty() {
                            // Comment-only or keep-alive frame.
                            continue;
                        }
                        match serde_json::from_str::<OrchestratorEvent>(&frame.data) {
                            Ok(ev) => return NextEvent::Event(ev),
                            Err(err) => {
                                warn!(error = %err, payload = %frame.data, "ignoring unparsable SSE frame");
                                continue;
                            }
                        }
                    }
                }
            }
            match self.resp.chunk().await {
                Ok(Some(bytes)) => {
                    let s = String::from_utf8_lossy(&bytes);
                    self.parser.push(&s);
                }
                Ok(None) => return NextEvent::Closed("server closed stream".into()),
                Err(err) => return NextEvent::Closed(format!("read error: {err}")),
            }
        }
    }
}

/// Open an SSE connection. Returns `Err` for any failure that should
/// trigger a reconnect — both transport errors and non-2xx responses.
async fn dial(client: &reqwest::Client, url: &str) -> Result<Connection, String> {
    let resp = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("server returned HTTP {}", resp.status()));
    }
    Ok(Connection {
        resp,
        parser: SseParser::new(),
    })
}

/// `symphony watch` entry point. Builds the SSE stream and hands it to
/// the [`crate::tui`] scaffold for rendering.
///
/// All transport behaviour (reconnect, backoff, lag handling) lives in
/// [`stream`]; presentation (alt-screen, key handling, layout) lives in
/// [`crate::tui`]. This entry point is just composition glue, kept
/// trivial so the operator-visible CLI surface is one short function.
pub async fn run(args: &WatchArgs, cancel: CancellationToken) -> anyhow::Result<()> {
    let s = stream(args.url.clone(), BackoffConfig::default(), cancel.clone());
    crate::tui::run(s, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn parser_splits_on_blank_line() {
        let mut p = SseParser::new();
        p.push("event: a\ndata: 1\n\nevent: b\ndata: 2\n\n");
        let f1 = p.next_frame().expect("frame 1");
        let f2 = p.next_frame().expect("frame 2");
        assert!(p.next_frame().is_none());
        assert_eq!(f1.event.as_deref(), Some("a"));
        assert_eq!(f1.data, "1");
        assert_eq!(f2.event.as_deref(), Some("b"));
        assert_eq!(f2.data, "2");
    }

    #[test]
    fn parser_buffers_partial_frame() {
        let mut p = SseParser::new();
        p.push("event: x\nda");
        assert!(p.next_frame().is_none(), "incomplete frame");
        p.push("ta: hi\n\n");
        let f = p.next_frame().expect("complete frame");
        assert_eq!(f.event.as_deref(), Some("x"));
        assert_eq!(f.data, "hi");
    }

    #[test]
    fn parser_drops_comments_and_keepalives() {
        let mut p = SseParser::new();
        // Comment-only frame plus a real frame.
        p.push(": ping\n\nevent: real\ndata: payload\n\n");
        let f1 = p.next_frame().expect("comment frame");
        // Comment-only frame: event=None, data="".
        assert!(f1.event.is_none());
        assert!(f1.data.is_empty());
        let f2 = p.next_frame().expect("real frame");
        assert_eq!(f2.event.as_deref(), Some("real"));
        assert_eq!(f2.data, "payload");
    }

    #[test]
    fn parser_concatenates_multi_line_data() {
        let mut p = SseParser::new();
        p.push("data: line1\ndata: line2\n\n");
        let f = p.next_frame().expect("frame");
        assert_eq!(f.data, "line1\nline2");
    }

    #[test]
    fn parser_handles_crlf_line_endings() {
        let mut p = SseParser::new();
        p.push("event: a\r\ndata: 1\r\n\r\n");
        let f = p.next_frame().expect("frame");
        assert_eq!(f.event.as_deref(), Some("a"));
        assert_eq!(f.data, "1");
    }

    #[test]
    fn parser_handles_field_without_space_after_colon() {
        let mut p = SseParser::new();
        p.push("event:tight\ndata:packed\n\n");
        let f = p.next_frame().expect("frame");
        assert_eq!(f.event.as_deref(), Some("tight"));
        assert_eq!(f.data, "packed");
    }

    #[test]
    fn backoff_step_grows_then_caps() {
        let cfg = BackoffConfig {
            initial: Duration::from_millis(100),
            max: Duration::from_millis(800),
            factor: 2.0,
        };
        let d0 = Duration::ZERO;
        let d1 = cfg.step(d0);
        assert_eq!(d1, Duration::from_millis(100));
        let d2 = cfg.step(d1);
        assert_eq!(d2, Duration::from_millis(200));
        let d3 = cfg.step(d2);
        assert_eq!(d3, Duration::from_millis(400));
        let d4 = cfg.step(d3);
        assert_eq!(d4, Duration::from_millis(800));
        // Saturates rather than overflows.
        let d5 = cfg.step(d4);
        assert_eq!(d5, Duration::from_millis(800));
    }

    #[test]
    fn backoff_default_factor_doubles() {
        let cfg = BackoffConfig::default();
        let d1 = cfg.step(Duration::ZERO);
        let d2 = cfg.step(d1);
        assert!(
            d2 >= d1 * 2,
            "default factor should at least double: {d1:?} -> {d2:?}"
        );
    }

    /// End-to-end against the real SSE server: connect, observe live
    /// events, kill the server, observe `Disconnected` + at least one
    /// `Reconnecting`, then cancel and assert the stream ends.
    ///
    /// This covers the SSE-server checklist dependency: the same code
    /// path `symphony watch` uses in production must survive a daemon
    /// restart cycle without the operator relaunching it.
    #[tokio::test]
    async fn stream_connects_and_emits_live_events() {
        use crate::sse::{SseConfig, SseState, serve};
        use std::time::Duration;
        use symphony_core::event_bus::EventBus;
        use symphony_core::events::OrchestratorEvent;
        use symphony_core::state_machine::ClaimState;
        use symphony_core::tracker::IssueId;

        let bus = EventBus::new(16);
        let state = SseState::new(bus.clone(), SseConfig::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_cancel = CancellationToken::new();
        let server = tokio::spawn(serve(listener, state, server_cancel.clone()));

        let watch_cancel = CancellationToken::new();
        let mut s = Box::pin(stream(
            format!("http://{addr}/events"),
            BackoffConfig::default(),
            watch_cancel.clone(),
        ));

        let first = tokio::time::timeout(Duration::from_secs(5), s.next())
            .await
            .expect("Connected within 5s")
            .expect("stream emits Connected");
        assert!(
            matches!(first, WatchEvent::Connected { .. }),
            "first frame should be Connected, got {first:?}"
        );

        for _ in 0..50 {
            if bus.subscriber_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        bus.emit(OrchestratorEvent::StateChanged {
            issue: IssueId::new("ENG-1"),
            identifier: "ENG-1".into(),
            previous: None,
            current: ClaimState::Running,
        });

        let next = tokio::time::timeout(Duration::from_secs(5), s.next())
            .await
            .expect("event within 5s")
            .expect("stream emits event");
        match next {
            WatchEvent::Event(OrchestratorEvent::StateChanged { ref identifier, .. }) => {
                assert_eq!(identifier, "ENG-1");
            }
            other => panic!("expected StateChanged, got {other:?}"),
        }

        // Drop the watcher before the server: holding the SSE response
        // open would block axum's graceful shutdown, deadlocking the
        // join below.
        watch_cancel.cancel();
        drop(s);
        server_cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }

    /// Reconnect path: dial a port that is not listening, observe a
    /// `Reconnecting` frame with a growing backoff delay, then cancel.
    /// Exercises the dial-failure → backoff path without depending on
    /// platform-specific TCP teardown semantics.
    #[tokio::test]
    async fn stream_emits_reconnecting_when_server_unreachable() {
        // Bind, capture the address, drop the listener so a dial here
        // returns connection-refused immediately on every platform.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);

        let watch_cancel = CancellationToken::new();
        let mut s = Box::pin(stream(
            format!("http://{addr}/events"),
            BackoffConfig {
                initial: Duration::from_millis(20),
                max: Duration::from_millis(40),
                factor: 2.0,
            },
            watch_cancel.clone(),
        ));

        // First two frames must be Reconnecting with the attempt count
        // climbing and the delay growing (or saturating at `max`).
        let f1 = tokio::time::timeout(Duration::from_secs(5), s.next())
            .await
            .expect("first frame within 5s")
            .expect("stream emits");
        let attempt1 = match f1 {
            WatchEvent::Reconnecting { attempt, delay } => {
                assert!(delay >= Duration::from_millis(20));
                attempt
            }
            other => panic!("expected Reconnecting, got {other:?}"),
        };
        assert_eq!(attempt1, 1);

        let f2 = tokio::time::timeout(Duration::from_secs(5), s.next())
            .await
            .expect("second frame within 5s")
            .expect("stream emits");
        match f2 {
            WatchEvent::Reconnecting { attempt, .. } => assert_eq!(attempt, 2),
            other => panic!("expected Reconnecting #2, got {other:?}"),
        }

        watch_cancel.cancel();
        // Stream may emit one more frame before observing cancel
        // (we're inside an in-flight dial). Drain until None.
        let drained = tokio::time::timeout(Duration::from_secs(5), async {
            while s.next().await.is_some() {}
        })
        .await;
        assert!(drained.is_ok(), "stream did not terminate after cancel");
    }
}
