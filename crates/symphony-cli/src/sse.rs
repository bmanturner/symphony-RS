//! HTTP server-sent events (SSE) bridge between the orchestrator's
//! [`EventBus`] and external observers (Phase 8 `symphony watch` TUI,
//! ad-hoc `curl` consumers, future dashboards).
//!
//! # Where this fits
//!
//! Per SPEC §3.1 layer 7, the status surface is *out-of-process*: the
//! orchestrator daemon stays headless and systemd-friendly, exposing a
//! single read-only HTTP endpoint that streams [`OrchestratorEvent`]s as
//! `text/event-stream`. The TUI is a separate process that subscribes
//! over HTTP, can crash and reconnect freely, and never touches the
//! daemon's address space.
//!
//! This module owns just the *server* half of that boundary.
//! [`serve`] is the entry point [`crate::run::run`] calls to spawn the
//! HTTP listener alongside the orchestrator; it shares its
//! [`CancellationToken`] with the poll loop so SIGINT drains both
//! halves on the same signal.
//!
//! # Wire format
//!
//! Each [`OrchestratorEvent`] becomes one SSE frame:
//!
//! ```text
//! event: state_changed
//! data: {"type":"state_changed","issue":"ENG-1",...}
//!
//! ```
//!
//! - The `event:` field carries [`OrchestratorEvent::kind`] so a switch
//!   on event type does not require parsing the JSON body.
//! - The `data:` field carries the full event JSON, identical to what
//!   [`EventBus::subscribe`] yields. The wire-format stability contract
//!   in [`symphony_core::events`] applies.
//! - Slow consumers receive one synthetic `event: lagged` frame and the
//!   stream is then closed. The TUI surfaces this as a "disconnected"
//!   notice and reconnects with backoff. Surfacing the gap explicitly
//!   is preferable to silently dropping events: the consumer learns it
//!   missed something rather than rendering a stale view forever.
//!
//! # Subscriber bound
//!
//! [`SseConfig::max_subscribers`] caps the number of concurrent
//! `GET /events` connections. Past that, the handler returns
//! `503 Service Unavailable` with a tiny diagnostic body. The cap exists
//! to bound memory: each subscriber holds one broadcast-channel slot and
//! one keep-alive timer. An attacker (or a buggy script) that opens
//! thousands of connections must not be able to balloon the daemon's
//! footprint.
//!
//! The cap is enforced with an `Arc<AtomicUsize>` rather than a
//! semaphore because we want the over-cap path to *fail fast* with a
//! 503 — semaphore acquire would block the request indefinitely, which
//! is exactly the wrong UX for a status endpoint.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use symphony_core::event_bus::EventBus;
use symphony_core::events::OrchestratorEvent;
use symphony_state::events::{EventRecord, EventRepository, EventSequence};
use symphony_state::{StateDb, StateError};
use tokio::net::TcpListener;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Default upper bound on concurrent `GET /events` subscribers.
///
/// Sized so a realistic operator (one TUI, a couple of `curl`s, a CI
/// scraper) lands well under cap, while a runaway client can't open
/// thousands of long-lived connections. Surface as a typed config knob
/// if a deployment ever needs to retune.
pub const DEFAULT_MAX_SUBSCRIBERS: usize = 32;

/// Default keep-alive cadence. SSE peers (browsers, intermediate
/// proxies) close idle connections after ~30 s; emitting a comment
/// frame every 15 s keeps the socket warm without spamming the wire.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Default cadence at which the durable-tail handler polls the
/// `events` table for new rows. Sized so a stalled write side doesn't
/// burn CPU and a bursty write side still feels live (200 ms is well
/// under the human-perceptible threshold for a status surface).
pub const DEFAULT_DURABLE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Default page size for each durable-tail poll. Big enough that the
/// handler catches up to backlogs without thousands of round-trips,
/// small enough that one slow consumer doesn't pin a giant payload in
/// memory.
pub const DEFAULT_DURABLE_PAGE_SIZE: usize = 256;

