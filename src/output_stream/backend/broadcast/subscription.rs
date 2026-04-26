use super::state::{Shared, SubscriberId, evict_locked};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    BestEffortDelivery, Delivery, NoReplay, Replay, StreamConfig, StreamEvent,
};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

#[derive(Debug)]
pub(super) struct FastSubscription {
    pub(super) receiver: broadcast::Receiver<StreamEvent>,
    pub(super) emit_terminal_event: Option<StreamEvent>,
}

impl FastSubscription {
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        if let Some(event) = self.emit_terminal_event.take() {
            return Some(event);
        }

        match self.receiver.recv().await {
            Ok(event) => Some(event),
            Err(RecvError::Closed) => None,
            Err(RecvError::Lagged(lagged)) => {
                tracing::warn!(lagged, "Broadcast subscriber is lagging behind");
                Some(StreamEvent::Gap)
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SharedSubscription<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    pub(super) shared: Arc<Shared>,
    pub(super) id: SubscriberId,
    pub(super) options: StreamConfig<D, R>,
    pub(super) done: bool,
}

impl<D, R> Drop for SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        if !self.done {
            let mut state = self.shared.state.lock().expect("broadcast state poisoned");
            state.remove_subscriber(self.id);
            evict_locked(&mut state, self.options);
            drop(state);
            self.shared.reader_available.notify_waiters();
        }
    }
}

impl<D, R> SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        loop {
            let notified = self.shared.output_available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut state = self.shared.state.lock().expect("broadcast state poisoned");
                let Some(subscriber) = state.subscribers.get(&self.id) else {
                    self.done = true;
                    return None;
                };
                let cursor = subscriber.cursor;

                if cursor < state.buffer_start_seq {
                    let buffer_start_seq = state.buffer_start_seq;
                    if let Some(subscriber) = state.subscribers.get_mut(&self.id) {
                        subscriber.cursor = buffer_start_seq;
                    }
                    return Some(StreamEvent::Gap);
                }

                if let Some(event) = state.event_at(cursor) {
                    if let Some(subscriber) = state.subscribers.get_mut(&self.id) {
                        subscriber.cursor += 1;
                    }
                    if matches!(event, StreamEvent::Eof | StreamEvent::ReadError(_)) {
                        state.remove_subscriber(self.id);
                        self.done = true;
                    }
                    evict_locked(&mut state, self.options);
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(event);
                }

                if state.eof {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(StreamEvent::Eof);
                }

                if let Some(err) = state.read_error.clone() {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(StreamEvent::ReadError(err));
                }

                if state.closed {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return None;
                }
            };

            notified.as_mut().await;
        }
    }
}

#[derive(Debug)]
pub(super) enum Subscription<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    Fast(FastSubscription),
    Shared(SharedSubscription<D, R>),
}

impl<D, R> Subscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        match self {
            Subscription::Fast(subscription) => subscription.recv().await,
            Subscription::Shared(subscription) => subscription.recv().await,
        }
    }
}

