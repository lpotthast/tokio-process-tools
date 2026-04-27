use super::state::{BestEffortLiveQueue, IndexedEvent, Shared, SubscriberId};
use crate::output_stream::Subscription;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::{BestEffortDelivery, Delivery, NoReplay, Replay};
use std::collections::VecDeque;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

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
pub(super) enum LiveReceiver {
    Reliable(mpsc::Receiver<IndexedEvent>),
    BestEffort(Arc<BestEffortLiveQueue>),
    Closed,
}

impl LiveReceiver {
    async fn recv(&mut self) -> Option<IndexedEvent> {
        match self {
            Self::Reliable(receiver) => receiver.recv().await,
            Self::BestEffort(queue) => queue.recv().await,
            Self::Closed => None,
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
    pub(super) id: Option<SubscriberId>,
    pub(super) replay: VecDeque<IndexedEvent>,
    pub(super) live_start_seq: u64,
    pub(super) live_receiver: LiveReceiver,
    pub(super) _marker: PhantomData<fn() -> (D, R)>,
    pub(super) done: bool,
}

impl<D, R> Drop for SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        if !self.done
            && let Some(id) = self.id.take()
        {
            let mut state = self.shared.state.lock().expect("broadcast state poisoned");
            state.remove_subscriber(id);
        }
    }
}

impl<D, R> SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        if let Some(event) = self.replay.pop_front() {
            if matches!(event.event, StreamEvent::Eof | StreamEvent::ReadError(_)) {
                self.detach();
            }
            return Some(event.event);
        }

        loop {
            let event = self.live_receiver.recv().await?;
            if event.seq < self.live_start_seq {
                continue;
            }
            if matches!(event.event, StreamEvent::Eof | StreamEvent::ReadError(_)) {
                self.detach();
            }
            return Some(event.event);
        }
    }

    fn detach(&mut self) {
        if let Some(id) = self.id.take() {
            let mut state = self.shared.state.lock().expect("broadcast state poisoned");
            state.remove_subscriber(id);
        }
        self.done = true;
    }
}

#[derive(Debug)]
pub(super) enum BroadcastSubscription<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    Fast(FastSubscription),
    Shared(SharedSubscription<D, R>),
}

impl<D, R> BroadcastSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        match self {
            BroadcastSubscription::Fast(subscription) => subscription.recv().await,
            BroadcastSubscription::Shared(subscription) => subscription.recv().await,
        }
    }
}

impl<D, R> Subscription for BroadcastSubscription<D, R>
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
    use super::super::state::{SubscriberSender, append_event};
    use super::*;
    use crate::StreamReadError;
    use crate::output_stream::event::Chunk;
    use crate::{NumBytesExt, ReliableDelivery, ReplayEnabled, ReplayRetention, StreamConfig};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;

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
        options: StreamConfig<D, R>,
    ) -> SharedSubscription<D, R>
    where
        D: Delivery,
        R: Replay,
    {
        let (sender, live_receiver) = match options.delivery_guarantee() {
            crate::DeliveryGuarantee::ReliableForActiveSubscribers => {
                let (sender, receiver) = mpsc::channel(options.max_buffered_chunks);
                (
                    SubscriberSender::Reliable(sender),
                    LiveReceiver::Reliable(receiver),
                )
            }
            crate::DeliveryGuarantee::BestEffort => {
                let queue = Arc::new(BestEffortLiveQueue::new(options.max_buffered_chunks));
                (
                    SubscriberSender::BestEffort(Arc::clone(&queue)),
                    LiveReceiver::BestEffort(queue),
                )
            }
        };

        let mut state = shared.state.lock().expect("broadcast state poisoned");
        let (replay, live_start_seq) = state.replay_snapshot(options);
        let id = if state.closed || state.terminal.is_some() {
            None
        } else {
            Some(state.add_subscriber(sender))
        };
        drop(state);

        SharedSubscription {
            shared: Arc::clone(shared),
            id,
            replay,
            live_start_seq,
            live_receiver,
            _marker: PhantomData,
            done: false,
        }
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
    async fn slow_best_effort_subscriber_observes_gap_then_newer_tail() {
        let options = best_effort_options(ReplayRetention::LastChunks(1));
        let shared = Arc::new(Shared::new());
        let mut subscription = subscribe(&shared, options);

        append_event(&shared, options, chunk(b"old")).await;
        append_event(&shared, options, chunk(b"new")).await;
        append_event(&shared, options, StreamEvent::Eof).await;

        assert_that!(subscription.recv().await)
            .is_some()
            .is_equal_to(StreamEvent::Gap);
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

        let mut subscription = subscribe(&shared, options);
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

        let mut subscription = subscribe(&shared, options);
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

        let mut subscription = subscribe(&shared, options);
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
    async fn active_subscription_does_not_duplicate_live_handoff() {
        let options = reliable_replay_options(ReplayRetention::All);
        let shared = Arc::new(Shared::new());

        append_event(&shared, options, chunk(b"replay")).await;
        let mut subscription = subscribe(&shared, options);
        append_event(&shared, options, chunk(b"live")).await;
        append_event(&shared, options, StreamEvent::Eof).await;

        assert_next_chunk(&mut subscription, b"replay").await;
        assert_next_chunk(&mut subscription, b"live").await;
        assert_that!(subscription.recv().await)
            .is_some()
            .is_equal_to(StreamEvent::Eof);
    }
}