/// Tunables for the SSE handler. Cheaply [`Clone`]able — the router
/// stores one copy and hands clones into request handlers.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// Hard cap on concurrent subscribers. Past this, new connections
    /// receive `503 Service Unavailable`.
    pub max_subscribers: usize,
    /// Cadence the durable-tail handler polls the `events` table at.
    pub durable_poll_interval: Duration,
    /// Maximum rows the durable-tail handler reads per poll.
    pub durable_page_size: usize,
}

impl Default for SseConfig {
    fn default() -> Self {
        Self {
            max_subscribers: DEFAULT_MAX_SUBSCRIBERS,
            durable_poll_interval: DEFAULT_DURABLE_POLL_INTERVAL,
            durable_page_size: DEFAULT_DURABLE_PAGE_SIZE,
        }
    }
}

/// Read-only access to the durable event tail used by the SSE handler.
///
/// The kernel persists every event into the `events` table before
/// broadcasting (ARCHITECTURE_v2 §5.7). The SSE surface therefore reads
/// from the durable log rather than the in-process broadcast channel —
/// a reconnecting consumer can resume from any sequence and trust the
/// stream is gap-free.
///
/// The trait is intentionally narrow: one paged read keyed by sequence
/// and one "where is the head right now" lookup. That is the entire
/// contract a polling SSE handler needs, and it lets tests substitute a
/// fake without depending on SQLite.
pub trait DurableTailSource: Send + Sync + 'static {
    /// Events strictly after `after`, in ascending sequence order, up
    /// to `limit`. Pass `None` to start from the beginning.
    fn events_after(
        &self,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<DurableEvent>, StateError>;

    /// The highest persisted sequence, or `None` when the log is empty.
    fn last_sequence(&self) -> Result<Option<i64>, StateError>;
}

/// Wire-shape representation of one durable event, decoupled from the
/// `symphony-state` row type so the SSE module can serialise without
/// pulling SQLite types into the response surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableEvent {
    /// Monotonically increasing sequence assigned at append time.
    pub sequence: i64,
    /// Event type discriminator, written to the SSE `event:` field.
    pub event_type: String,
    /// Already-encoded JSON payload, written to the SSE `data:` field.
    pub payload: String,
}

impl From<EventRecord> for DurableEvent {
    fn from(record: EventRecord) -> Self {
        Self {
            sequence: record.sequence.0,
            event_type: record.event_type,
            payload: record.payload,
        }
    }
}

/// `DurableTailSource` adapter over a shared [`StateDb`] handle.
///
/// `StateDb` wraps a single `rusqlite::Connection`, which is `!Sync`;
/// we serialise reads through a `Mutex` so the SSE handler (which may
/// run multiple concurrent subscribers) can share one connection
/// without each spinning up its own SQLite handle.
pub struct StateDbTailSource {
    db: Arc<Mutex<StateDb>>,
}

impl StateDbTailSource {
    /// Wrap an existing `StateDb`. The caller is responsible for having
    /// already run migrations.
    #[must_use]
    pub fn new(db: Arc<Mutex<StateDb>>) -> Self {
        Self { db }
    }
}

impl DurableTailSource for StateDbTailSource {
    fn events_after(
        &self,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<DurableEvent>, StateError> {
        let db = self.db.lock().expect("state-db mutex poisoned");
        let rows = db.list_events_after(after.map(EventSequence), limit)?;
        Ok(rows.into_iter().map(DurableEvent::from).collect())
    }

    fn last_sequence(&self) -> Result<Option<i64>, StateError> {
        let db = self.db.lock().expect("state-db mutex poisoned");
        Ok(db.last_event_sequence()?.map(|s| s.0))
    }
}

/// Application state passed to the `GET /events` handler.
///
/// Holds the typed [`SseConfig`] knobs, an atomic counter tracking
/// active subscribers, and one of two event sources:
///
/// - The orchestrator's [`EventBus`] (for legacy/tests).
/// - A [`DurableTailSource`] backed by the durable `events` table —
///   the production path per SPEC v2 §5.14, which lets a reconnecting
///   consumer resume from any sequence without losing rows.
///
/// The counter is shared via `Arc` so the [`SubscriberGuard`] can
/// decrement it on drop without needing access to the `Router` state.
#[derive(Clone)]
pub struct SseState {
    bus: EventBus,
    durable: Option<Arc<dyn DurableTailSource>>,
    config: SseConfig,
    active: Arc<AtomicUsize>,
}

impl std::fmt::Debug for SseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseState")
            .field("bus", &self.bus)
            .field("durable", &self.durable.as_ref().map(|_| "<durable>"))
            .field("config", &self.config)
            .field("active", &self.active)
            .finish()
    }
}

