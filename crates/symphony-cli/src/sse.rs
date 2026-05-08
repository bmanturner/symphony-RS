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
//! This module owns just the *server* half of that boundary. Wiring it
//! into [`crate::run::run`]'s lifecycle (start with the orchestrator,
//! drain on SIGINT) is the next checklist item — keeping the surface
//! split lets us land and test the request handler without entangling
//! it in the binary's startup path.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::Stream;
use futures::StreamExt;
use symphony_core::event_bus::EventBus;
use symphony_core::events::OrchestratorEvent;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{debug, warn};

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

/// Tunables for the SSE handler. Cheaply [`Clone`]able — the router
/// stores one copy and hands clones into request handlers.
#[derive(Debug, Clone)]
pub struct SseConfig {
    /// Hard cap on concurrent subscribers. Past this, new connections
    /// receive `503 Service Unavailable`.
    pub max_subscribers: usize,
}

impl Default for SseConfig {
    fn default() -> Self {
        Self {
            max_subscribers: DEFAULT_MAX_SUBSCRIBERS,
        }
    }
}

/// Application state passed to the `GET /events` handler.
///
/// Holds a clone of the orchestrator's [`EventBus`] (so subscribers see
/// the same stream the daemon emits), the typed [`SseConfig`] knobs,
/// and an atomic counter tracking active subscribers. The counter is
/// shared via `Arc` so the [`SubscriberGuard`] can decrement it on
/// drop without needing access to the `Router` state.
#[derive(Debug, Clone)]
pub struct SseState {
    bus: EventBus,
    config: SseConfig,
    active: Arc<AtomicUsize>,
}

impl SseState {
    /// Build state from an existing [`EventBus`] (typically the same
    /// instance the orchestrator emits onto) and the configured caps.
    #[must_use]
    pub fn new(bus: EventBus, config: SseConfig) -> Self {
        Self {
            bus,
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

/// `GET /events` handler. Fast-fails with 503 when over cap; otherwise
/// returns a long-lived SSE response that stays open until the client
/// disconnects or the broadcast channel signals an unrecoverable lag.
async fn events_handler(State(state): State<SseState>) -> Response {
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

    let stream = sse_stream(state.bus.subscribe(), guard);
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
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
        let state = SseState::new(bus, SseConfig { max_subscribers: 2 });

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
        let state = SseState::new(bus.clone(), SseConfig { max_subscribers: 1 });

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
        // returns the expected 503 path. We don't spin up an HTTP
        // server here — the next checklist item adds the wiremock-
        // style integration test in `symphony run` lifecycle.
        let bus = EventBus::new(4);
        let state = SseState::new(bus, SseConfig { max_subscribers: 1 });
        let _g = state.try_acquire().expect("slot");
        let _router = router(state.clone());
        assert!(state.try_acquire().is_none());
    }
}
