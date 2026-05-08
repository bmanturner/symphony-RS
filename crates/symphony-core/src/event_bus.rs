//! Broadcast event bus for [`OrchestratorEvent`]s.
//!
//! This module is the *runtime carrier* for the wire format defined in
//! [`crate::events`]. The orchestrator emits one event per observable
//! lifecycle inflection (claim, retry-arm, reconcile, release, agent
//! re-emission). Subscribers — today the `symphony watch` SSE server in
//! `symphony-cli`; tomorrow other observers — fan out from a
//! [`tokio::sync::broadcast`] channel without having to plumb a custom
//! pub/sub layer through every transition site.
//!
//! # Why `tokio::sync::broadcast`?
//!
//! Every subscriber gets *every* event. There is no work-stealing or
//! single-consumer routing; the bus is purely observational. The
//! broadcast channel's slow-consumer behaviour — buffered events
//! eventually overwrite oldest with `Lagged` — matches how an SSE
//! consumer should see a momentary disconnect: a hint that it dropped
//! frames, not silent data loss. The Phase-8 SSE handler will surface
//! `Lagged` as a `disconnected` notice and let the TUI reconnect.
//!
//! # Replay buffer sizing
//!
//! The buffer is the per-subscriber backlog *and* the maximum
//! out-of-band burst before lag. We default to **256** which comfortably
//! covers a poll-tick (a few `StateChanged`s and `Released`s) plus a
//! short burst of agent re-emission frames. Operators can override via
//! `WORKFLOW.md status.replay_buffer`. The minimum honoured is 1 — the
//! broadcast channel rejects zero, and a zero-sized bus is never the
//! intent.
//!
//! # Pure addition
//!
//! Constructing an [`EventBus`] is free of side effects and never
//! blocks. [`EventBus::emit`] is non-blocking and never errors back to
//! the caller — events with no subscribers are dropped silently because
//! the orchestrator must not stall on observability.

use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::events::OrchestratorEvent;

/// Default replay buffer if the operator doesn't override it. Sized to
/// hold a few poll ticks' worth of events plus a short agent burst
/// without forcing a `Lagged` on a healthy subscriber.
pub const DEFAULT_REPLAY_BUFFER: usize = 256;

/// Multi-producer multi-subscriber bus carrying [`OrchestratorEvent`]s.
///
/// Cheaply [`Clone`]able — clones share the same channel. Construct one
/// at orchestrator startup, hand a clone to whatever wants to emit
/// (today: only [`crate::poll_loop::PollLoop`]) and another to whatever
/// wants to observe (today: the `symphony-cli` SSE handler; in tests:
/// [`Self::subscribe`] directly).
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<OrchestratorEvent>,
}

impl EventBus {
    /// Build a bus with `buffer` slots in its replay queue. Values
    /// below 1 are clamped up — see the module docs for sizing notes.
    #[must_use]
    pub fn new(buffer: usize) -> Self {
        let buffer = buffer.max(1);
        let (sender, _initial_rx) = broadcast::channel(buffer);
        Self { sender }
    }

    /// Subscribe and adapt to a [`Stream`](futures::Stream) so callers
    /// can `.next().await` rather than threading the raw broadcast
    /// receiver through their plumbing. `Lagged` errors surface as
    /// `Err` items on the stream — the SSE handler converts those to a
    /// disconnect notice for downstream consumers.
    #[must_use]
    pub fn subscribe(&self) -> BroadcastStream<OrchestratorEvent> {
        BroadcastStream::new(self.sender.subscribe())
    }

    /// Best-effort emit. Returns silently when no subscribers are
    /// attached — that is the steady state for a daemon running without
    /// `symphony watch` connected, and we deliberately do not force the
    /// caller to handle "no observers" as an error.
    pub fn emit(&self, event: OrchestratorEvent) {
        // `broadcast::Sender::send` returns the dropped event on
        // `SendError` when there are zero receivers. That is not a
        // failure mode for us.
        let _ = self.sender.send(event);
    }

    /// Number of currently-attached subscribers. Useful for deciding
    /// whether to skip expensive event construction (e.g. agent
    /// re-emission) when nobody is listening.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    /// Builds a bus with [`DEFAULT_REPLAY_BUFFER`] slots.
    fn default() -> Self {
        Self::new(DEFAULT_REPLAY_BUFFER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_machine::ClaimState;
    use crate::tracker::IssueId;
    use futures::StreamExt;

    fn state_changed(id: &str) -> OrchestratorEvent {
        OrchestratorEvent::StateChanged {
            issue: IssueId::new(id),
            identifier: id.into(),
            previous: None,
            current: ClaimState::Running,
        }
    }

    #[tokio::test]
    async fn subscribe_then_emit_delivers_event() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.emit(state_changed("ENG-1"));
        let got = rx.next().await.expect("stream item").expect("not lagged");
        assert_eq!(got.kind(), "state_changed");
    }

    #[tokio::test]
    async fn emit_without_subscribers_is_a_noop() {
        let bus = EventBus::new(4);
        // Should not panic, hang, or block.
        bus.emit(state_changed("ENG-1"));
        assert_eq!(bus.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn clone_shares_the_same_channel() {
        let bus = EventBus::new(8);
        let echo = bus.clone();
        let mut rx = echo.subscribe();
        bus.emit(state_changed("ENG-2"));
        let got = rx.next().await.expect("item").expect("not lagged");
        assert!(matches!(got, OrchestratorEvent::StateChanged { .. }));
    }

    #[tokio::test]
    async fn buffer_zero_clamps_to_at_least_one() {
        // broadcast::channel(0) panics; the bus must clamp.
        let bus = EventBus::new(0);
        let mut rx = bus.subscribe();
        bus.emit(state_changed("ENG-3"));
        assert!(rx.next().await.expect("item").is_ok());
    }

    #[tokio::test]
    async fn slow_subscriber_receives_lagged_then_recovers() {
        // Buffer of 2: we send 5 events without consuming, then start
        // reading. The first read must be `Lagged`, after which the
        // stream resumes with the freshest events.
        let bus = EventBus::new(2);
        let mut rx = bus.subscribe();
        for i in 0..5 {
            bus.emit(state_changed(&format!("ENG-{i}")));
        }
        // First item: Lagged signal.
        let first = rx.next().await.expect("item");
        assert!(first.is_err(), "expected lag signal, got {first:?}");
        // Subsequent items: real events again. The stream has at least
        // one buffered survivor.
        let next = rx
            .next()
            .await
            .expect("item")
            .expect("post-lag event delivered");
        assert!(matches!(next, OrchestratorEvent::StateChanged { .. }));
    }

    #[tokio::test]
    async fn subscriber_count_tracks_active_subscribers() {
        let bus = EventBus::new(4);
        assert_eq!(bus.subscriber_count(), 0);
        let _rx1 = bus.subscribe();
        let _rx2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);
        drop(_rx1);
        // Dropping the stream wrapper releases the inner receiver.
        assert_eq!(bus.subscriber_count(), 1);
    }
}