impl<D, R> EventSubscription for Subscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    #[allow(
        clippy::manual_async_fn,
        reason = "the trait method must expose a Send future for tokio::spawn"
    )]
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        async move { self.recv().await }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{Shared, append_event, evict_locked};
    use super::*;
    use crate::StreamReadError;
    use crate::output_stream::Chunk;
    use crate::{NumBytesExt, ReliableDelivery, ReplayEnabled, ReplayRetention};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::sync::Arc;
    use std::time::Duration;

    fn chunk(bytes: &'static [u8]) -> StreamEvent {
        StreamEvent::Chunk(Chunk(Bytes::from_static(bytes)))
    }

    fn best_effort_options(
        retention: ReplayRetention,
    ) -> StreamConfig<BestEffortDelivery, ReplayEnabled> {
        let builder = StreamConfig::builder().best_effort_delivery();
        match retention {
            ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
            ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
            ReplayRetention::All => builder.replay_all(),
        }
        .read_chunk_size(3.bytes())
        .max_buffered_chunks(1)
        .build()
    }

    fn reliable_no_replay_options() -> StreamConfig<ReliableDelivery, NoReplay> {
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(1.bytes())
            .max_buffered_chunks(1)
            .build()
    }

    fn reliable_replay_options(
        retention: ReplayRetention,
    ) -> StreamConfig<ReliableDelivery, ReplayEnabled> {
        let builder = StreamConfig::builder().reliable_for_active_subscribers();
        match retention {
            ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
            ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
            ReplayRetention::All => builder.replay_all(),
        }
        .read_chunk_size(1.bytes())
        .max_buffered_chunks(4)
        .build()
    }

    fn subscribe<D, R>(
        shared: &Arc<Shared>,
        cursor: u64,
        options: StreamConfig<D, R>,
    ) -> SharedSubscription<D, R>
    where
        D: Delivery,
        R: Replay,
    {
        let id = shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .add_subscriber(cursor);
        SharedSubscription {
            shared: Arc::clone(shared),
            id,
            options,
            done: false,
        }
    }

    fn late_replay_cursor(shared: &Shared) -> u64 {
        shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .replay_start_seq
    }

    fn late_live_cursor(shared: &Shared) -> u64 {
        shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .next_seq
    }

    async fn assert_next_chunk<D, R>(
        subscription: &mut SharedSubscription<D, R>,
        expected: &'static [u8],
    ) where
        D: Delivery,
        R: Replay,
    {
        match subscription.recv().await {
            Some(StreamEvent::Chunk(chunk)) => {
                assert_that!(chunk.as_ref()).is_equal_to(expected);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected chunk, got {other:?}"));
            }
        }
    }

    #[tokio::test]
    async fn slow_best_effort_subscriber_observes_gap_then_retained_tail() {
        let options = best_effort_options(ReplayRetention::LastChunks(1));
        let shared = Arc::new(Shared::new());
        let mut subscription = subscribe(&shared, 0, options);

        append_event(&shared, options, chunk(b"rea")).await;
        append_event(&shared, options, chunk(b"dy\n")).await;
        append_event(&shared, options, StreamEvent::Eof).await;

        assert_that!(subscription.recv().await)
            .is_some()
            .is_equal_to(StreamEvent::Gap);
        assert_next_chunk(&mut subscription, b"dy\n").await;
        assert_that!(subscription.recv().await)
            .is_some()
            .is_equal_to(StreamEvent::Eof);
    }

    #[tokio::test]
    async fn eof_is_replayed_to_late_subscribers_before_seal() {
        let options = reliable_replay_options(ReplayRetention::All);
        let shared = Arc::new(Shared::new());

        append_event(&shared, options, chunk(b"tail")).await;
        append_event(&shared, options, StreamEvent::Eof).await;

        let cursor = late_replay_cursor(&shared);
        let mut subscription = subscribe(&shared, cursor, options);
        assert_next_chunk(&mut subscription, b"tail").await;
        assert_that!(subscription.recv().await)
            .is_some()
            .is_equal_to(StreamEvent::Eof);
    }

    #[tokio::test]
    async fn no_replay_late_subscriber_observes_terminal_read_error() {
        let options = reliable_no_replay_options();
        let shared = Arc::new(Shared::new());

        append_event(&shared, options, chunk(b"booting\n")).await;
        append_event(
            &shared,
            options,
            StreamEvent::ReadError(StreamReadError::new(
                "custom",
                io::Error::from(io::ErrorKind::BrokenPipe),
            )),
        )
        .await;

        let cursor = late_live_cursor(&shared);
        let mut subscription = subscribe(&shared, cursor, options);
        match subscription.recv().await {
            Some(StreamEvent::ReadError(err)) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
            }
        }
    }

    #[tokio::test]
    async fn replay_late_subscriber_observes_retained_output_then_read_error() {
        let options = reliable_replay_options(ReplayRetention::All);
        let shared = Arc::new(Shared::new());

        append_event(&shared, options, chunk(b"booting\npartial")).await;
        append_event(
            &shared,
            options,
            StreamEvent::ReadError(StreamReadError::new(
                "custom",
                io::Error::from(io::ErrorKind::BrokenPipe),
            )),
        )
        .await;

        let cursor = late_replay_cursor(&shared);
        let mut subscription = subscribe(&shared, cursor, options);
        assert_next_chunk(&mut subscription, b"booting\npartial").await;
        match subscription.recv().await {
            Some(StreamEvent::ReadError(err)) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
            }
        }
    }

    #[tokio::test]
    async fn dropping_slow_subscriber_after_seal_frees_retained_history() {
        let options = reliable_replay_options(ReplayRetention::All);
        let shared = Arc::new(Shared::new());
        let slow = subscribe(&shared, 0, options);

        append_event(&shared, options, chunk(b"old\n")).await;
        {
            let mut state = shared.state.lock().expect("broadcast state poisoned");
            state.replay_sealed = true;
            evict_locked(&mut state, options);
            assert_that!(state.events.len()).is_equal_to(1);
            assert_that!(state.retained_chunk_count).is_equal_to(1);
            assert_that!(state.retained_byte_count).is_equal_to(4);
            assert_that!(state.replay_byte_count).is_equal_to(0);
        }

        drop(slow);

        let state = shared.state.lock().expect("broadcast state poisoned");
        assert_that!(state.events.len()).is_equal_to(0);
        assert_that!(state.retained_chunk_count).is_equal_to(0);
        assert_that!(state.retained_byte_count).is_equal_to(0);
    }

    #[tokio::test]
    async fn dropping_slow_reliable_subscriber_unpins_buffer() {
        let options = reliable_no_replay_options();
        let shared = Arc::new(Shared::new());
        let slow = subscribe(&shared, 0, options);
        let mut fast = subscribe(&shared, 0, options);

        append_event(&shared, options, chunk(b"a")).await;
        assert_next_chunk(&mut fast, b"a").await;

        {
            let state = shared.state.lock().expect("broadcast state poisoned");
            assert_that!(state.events.len()).is_equal_to(1);
            assert_that!(state.retained_chunk_count).is_equal_to(1);
            assert_that!(state.retained_byte_count).is_equal_to(1);
        }

        drop(slow);

        let state = shared.state.lock().expect("broadcast state poisoned");
        assert_that!(state.events.len()).is_equal_to(0);
        assert_that!(state.retained_chunk_count).is_equal_to(0);
        assert_that!(state.retained_byte_count).is_equal_to(0);
    }

    #[tokio::test]
    async fn dropping_slow_subscriber_unblocks_stream_consumption() {
        let options = reliable_no_replay_options();
        let shared = Arc::new(Shared::new());
        let slow = subscribe(&shared, 0, options);

        append_event(&shared, options, chunk(b"a")).await;
        let shared_for_append = Arc::clone(&shared);
        let mut second_append =
            tokio::spawn(
                async move { append_event(&shared_for_append, options, chunk(b"b")).await },
            );

        assert_that!(
            tokio::time::timeout(Duration::from_millis(25), &mut second_append)
                .await
                .is_err()
        )
        .is_true();

        drop(slow);

        second_append.await.unwrap();
        let state = shared.state.lock().expect("broadcast state poisoned");
        assert_that!(state.next_seq).is_equal_to(2);
        assert_that!(state.events.len()).is_equal_to(0);
    }

    #[tokio::test]
    async fn late_replay_subscriber_starts_at_bounded_replay_window_when_buffer_start_is_pinned() {
        let options = reliable_replay_options(ReplayRetention::LastChunks(1));
        let shared = Arc::new(Shared::new());
        let _pinned_subscription = subscribe(&shared, 0, options);

        append_event(&shared, options, chunk(b"a")).await;
        append_event(&shared, options, chunk(b"b")).await;

        {
            let state = shared.state.lock().expect("broadcast state poisoned");
            assert_that!(state.buffer_start_seq).is_equal_to(0);
            assert_that!(state.replay_start_seq).is_equal_to(1);
        }

        let cursor = late_replay_cursor(&shared);
        let mut subscription = subscribe(&shared, cursor, options);
        assert_next_chunk(&mut subscription, b"b").await;
    }
}
