//! Single-active-consumer backend with optional replay.

use crate::WaitForLineResult;
use crate::output_stream::config::StreamConfig;
use crate::output_stream::consumer::driver::consume_sync;
use crate::output_stream::consumer::{spawn_consumer_async, spawn_consumer_sync};
use crate::output_stream::event::StreamEvent;
use crate::output_stream::line::adapter::LineAdapter;
use crate::output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, Replay, ReplayEnabled,
    ReplayRetention,
};
use crate::output_stream::visitors::factories::impl_consumer_factories;
use crate::output_stream::visitors::wait::WaitForLineSink;
use crate::output_stream::{OutputStream, Subscription, TrySubscribable};
use crate::{
    AsyncStreamVisitor, Consumer, LineParsingOptions, NumBytes, StreamConsumerError, StreamVisitor,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Per-backend return-type alias used by the [`impl_consumer_factories!`] macro to keep the
/// macro body backend-agnostic. Single-subscriber factories may fail when a consumer is
/// already active, so each factory return is wrapped in `Result<_, StreamConsumerError>`.
type FactoryReturn<T> = Result<T, StreamConsumerError>;

mod reader;
mod state;
mod subscription;

use reader::{read_chunked_best_effort, read_chunked_reliable};
use state::{ActiveSubscriber, ConfiguredShared};
use subscription::SingleSubscriberSubscription;

impl Subscription for mpsc::Receiver<StreamEvent> {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        self.recv()
    }
}

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing exactly one active consumer at a time. It is
/// useful when one inspector, collector, or line waiter should own the stream and accidental
/// concurrent fanout should be rejected early. It can reduce coordination overhead in some
/// single-consumer paths, but it is not a categorical throughput replacement for broadcast.
/// If multiple concurrent consumers are required, use the
/// `output_stream::backend::broadcast::BroadcastOutputStream`.
pub struct SingleSubscriberOutputStream<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    /// The task that reads output from the underlying stream and routes it to the active
    /// subscriber, replay storage, or discard path.
    stream_reader: JoinHandle<()>,

    /// Typed stream configuration selected by the process stream builder.
    options: StreamConfig<D, R>,

    /// Shared replay state for typed single-subscriber configurations.
    configured_shared: Arc<ConfiguredShared>,

    /// Name of this stream.
    name: &'static str,
}

impl<D, R> Drop for SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        self.stream_reader.abort();
        self.configured_shared.clear_active();
    }
}

impl<D, R> Debug for SingleSubscriberOutputStream<D, R>
where
    D: Delivery + Debug,
    R: Replay + Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleSubscriberOutputStream")
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field("options", &self.options)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<D, R> OutputStream for SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn read_chunk_size(&self) -> NumBytes {
        self.options.read_chunk_size
    }

    fn max_buffered_chunks(&self) -> usize {
        self.options.max_buffered_chunks
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl<D, R> SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Creates a new single-subscriber output stream from an async read stream and typed stream config.
    pub fn from_stream<S>(stream: S, stream_name: &'static str, options: StreamConfig<D, R>) -> Self
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        options.assert_valid("options");

        let shared = Arc::new(ConfiguredShared::new());
        let active_rx = shared.subscribe_active();
        let delivery_guarantee = options.delivery_guarantee();
        let replay_retention = options.replay_retention();

        let stream_reader = match delivery_guarantee {
            DeliveryGuarantee::BestEffort => tokio::spawn(read_chunked_best_effort(
                stream,
                Arc::clone(&shared),
                active_rx,
                options.read_chunk_size,
                replay_retention,
                stream_name,
            )),
            DeliveryGuarantee::ReliableForActiveSubscribers => tokio::spawn(read_chunked_reliable(
                stream,
                Arc::clone(&shared),
                active_rx,
                options.read_chunk_size,
                replay_retention,
                stream_name,
            )),
        };

        Self {
            stream_reader,
            options,
            configured_shared: shared,
            name: stream_name,
        }
    }

    /// Returns whether replay-specific APIs are enabled for this stream.
    #[must_use]
    pub fn replay_enabled(&self) -> bool {
        self.options.replay_enabled()
    }

    /// Returns the configured replay retention.
    #[must_use]
    pub fn replay_retention(&self) -> Option<ReplayRetention> {
        self.options.replay_retention()
    }

    fn take_subscription(&self) -> Result<SingleSubscriberSubscription, StreamConsumerError> {
        let shared = &self.configured_shared;

        let (sender, receiver) = mpsc::channel(self.options.max_buffered_chunks);
        let (id, replay, terminal_event) = {
            let mut state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");

            if state.active_id.is_some() {
                return Err(StreamConsumerError::ActiveConsumer {
                    stream_name: self.name,
                });
            }

            let replay = if state.replay_sealed || self.options.replay_retention().is_none() {
                VecDeque::default()
            } else {
                state.snapshot_events()
            };
            let id = state.attach_subscriber();
            shared
                .active_tx
                .send_replace(Some(Arc::new(ActiveSubscriber { id, sender })));
            (id, replay, state.terminal_event.clone())
        };

        Ok(SingleSubscriberSubscription {
            id,
            shared: Arc::clone(shared),
            replay,
            terminal_event,
            live_receiver: Some(receiver),
        })
    }
}