impl SseState {
    /// Build state from an existing [`EventBus`] (typically the same
    /// instance the orchestrator emits onto) and the configured caps.
    /// The handler streams from the in-process broadcast channel — no
    /// durable replay or resume.
    #[must_use]
    pub fn new(bus: EventBus, config: SseConfig) -> Self {
        Self {
            bus,
            durable: None,
            config,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Build state that streams from a [`DurableTailSource`] (the
    /// production path) instead of the in-process broadcast channel.
    /// `bus` is still kept on the struct so legacy callers wiring
    /// `EventBus`-based tests through this constructor remain
    /// compatible; the handler ignores it whenever `durable` is set.
    #[must_use]
    pub fn with_durable_tail(
        bus: EventBus,
        durable: Arc<dyn DurableTailSource>,
        config: SseConfig,
    ) -> Self {
        Self {
            bus,
            durable: Some(durable),
            config,
            active: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Currently-active subscriber count. Useful for tests and metrics
    /// surfaces; not exposed over HTTP.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Try to claim a subscriber slot. Returns `None` when the cap is
    /// already reached. The [`SubscriberGuard`] released by this call
    /// owns the slot and decrements the counter on drop, including
    /// when the SSE stream future is cancelled by a client disconnect.
    fn try_acquire(&self) -> Option<SubscriberGuard> {
        // We use a CAS-style increment-then-check: an over-cap caller
        // increments and immediately decrements. This is racy-by-design
        // — under contention we may briefly exceed `max_subscribers` by
        // one or two. That is acceptable for a soft cap whose purpose
        // is to bound steady-state memory, not enforce a hard ceiling.
        let prev = self.active.fetch_add(1, Ordering::SeqCst);
        if prev >= self.config.max_subscribers {
            self.active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(SubscriberGuard {
            counter: Arc::clone(&self.active),
        })
    }
}

/// RAII handle that decrements the active-subscriber counter when
/// dropped. Held inside the SSE response stream, so dropping the
/// response (client disconnect, axum shutdown) releases the slot
/// automatically without a separate registration step.
#[derive(Debug)]
struct SubscriberGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Build an [`axum::Router`] exposing `GET /events`.
///
/// The router is intentionally narrow: one route, no health probe, no
/// metrics endpoint. Mount it under whatever bind address is configured
/// in `WORKFLOW.md`'s `status.bind` field. A future checklist item may
/// add observability endpoints alongside; this module is the natural
/// home for them.
pub fn router(state: SseState) -> Router {
    Router::new()
        .route("/events", get(events_handler))
        .with_state(state)
}

/// Serve the SSE router on `listener` until `cancel` fires.
///
/// Pulled out of [`crate::run::run`] so the integration test can bind a
/// random port (`127.0.0.1:0`), discover the assigned address via
/// [`TcpListener::local_addr`], and drive a real HTTP client against
/// the same code path the daemon uses.
///
/// Cancellation is cooperative: we hand `cancel.cancelled()` to axum's
/// `with_graceful_shutdown`, which stops accepting new connections and
/// waits for in-flight requests to drain. SSE streams currently in
/// flight unblock through their own broadcast subscription — when the
/// orchestrator stops emitting, the keep-alive timer is the only thing
/// keeping them alive, and dropping the bus side closes the channels.
pub async fn serve(
    listener: TcpListener,
    state: SseState,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let local = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    info!(bind = %local, "SSE status server listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
}

/// Bind `bind` and call [`serve`]. Convenience wrapper for the
/// composition root, which has a string from `WORKFLOW.md` rather than a
/// pre-built listener.
pub async fn bind_and_serve(
    bind: &str,
    state: SseState,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    serve(listener, state, cancel).await
}

/// Query parameters accepted by `GET /events`.
#[derive(Debug, Deserialize, Default)]
pub struct EventsQuery {
    /// Resume after this sequence. Equivalent to `Last-Event-ID`. The
    /// query parameter is honoured first because some intermediate
    /// proxies strip arbitrary headers; passing `?after=N` works
    /// everywhere.
    pub after: Option<i64>,
}

/// `GET /events` handler. Fast-fails with 503 when over cap; otherwise
/// returns a long-lived SSE response that stays open until the client
/// disconnects.
///
/// When the SSE state was built with [`SseState::with_durable_tail`]
/// the response streams from the durable `events` table (resume cursor
/// honoured via `Last-Event-ID` or `?after=`). Otherwise it falls back
/// to the in-process broadcast channel, which the handler keeps for
/// tests and for the headless-CI codepath.
async fn events_handler(
    State(state): State<SseState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Response {
    let Some(guard) = state.try_acquire() else {
        warn!(
            cap = state.config.max_subscribers,
            "rejecting SSE subscriber: at concurrent-subscriber cap",
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "symphony: at concurrent-subscriber cap",
        )
            .into_response();
    };
    debug!(active = state.active(), "accepted SSE subscriber");

    if let Some(durable) = state.durable.clone() {
        // Per SSE convention, `Last-Event-ID` carries the cursor on
        // reconnect. We accept the query parameter as a fallback for
        // proxies that drop the header.
        let cursor = query
            .after
            .or_else(|| parse_last_event_id(&headers))
            .or_else(|| match durable.last_sequence() {
                Ok(seq) => seq,
                Err(err) => {
                    warn!(error = %err, "durable last-sequence lookup failed; starting from 0");
                    None
                }
            });
        let stream = durable_tail_stream(
            durable,
            cursor,
            state.config.durable_poll_interval,
            state.config.durable_page_size,
            guard,
        );
        return Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
            .into_response();
    }

    let stream = sse_stream(state.bus.subscribe(), guard);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
}

fn parse_last_event_id(headers: &HeaderMap) -> Option<i64> {
    headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<i64>().ok())
}

/// Adapt a [`DurableTailSource`] into the [`Stream`] shape `axum`'s
/// [`Sse`] expects.
///
/// The stream polls the source on `poll_interval`, walks each page of
/// new events into SSE frames (one frame per row, with `id:` set to
/// the sequence so a reconnect can resume), and advances the cursor.
/// On a query error we emit a warn-log and back off one tick rather
/// than ending the stream — a transient SQLite hiccup should not force
/// every subscriber to reconnect.
fn durable_tail_stream(
    source: Arc<dyn DurableTailSource>,
    start_after: Option<i64>,
    poll_interval: Duration,
    page_size: usize,
    guard: SubscriberGuard,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    // State carries (source, current cursor, guard, pending page drain).
    // The drain queue absorbs a multi-row page so each poll yields one
    // SSE frame per `next()` rather than baking up a Vec into the
    // stream. When the queue empties, we sleep one `poll_interval` and
    // refill.
    let init = (
        source,
        start_after,
        guard,
        std::collections::VecDeque::<DurableEvent>::new(),
    );
    futures::stream::unfold(
        init,
        move |(source, cursor, guard, mut pending)| async move {
            // Drain any already-fetched page first.
            if let Some(event) = pending.pop_front() {
                let next_cursor = Some(event.sequence);
                let frame = Event::default()
                    .id(event.sequence.to_string())
                    .event(event.event_type)
                    .data(event.payload);
                return Some((Ok(frame), (source, next_cursor, guard, pending)));
            }
            // Backoff between polls. Yields a keep-alive comment frame so
            // the SSE wire stays warm even on quiet logs; production
            // consumers ignore comment lines.
            tokio::time::sleep(poll_interval).await;
            match source.events_after(cursor, page_size) {
                Ok(rows) => {
                    pending.extend(rows);
                    if let Some(event) = pending.pop_front() {
                        let next_cursor = Some(event.sequence);
                        let frame = Event::default()
                            .id(event.sequence.to_string())
                            .event(event.event_type)
                            .data(event.payload);
                        Some((Ok(frame), (source, next_cursor, guard, pending)))
                    } else {
                        Some((
                            Ok(Event::default().comment("idle")),
                            (source, cursor, guard, pending),
                        ))
                    }
                }
                Err(err) => {
                    warn!(error = %err, "durable-tail poll failed; backing off");
                    Some((
                        Ok(Event::default().comment("poll-error")),
                        (source, cursor, guard, pending),
                    ))
                }
            }
        },
    )
}

/// Adapt a broadcast subscription into the [`Stream`] shape `axum`'s
/// [`Sse`] expects.
///
/// Two things deserve callouts:
///
/// 1. **Lagged handling.** When the broadcast channel signals
///    [`BroadcastStreamRecvError::Lagged`], we emit one synthetic SSE
///    frame (`event: lagged`, `data: <count>`) and *end* the stream.
///    Continuing past a lag would silently desynchronise the consumer
///    from the orchestrator's actual state — it's safer to force a
///    reconnect, where the consumer can re-fetch a status snapshot.
///
/// 2. **JSON serialisation failure.** `Event::json_data` only fails if
///    serde rejects the value, which for [`OrchestratorEvent`] cannot
///    happen — every variant uses derives or owned strings. We log and
///    skip rather than crash, defending against a future refactor that
///    might introduce a non-serialisable field; `Infallible` keeps the
///    error type closed for axum.
///
/// Factored out of `events_handler` so unit tests can drive it without
/// spinning up the whole router.
fn sse_stream(
    rx: tokio_stream::wrappers::BroadcastStream<OrchestratorEvent>,
    guard: SubscriberGuard,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    // The state carries (the receiver, the guard, "have we already
    // emitted a lagged frame?"). When `done` is true the next poll
    // yields `None` and the unfold drops its state — releasing both the
    // broadcast subscription and the subscriber-counter slot.
    futures::stream::unfold((rx, guard, false), |(mut rx, guard, done)| async move {
        if done {
            return None;
        }
        match rx.next().await {
            None => None,
            Some(Ok(event)) => match Event::default().event(event.kind()).json_data(&event) {
                Ok(frame) => Some((Ok(frame), (rx, guard, false))),
                Err(err) => {
                    warn!(error = %err, kind = event.kind(), "skipping unserialisable event");
                    // Try the next event rather than ending the stream.
                    // Recursing would build unbounded future depth, so
                    // we synthesise a no-op comment frame and continue.
                    Some((Ok(Event::default().comment("skip")), (rx, guard, false)))
                }
            },
            Some(Err(BroadcastStreamRecvError::Lagged(missed))) => {
                warn!(
                    missed,
                    "SSE subscriber lagged; emitting lagged frame and closing"
                );
                let frame = Event::default().event("lagged").data(missed.to_string());
                // `done = true`: next poll returns None, terminating
                // the stream after this frame is delivered.
                Some((Ok(frame), (rx, guard, true)))
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_core::events::OrchestratorEvent;
    use symphony_core::state_machine::ClaimState;
    use symphony_core::tracker::IssueId;

    fn ev(id: &str) -> OrchestratorEvent {
        OrchestratorEvent::StateChanged {
            issue: IssueId::new(id),
            identifier: id.into(),
            previous: None,
            current: ClaimState::Running,
        }
    }

    #[tokio::test]
    async fn sse_stream_emits_one_frame_per_event() {
        let bus = EventBus::new(8);
        let state = SseState::new(bus.clone(), SseConfig::default());
        let guard = state.try_acquire().expect("slot");
        let mut stream = Box::pin(sse_stream(bus.subscribe(), guard));

        bus.emit(ev("ENG-1"));
        bus.emit(ev("ENG-2"));

        // First two frames are the events. We can't easily inspect the
        // SSE-wire bytes from outside axum, but the public `Event` API
        // is opaque enough that "got two Ok frames" is what this test
        // owes us.
        let _f1 = stream.next().await.expect("frame").expect("ok");
        let _f2 = stream.next().await.expect("frame").expect("ok");
    }

    #[tokio::test]
    async fn lagged_subscriber_receives_lagged_frame_then_stream_ends() {
        // Buffer of 2: emit five before consuming. The first read is
        // the synthetic Lagged frame; the next must be `None`, so a
        // reconnect is the only path forward.
        let bus = EventBus::new(2);
        let state = SseState::new(bus.clone(), SseConfig::default());
        let guard = state.try_acquire().expect("slot");
        let mut stream = Box::pin(sse_stream(bus.subscribe(), guard));

        for i in 0..5 {
            bus.emit(ev(&format!("ENG-{i}")));
        }

        let first = stream.next().await.expect("frame").expect("ok");
        // Crude shape check: the `Event` type is opaque, but its
        // `Display`/`Debug` includes the `event:` name we set.
        let dbg = format!("{first:?}");
        assert!(dbg.contains("lagged"), "expected lagged frame, got {dbg:?}");

        let next = stream.next().await;
        assert!(next.is_none(), "stream must end after lagged frame");
    }

    #[tokio::test]
    async fn try_acquire_enforces_max_subscribers() {
        let bus = EventBus::new(4);
        let state = SseState::new(
            bus,
            SseConfig {
                max_subscribers: 2,
                ..SseConfig::default()
            },
        );

        let g1 = state.try_acquire().expect("first slot");
        let g2 = state.try_acquire().expect("second slot");
        assert_eq!(state.active(), 2);
        assert!(state.try_acquire().is_none(), "third must be rejected");

        drop(g1);
        assert_eq!(state.active(), 1);

        // After releasing one, a new subscriber must succeed.
        let _g3 = state.try_acquire().expect("slot freed");
        assert_eq!(state.active(), 2);
        drop(g2);
    }

    #[tokio::test]
    async fn dropping_stream_releases_subscriber_slot() {
        let bus = EventBus::new(4);
        let state = SseState::new(
            bus.clone(),
            SseConfig {
                max_subscribers: 1,
                ..SseConfig::default()
            },
        );

        // Build and drop a stream; the embedded guard must release.
        {
            let guard = state.try_acquire().expect("slot");
            let _stream = sse_stream(bus.subscribe(), guard);
            assert_eq!(state.active(), 1);
        }
        assert_eq!(state.active(), 0, "drop must release the slot");
    }

    #[tokio::test]
    async fn router_rejects_when_over_cap() {
        // Smoke-check the router builds and that `try_acquire`
        // returns the expected 503 path.
        let bus = EventBus::new(4);
        let state = SseState::new(
            bus,
            SseConfig {
                max_subscribers: 1,
                ..SseConfig::default()
            },
        );
        let _g = state.try_acquire().expect("slot");
        let _router = router(state.clone());
        assert!(state.try_acquire().is_none());
    }

    /// End-to-end HTTP test: bind a real listener, drive it via
    /// [`serve`], connect a `reqwest` client, emit a scripted event
    /// sequence on the bus, and assert each frame arrives in order.
    ///
    /// This is the wiremock-style integration test the Phase-8
    /// checklist asks for: the server runs the same code path
    /// `symphony run` will use, and the client parses the SSE wire
    /// format byte-for-byte.
    #[tokio::test]
    async fn sse_server_streams_scripted_events_to_real_client() {
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let bus = EventBus::new(16);
        let state = SseState::new(bus.clone(), SseConfig::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let cancel = CancellationToken::new();

        let server = tokio::spawn(serve(listener, state, cancel.clone()));

        // Open the SSE connection. We deliberately skip any SSE-client
        // crate and read raw bytes — the daemon's TUI client is also
        // hand-rolled (per the next checklist item), so this exercises
        // the same parsing surface a production consumer will see.
        let url = format!("http://{addr}/events");
        let resp = reqwest::Client::new()
            .get(&url)
            .header("accept", "text/event-stream")
            .send()
            .await
            .expect("GET /events");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or("").to_string())
                .unwrap_or_default(),
            "text/event-stream"
        );

        // Wait for the server to register the subscriber before we
        // start emitting — broadcast events sent before subscription
        // are silently dropped by tokio::sync::broadcast, and a race
        // here would manifest as a flaky test.
        for _ in 0..50 {
            if bus.subscriber_count() >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            bus.subscriber_count() >= 1,
            "subscriber should have attached"
        );

        // Emit a scripted sequence the test will assert on, in order.
        let scripted: Vec<OrchestratorEvent> = vec![ev("ENG-1"), ev("ENG-2"), ev("ENG-3")];
        for e in &scripted {
            bus.emit(e.clone());
        }

        // Read chunks until we've parsed three `data: ` JSON lines, or
        // a generous wall-clock timeout fires. A real consumer reads
        // until either `\n\n` (frame boundary) or EOF; here we keep it
        // simple and accumulate into a `String`.
        let mut buf = String::new();
        let mut resp = resp;
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            while data_lines(&buf).len() < scripted.len() {
                let chunk = match resp.chunk().await {
                    Ok(Some(c)) => c,
                    Ok(None) => break, // server closed
                    Err(e) => panic!("chunk read failed: {e}"),
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
            }
        })
        .await;
        assert!(
            read.is_ok(),
            "timed out waiting for SSE frames; buf={buf:?}"
        );

        // Each `data:` line must parse as the matching OrchestratorEvent.
        let lines = data_lines(&buf);
        assert_eq!(lines.len(), scripted.len(), "frame count; raw={buf:?}");
        for (i, line) in lines.iter().enumerate() {
            let parsed: OrchestratorEvent =
                serde_json::from_str(line).expect("data: payload is valid JSON");
            // We compare via JSON round-trip: OrchestratorEvent doesn't
            // derive PartialEq (it carries enum payloads with f64s), so
            // structural equality lives on the wire.
            let expected = serde_json::to_value(&scripted[i]).unwrap();
            let got = serde_json::to_value(&parsed).unwrap();
            assert_eq!(got, expected, "frame {i} mismatch");
        }

        // Drop the response *before* cancelling — axum's graceful
        // shutdown waits for in-flight requests to drop. Holding the
        // response would deadlock the join below until the keep-alive
        // timer wins.
        drop(resp);

        cancel.cancel();
        let join = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server task did not exit before deadline")
            .expect("server task did not panic");
        assert!(join.is_ok(), "server returned error: {join:?}");
    }

    /// Extract the JSON payload from each `data: ...` line in an
    /// accumulated SSE buffer. Strips the `data: ` prefix and the
    /// trailing newline; ignores `event:` and comment lines, which the
    /// integration test asserts on separately if it cares.
    fn data_lines(buf: &str) -> Vec<&str> {
        buf.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect()
    }

    /// In-memory `DurableTailSource` for handler tests. Holds an
    /// append-only Vec keyed by sequence; `push_event` mimics the
    /// kernel writing an event into `events`.
    #[derive(Default)]
    struct FakeDurableTail {
        rows: std::sync::Mutex<Vec<DurableEvent>>,
    }

    impl FakeDurableTail {
        fn push(&self, event_type: &str, payload: &str) -> i64 {
            let mut rows = self.rows.lock().unwrap();
            let sequence = rows.last().map(|e| e.sequence).unwrap_or(0) + 1;
            rows.push(DurableEvent {
                sequence,
                event_type: event_type.into(),
                payload: payload.into(),
            });
            sequence
        }
    }

    impl DurableTailSource for FakeDurableTail {
        fn events_after(
            &self,
            after: Option<i64>,
            limit: usize,
        ) -> Result<Vec<DurableEvent>, StateError> {
            let rows = self.rows.lock().unwrap();
            let cutoff = after.unwrap_or(0);
            Ok(rows
                .iter()
                .filter(|e| e.sequence > cutoff)
                .take(limit)
                .cloned()
                .collect())
        }

        fn last_sequence(&self) -> Result<Option<i64>, StateError> {
            Ok(self.rows.lock().unwrap().last().map(|e| e.sequence))
        }
    }

    fn frame_dbg(event: &Event) -> String {
        format!("{event:?}")
    }

    #[tokio::test]
    async fn durable_tail_emits_persisted_events_in_sequence_order() {
        let tail = Arc::new(FakeDurableTail::default());
        tail.push("work_item.created", r#"{"id":1}"#);
        tail.push("run.started", r#"{"id":1}"#);

        let bus = EventBus::new(8);
        let state = SseState::with_durable_tail(
            bus,
            tail.clone(),
            SseConfig {
                durable_poll_interval: Duration::from_millis(10),
                ..SseConfig::default()
            },
        );
        let guard = state.try_acquire().expect("slot");
        let mut stream = Box::pin(durable_tail_stream(
            tail,
            None,
            Duration::from_millis(10),
            16,
            guard,
        ));

        // First poll picks up both rows; first frame is sequence 1.
        let f1 = stream.next().await.expect("frame").expect("ok");
        let dbg = frame_dbg(&f1);
        assert!(dbg.contains("work_item.created"), "got {dbg:?}");
        // VecDeque drain hands out the second row without a fresh poll.
        let f2 = stream.next().await.expect("frame").expect("ok");
        let dbg = frame_dbg(&f2);
        assert!(dbg.contains("run.started"), "got {dbg:?}");
    }

    #[tokio::test]
    async fn durable_tail_resume_from_cursor_skips_seen_events() {
        let tail = Arc::new(FakeDurableTail::default());
        tail.push("a", r#"{"k":1}"#);
        tail.push("b", r#"{"k":2}"#);
        tail.push("c", r#"{"k":3}"#);

        // Resume after sequence 2 — only "c" is new.
        let bus = EventBus::new(8);
        let _state = SseState::with_durable_tail(bus, tail.clone(), SseConfig::default());
        let active = Arc::new(AtomicUsize::new(1));
        let guard = SubscriberGuard {
            counter: active.clone(),
        };
        let mut stream = Box::pin(durable_tail_stream(
            tail,
            Some(2),
            Duration::from_millis(10),
            16,
            guard,
        ));

        let frame = stream.next().await.expect("frame").expect("ok");
        let dbg = frame_dbg(&frame);
        assert!(dbg.contains("event: c"), "got {dbg:?}");
        assert!(dbg.contains("id: 3"), "got {dbg:?}");
    }

    #[tokio::test]
    async fn durable_tail_picks_up_late_appends() {
        let tail = Arc::new(FakeDurableTail::default());
        tail.push("first", r#"{"n":1}"#);

        let active = Arc::new(AtomicUsize::new(1));
        let guard = SubscriberGuard {
            counter: active.clone(),
        };
        let mut stream = Box::pin(durable_tail_stream(
            tail.clone(),
            None,
            Duration::from_millis(10),
            16,
            guard,
        ));

        let f1 = stream.next().await.expect("frame").expect("ok");
        assert!(frame_dbg(&f1).contains("first"));

        // After the page drains the next poll yields a comment frame.
        // Push a new row and assert the *following* frame carries it —
        // the comment is acceptable noise on idle ticks.
        tail.push("second", r#"{"n":2}"#);
        // Drain frames until we see the new event or hit a budget.
        let mut saw_second = false;
        for _ in 0..5 {
            let f = stream.next().await.expect("frame").expect("ok");
            if frame_dbg(&f).contains("second") {
                saw_second = true;
                break;
            }
        }
        assert!(saw_second, "expected `second` event after late append");
    }

    #[test]
    fn parse_last_event_id_handles_well_formed_header() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "42".parse().unwrap());
        assert_eq!(parse_last_event_id(&headers), Some(42));
    }

    #[test]
    fn parse_last_event_id_returns_none_on_garbage() {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", "not-a-number".parse().unwrap());
        assert_eq!(parse_last_event_id(&headers), None);
    }

    #[test]
    fn state_db_tail_source_round_trips_events() {
        use symphony_state::events::NewEvent;
        use symphony_state::migrations::migrations;

        let mut db = StateDb::open_in_memory().expect("open");
        db.migrate(migrations()).expect("migrate");
        db.append_event(NewEvent {
            event_type: "x.created",
            work_item_id: None,
            run_id: None,
            payload: r#"{"hello":"world"}"#,
            now: "2026-05-08T00:00:00Z",
        })
        .expect("append");

        let source = StateDbTailSource::new(Arc::new(Mutex::new(db)));
        let last = source.last_sequence().expect("last");
        assert_eq!(last, Some(1));
        let rows = source.events_after(None, 10).expect("after");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_type, "x.created");
        assert_eq!(rows[0].payload, r#"{"hello":"world"}"#);
    }
}
