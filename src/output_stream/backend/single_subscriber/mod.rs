//! Single-active-consumer backend with optional replay.

use crate::WaitForLineResult;
use crate::output_stream::config::StreamConfig;
use crate::output_stream::consumer::consumer::{spawn_consumer_async, spawn_consumer_sync};
use crate::output_stream::consumer::line_waiter::LineWaiter;
use crate::output_stream::consumer::visitor::consume_sync;
use crate::output_stream::consumer::visitors::collect::{
    CollectChunks, CollectChunksAsync, CollectLines, CollectLinesAsync,
};
use crate::output_stream::consumer::visitors::inspect::{
    InspectChunks, InspectChunksAsync, InspectLines, InspectLinesAsync,
};
use crate::output_stream::consumer::visitors::wait::WaitForLine;
use crate::output_stream::consumer::visitors::write::{WriteChunks, WriteLines};
use crate::output_stream::event::StreamEvent;
use crate::output_stream::line::LineParserState;
use crate::output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, Replay, ReplayEnabled,
    ReplayRetention,
};
use crate::output_stream::{OutputStream, Subscription, TrySubscribable};
use crate::{
    AsyncChunkCollector, AsyncLineCollector, AsyncStreamVisitor, Chunk, CollectedBytes,
    CollectedLines, Consumer, LineCollectionOptions, LineParsingOptions, LineWriteMode, Next,
    NumBytes, RawCollectionOptions, Sink, SinkWriteError, SinkWriteErrorHandler,
    StreamConsumerError, StreamVisitor, WriteCollectionOptions,
};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

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

    /// Tries to inspect chunks of output from the stream without storing them.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn inspect_chunks(
        &self,
        f: impl FnMut(Chunk) -> Next + Send + 'static,
    ) -> Result<Consumer<()>, StreamConsumerError> {
        self.consume_with(InspectChunks::builder().f(f).build())
    }

    /// Tries to inspect chunks of output from the stream without storing them, using an async
    /// closure.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn inspect_chunks_async<Fut>(
        &self,
        f: impl FnMut(Chunk) -> Fut + Send + 'static,
    ) -> Result<Consumer<()>, StreamConsumerError>
    where
        Fut: Future<Output = Next> + Send + 'static,
    {
        self.consume_with_async(InspectChunksAsync::builder().f(f).build())
    }

    /// Tries to inspect lines of output from the stream without storing them.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn inspect_lines(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Result<Consumer<()>, StreamConsumerError> {
        self.consume_with(
            InspectLines::builder()
                .parser(LineParserState::new())
                .options(options)
                .f(f)
                .build(),
        )
    }

    /// Tries to inspect lines of output from the stream without storing them, using an async
    /// closure.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn inspect_lines_async<Fut>(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Fut + Send + 'static,
        options: LineParsingOptions,
    ) -> Result<Consumer<()>, StreamConsumerError>
    where
        Fut: Future<Output = Next> + Send + 'static,
    {
        self.consume_with_async(
            InspectLinesAsync::builder()
                .parser(LineParserState::new())
                .options(options)
                .f(f)
                .build(),
        )
    }

    /// Tries to collect chunks from the stream into a sink.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Chunk, &mut S) + Send + 'static,
    ) -> Result<Consumer<S>, StreamConsumerError> {
        self.consume_with(CollectChunks::builder().sink(into).f(collect).build())
    }

    /// Tries to collect chunks from the stream into a sink using an async collector.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_chunks_async<S, C>(
        &self,
        into: S,
        collect: C,
    ) -> Result<Consumer<S>, StreamConsumerError>
    where
        S: Sink,
        C: AsyncChunkCollector<S>,
    {
        self.consume_with_async(
            CollectChunksAsync::builder()
                .sink(into)
                .collector(collect)
                .build(),
        )
    }

    /// Tries to collect lines from the stream into a sink.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Cow<'_, str>, &mut S) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Result<Consumer<S>, StreamConsumerError> {
        self.consume_with(
            CollectLines::builder()
                .parser(LineParserState::new())
                .options(options)
                .sink(into)
                .f(collect)
                .build(),
        )
    }

    /// Tries to collect lines from the stream into a sink using an async collector.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_lines_async<S, C>(
        &self,
        into: S,
        collect: C,
        options: LineParsingOptions,
    ) -> Result<Consumer<S>, StreamConsumerError>
    where
        S: Sink,
        C: AsyncLineCollector<S>,
    {
        self.consume_with_async(
            CollectLinesAsync::builder()
                .parser(LineParserState::new())
                .options(options)
                .sink(into)
                .collector(collect)
                .build(),
        )
    }

    /// Tries to collect chunks into a bounded byte vector.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_chunks_into_vec(
        &self,
        options: RawCollectionOptions,
    ) -> Result<Consumer<CollectedBytes>, StreamConsumerError> {
        self.consume_with(
            CollectChunks::builder()
                .sink(CollectedBytes::new())
                .f(move |chunk: Chunk, sink: &mut CollectedBytes| {
                    sink.push_chunk(chunk.as_ref(), options);
                })
                .build(),
        )
    }

    /// Tries to collect lines into a line buffer.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    ///
    /// # Panics
    ///
    /// Panics if `parsing_options.max_line_length` is zero and bounded collection is used.
    pub fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Result<Consumer<CollectedLines>, StreamConsumerError> {
        self.consume_with(
            CollectLines::builder()
                .parser(LineParserState::new())
                .options(parsing_options)
                .sink(CollectedLines::new())
                .f(move |line: Cow<'_, str>, sink: &mut CollectedLines| {
                    sink.push_line(line.into_owned(), collection_options);
                    Next::Continue
                })
                .build(),
        )
    }

    /// Tries to collect chunks into an async writer.
    ///
    /// Sink write failures are handled according to `write_options`. Use
    /// [`WriteCollectionOptions::fail_fast`] to surface the [`SinkWriteError`] as the inner
    /// `Err` of the resulting `Result<W, SinkWriteError>`,
    /// [`WriteCollectionOptions::log_and_continue`] to log each failure and keep collecting, or
    /// [`WriteCollectionOptions::with_error_handler`] to make a per-error continue-or-stop
    /// decision.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_chunks_into_write<W, H>(
        &self,
        write: W,
        write_options: WriteCollectionOptions<H>,
    ) -> Result<Consumer<Result<W, SinkWriteError>>, StreamConsumerError>
    where
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        self.consume_with_async(
            WriteChunks::builder()
                .stream_name(self.name())
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper((|chunk: Chunk| chunk) as fn(Chunk) -> Chunk)
                .error(None)
                .build(),
        )
    }

    /// Tries to collect lines into an async writer.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_lines_into_write<W, H>(
        &self,
        write: W,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Result<Consumer<Result<W, SinkWriteError>>, StreamConsumerError>
    where
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        self.consume_with_async(
            WriteLines::builder()
                .stream_name(self.name())
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper((|line: Cow<'_, str>| line.into_owned()) as fn(Cow<'_, str>) -> String)
                .parser(LineParserState::new())
                .options(options)
                .mode(mode)
                .error(None)
                .build(),
        )
    }

    /// Tries to collect chunks into an async writer after mapping them.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_chunks_into_write_mapped<W, B, H>(
        &self,
        write: W,
        mapper: impl Fn(Chunk) -> B + Send + Sync + 'static,
        write_options: WriteCollectionOptions<H>,
    ) -> Result<Consumer<Result<W, SinkWriteError>>, StreamConsumerError>
    where
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send + 'static,
        H: SinkWriteErrorHandler,
    {
        self.consume_with_async(
            WriteChunks::builder()
                .stream_name(self.name())
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper(mapper)
                .error(None)
                .build(),
        )
    }

    /// Tries to collect lines into an async writer after mapping them.
    ///
    /// # Errors
    ///
    /// Returns [`StreamConsumerError`] if the backend rejects the consumer.
    pub fn collect_lines_into_write_mapped<W, B, H>(
        &self,
        write: W,
        mapper: impl Fn(Cow<'_, str>) -> B + Send + Sync + 'static,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Result<Consumer<Result<W, SinkWriteError>>, StreamConsumerError>
    where
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send + 'static,
        H: SinkWriteErrorHandler,
    {
        self.consume_with_async(
            WriteLines::builder()
                .stream_name(self.name())
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper(mapper)
                .parser(LineParserState::new())
                .options(options)
                .mode(mode)
                .error(None)
                .build(),
        )
    }

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
    ) -> Result<LineWaiter, StreamConsumerError> {
        let subscription = self.take_subscription()?;
        let visitor = WaitForLine::builder()
            .parser(LineParserState::new())
            .options(options)
            .predicate(predicate)
            .matched(false)
            .build();
        Ok(LineWaiter::new(async move {
            let (_term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
            match tokio::time::timeout(timeout, consume_sync(subscription, visitor, term_sig_rx))
                .await
            {
                Ok(Ok(true)) => Ok(WaitForLineResult::Matched),
                Ok(Ok(false)) => Ok(WaitForLineResult::StreamClosed),
                Ok(Err(err)) => Err(err),
                Err(_) => Ok(WaitForLineResult::Timeout),
            }
        }))
    }
}

#[cfg(test)]
mod tests;