impl<D> SingleSubscriberOutputStream<D, ReplayEnabled>
where
    D: Delivery,
{
    /// Seals replay history for future subscribers.
    ///
    /// This is a one-way, idempotent operation.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    pub fn seal_replay(&self) {
        let mut state = self
            .configured_shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.replay_sealed = true;
        state.trim_replay_window(self.options.replay_retention());
    }

    /// Returns `true` once replay history has been sealed.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    #[must_use]
    pub fn is_replay_sealed(&self) -> bool {
        self.configured_shared
            .state
            .lock()
            .expect("single-subscriber state poisoned")
            .replay_sealed
    }
}

impl<D, R> TrySubscribable for SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn try_subscribe(&self) -> Result<impl Subscription, StreamConsumerError> {
        self.take_subscription()
    }
}

impl<D, R> SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Tries to drive the provided synchronous [`StreamVisitor`] over this stream.
    ///
    /// Returns a [`Consumer`] that owns the spawned task driving the visitor. All built-in
    /// `inspect_*`, `collect_*`, and `wait_for_line` factories construct a built-in visitor and
    /// call this method internally.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer (single-subscriber
    /// streams allow only one active consumer or line waiter at a time).
    pub fn consume_with<V>(&self, visitor: V) -> Result<Consumer<V::Output>, StreamConsumerError>
    where
        V: StreamVisitor,
    {
        Ok(spawn_consumer_sync(
            self.name(),
            self.take_subscription()?,
            visitor,
        ))
    }

    /// Tries to drive the provided asynchronous [`AsyncStreamVisitor`] over this stream.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn consume_with_async<V>(
        &self,
        visitor: V,
    ) -> Result<Consumer<V::Output>, StreamConsumerError>
    where
        V: AsyncStreamVisitor,
    {
        Ok(spawn_consumer_async(
            self.name(),
            self.take_subscription()?,
            visitor,
        ))
    }

    impl_consumer_factories!();

    /// Tries to wait for a line that matches the given predicate within `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the line waiter.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn wait_for_line(
        &self,
        timeout: Duration,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> Result<
        impl Future<Output = Result<WaitForLineResult, crate::StreamReadError>> + Send + 'static,
        StreamConsumerError,
    > {
        let subscription = self.take_subscription()?;
        let visitor = LineAdapter::new(options, WaitForLineSink::new(predicate));
        Ok(async move {
            let (_term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
            match tokio::time::timeout(timeout, consume_sync(subscription, visitor, term_sig_rx))
                .await
            {
                Ok(Ok(true)) => Ok(WaitForLineResult::Matched),
                Ok(Ok(false)) => Ok(WaitForLineResult::StreamClosed),
                Ok(Err(err)) => Err(err),
                Err(_) => Ok(WaitForLineResult::Timeout),
            }
        })
    }
}

#[cfg(test)]
mod tests;
