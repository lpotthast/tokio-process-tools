use crate::collector::{AsyncChunkCollector, AsyncLineCollector, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::consumer;
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    BestEffortDelivery, Chunk, CollectedBytes, CollectedLines, Delivery, DeliveryGuarantee,
    LineCollectionOptions, LineParsingOptions, LineWriteMode, Next, NoReplay, OutputStream,
    RawCollectionOptions, Replay, ReplayEnabled, ReplayRetention, SinkWriteErrorHandler,
    StreamConfig, StreamEvent, WriteCollectionOptions,
};
use crate::{NumBytes, StreamReadError};
use std::borrow::Cow;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

mod reader;
mod state;
mod subscription;

pub use crate::output_stream::line_waiter::LineWaiter;
use reader::{read_chunked_fast, read_chunked_shared};
use state::{Shared, evict_locked};
use subscription::{FastSubscription, SharedSubscription, Subscription};

struct FastClosureState {
    closed: bool,
    read_error: Option<StreamReadError>,
}

struct FastBackend {
    stream_reader: JoinHandle<()>,
    sender: broadcast::Sender<StreamEvent>,
    closure_state: Arc<Mutex<FastClosureState>>,
    options: StreamConfig<BestEffortDelivery, NoReplay>,
    name: &'static str,
}

struct SharedBackend<D, R>
where
    D: Delivery,
    R: Replay,
{
    stream_reader: JoinHandle<()>,
    shared: Arc<Shared>,
    options: StreamConfig<D, R>,
    name: &'static str,
}

enum Backend<D, R>
where
    D: Delivery,
    R: Replay,
{
    Fast(FastBackend),
    Shared(SharedBackend<D, R>),
}

/// The output stream from a process using a multi-consumer broadcast backend.
///
/// Broadcast streams support multiple consumers and can optionally retain replay history for
/// consumers that attach after output has already arrived.
pub struct BroadcastOutputStream<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    backend: Backend<D, R>,
}

impl<D, R> OutputStream for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn read_chunk_size(&self) -> NumBytes {
        match &self.backend {
            Backend::Fast(backend) => backend.options.read_chunk_size,
            Backend::Shared(backend) => backend.options.read_chunk_size,
        }
    }

    fn max_buffered_chunks(&self) -> usize {
        match &self.backend {
            Backend::Fast(backend) => backend.options.max_buffered_chunks,
            Backend::Shared(backend) => backend.options.max_buffered_chunks,
        }
    }

    fn name(&self) -> &'static str {
        match &self.backend {
            Backend::Fast(backend) => backend.name,
            Backend::Shared(backend) => backend.name,
        }
    }
}

impl<D, R> Drop for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        match &self.backend {
            Backend::Fast(backend) => {
                backend.stream_reader.abort();
            }
            Backend::Shared(backend) => {
                backend.stream_reader.abort();
                {
                    let mut state = backend
                        .shared
                        .state
                        .lock()
                        .expect("broadcast state poisoned");
                    state.closed = true;
                }
                backend.shared.output_available.notify_waiters();
                backend.shared.reader_available.notify_waiters();
            }
        }
    }
}

impl<D, R> Debug for BroadcastOutputStream<D, R>
where
    D: Delivery + Debug,
    R: Replay + Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("BroadcastOutputStream");
        debug.field("output_collector", &"non-debug < JoinHandle<()> >");
        match &self.backend {
            Backend::Fast(backend) => {
                debug.field("backend", &"tokio::sync::broadcast");
                debug.field("options", &backend.options);
                debug.field("name", &backend.name);
            }
            Backend::Shared(backend) => {
                debug.field("backend", &"shared replay");
                debug.field("options", &backend.options);
                debug.field("name", &backend.name);
            }
        }
        debug.finish_non_exhaustive()
    }
}

fn wait_for_line<S>(
    subscription: S,
    predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
    options: LineParsingOptions,
    timeout: Option<Duration>,
) -> LineWaiter
where
    S: EventSubscription,
{
    LineWaiter::new(consumer::wait_for_line_with_optional_timeout(
        subscription,
        predicate,
        options,
        timeout,
    ))
}

impl<D, R> BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Creates a new broadcast output stream from an async read stream and typed stream config.
    pub fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        stream_name: &'static str,
        options: StreamConfig<D, R>,
    ) -> Self {
        options.assert_valid("options");

        let shared = Arc::new(Shared::new());
        if options.delivery_guarantee() == DeliveryGuarantee::BestEffort
            && !options.replay_enabled()
        {
            let fast_options = StreamConfig {
                read_chunk_size: options.read_chunk_size,
                max_buffered_chunks: options.max_buffered_chunks,
                delivery: BestEffortDelivery,
                replay: NoReplay,
            };
            let (sender, receiver) = broadcast::channel::<StreamEvent>(options.max_buffered_chunks);
            drop(receiver);
            let closure_state = Arc::new(Mutex::new(FastClosureState {
                closed: false,
                read_error: None,
            }));
            let stream_reader = tokio::spawn(read_chunked_fast(
                stream,
                options.read_chunk_size,
                sender.clone(),
                Arc::clone(&closure_state),
                stream_name,
            ));

            return Self {
                backend: Backend::Fast(FastBackend {
                    stream_reader,
                    sender,
                    closure_state,
                    options: fast_options,
                    name: stream_name,
                }),
            };
        }

        let stream_reader = tokio::spawn(read_chunked_shared(
            stream,
            Arc::clone(&shared),
            options,
            stream_name,
        ));

        Self {
            backend: Backend::Shared(SharedBackend {
                stream_reader,
                shared,
                options,
                name: stream_name,
            }),
        }
    }
}

impl<D> BroadcastOutputStream<D, ReplayEnabled>
where
    D: Delivery,
{
    /// Seals replay history for future subscribers.
    ///
    /// This is a one-way, idempotent operation. Active subscribers keep their unread tail data
    /// according to the configured delivery policy.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    pub fn seal_replay(&self) {
        let Backend::Shared(backend) = &self.backend else {
            return;
        };
        {
            let mut state = backend
                .shared
                .state
                .lock()
                .expect("broadcast state poisoned");
            state.replay_sealed = true;
            evict_locked(&mut state, backend.options);
        }
        backend.shared.reader_available.notify_waiters();
        backend.shared.output_available.notify_waiters();
    }

    /// Returns `true` once replay history has been sealed.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    #[must_use]
    pub fn is_replay_sealed(&self) -> bool {
        let Backend::Shared(backend) = &self.backend else {
            return false;
        };
        backend
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .replay_sealed
    }
}

impl<D, R> BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn subscribe(&self) -> Subscription<D, R> {
        let Backend::Shared(backend) = &self.backend else {
            panic!("shared broadcast subscription requested for fast backend");
        };
        let mut state = backend
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        let cursor = match backend.options.replay_retention() {
            Some(
                ReplayRetention::LastChunks(_)
                | ReplayRetention::LastBytes(_)
                | ReplayRetention::All,
            ) if !state.replay_sealed => state.replay_start_seq,
            None | Some(_) => state.next_seq,
        };

        let id = state.add_subscriber(cursor);
        Subscription::Shared(SharedSubscription {
            shared: Arc::clone(&backend.shared),
            id,
            options: backend.options,
            done: false,
        })
    }

    fn subscribe_normal(&self) -> Subscription<D, R> {
        match &self.backend {
            Backend::Fast(backend) => {
                let (receiver, emit_terminal_event) = {
                    let state = backend
                        .closure_state
                        .lock()
                        .expect("closure_state poisoned");
                    let receiver = backend.sender.subscribe();
                    let terminal_event = state
                        .read_error
                        .clone()
                        .map(StreamEvent::ReadError)
                        .or_else(|| state.closed.then_some(StreamEvent::Eof));
                    (receiver, terminal_event)
                };

                Subscription::Fast(FastSubscription {
                    receiver,
                    emit_terminal_event,
                })
            }
            Backend::Shared(_) => self.subscribe(),
        }
    }

    /// Inspects chunks of output from the stream without storing them.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&self, f: impl FnMut(Chunk) -> Next + Send + 'static) -> Inspector {
        consumer::inspect_chunks(self.name(), self.subscribe_normal(), f)
    }

    /// Inspects lines of output from the stream without storing them.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector {
        consumer::inspect_lines(self.name(), self.subscribe_normal(), f, options)
    }

    /// Inspects lines of output from the stream without storing them, using an async closure.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Fut + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        consumer::inspect_lines_async(self.name(), self.subscribe_normal(), f, options)
    }

    /// Collects chunks from the stream into a sink.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Chunk, &mut S) + Send + 'static,
    ) -> Collector<S> {
        consumer::collect_chunks(self.name(), self.subscribe_normal(), into, collect)
    }

    /// Collects chunks from the stream into a sink using an async collector.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, C>(&self, into: S, collect: C) -> Collector<S>
    where
        S: Sink,
        C: AsyncChunkCollector<S>,
    {
        consumer::collect_chunks_async(self.name(), self.subscribe_normal(), into, collect)
    }

    /// Collects lines from the stream into a sink.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Cow<'_, str>, &mut S) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Collector<S> {
        consumer::collect_lines(self.name(), self.subscribe_normal(), into, collect, options)
    }

    /// Collects lines from the stream into a sink using an async collector.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, C>(
        &self,
        into: S,
        collect: C,
        options: LineParsingOptions,
    ) -> Collector<S>
    where
        S: Sink,
        C: AsyncLineCollector<S>,
    {
        consumer::collect_lines_async(self.name(), self.subscribe_normal(), into, collect, options)
    }

    /// Convenience method to collect chunks into a bounded byte vector.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_vec(
        &self,
        options: RawCollectionOptions,
    ) -> Collector<CollectedBytes> {
        self.collect_chunks(CollectedBytes::new(), move |chunk, collected| {
            collected.push_chunk(chunk.as_ref(), options);
        })
    }

    /// Trusted-output-only convenience method to collect all chunks into a `Vec<u8>`.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its
    /// output volume are trusted.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_all_chunks_into_vec_trusted(&self) -> Collector<Vec<u8>> {
        self.collect_chunks(Vec::new(), |chunk, vec| {
            vec.extend_from_slice(chunk.as_ref());
        })
    }

    /// Convenience method to collect lines into a bounded line buffer.
    ///
    /// `parsing_options.max_line_length` must be non-zero. An unbounded single-line parser can
    /// allocate without limit before this collector receives any complete line.
    ///
    /// # Panics
    ///
    /// Panics if `parsing_options.max_line_length` is zero.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines> {
        assert!(
            parsing_options.max_line_length.bytes() > 0,
            "parsing_options.max_line_length must be greater than zero for bounded line collection"
        );
        self.collect_lines(
            CollectedLines::new(),
            move |line, collected| {
                collected.push_line(line.into_owned(), collection_options);
                Next::Continue
            },
            parsing_options,
        )
    }

    /// Trusted-output-only convenience method to collect all lines into a `Vec<String>`.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its
    /// output volume are trusted. `LineParsingOptions::max_line_length` still controls the maximum
    /// size of one parsed line.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_all_lines_into_vec_trusted(
        &self,
        options: LineParsingOptions,
    ) -> Collector<Vec<String>> {
        self.collect_lines(
            Vec::new(),
            |line, vec| {
                vec.push(line.into_owned());
                Next::Continue
            },
            options,
        )
    }

    /// Collects chunks into an async writer.
    ///
    /// Sink write failures are handled according to `write_options`. Use
    /// `WriteCollectionOptions::fail_fast()` to stop collection and return
    /// [`crate::CollectorError::SinkWrite`] from [`Collector::wait`] or
    /// [`Collector::cancel`], `WriteCollectionOptions::log_and_continue()` to log each
    /// failure and keep collecting, or `WriteCollectionOptions::with_error_handler(...)`
    /// to make a per-error continue-or-stop decision.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write<W, H>(
        &self,
        write: W,
        write_options: WriteCollectionOptions<H>,
    ) -> Collector<W>
    where
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        consumer::collect_chunks_into_write(
            self.name(),
            self.subscribe_normal(),
            write,
            write_options,
        )
    }

    /// Collects lines into an async writer.
    ///
    /// Sink write failures are handled according to `write_options`. Use
    /// `WriteCollectionOptions::fail_fast()` to stop collection and return
    /// [`crate::CollectorError::SinkWrite`] from [`Collector::wait`] or
    /// [`Collector::cancel`], `WriteCollectionOptions::log_and_continue()` to log each
    /// failure and keep collecting, or `WriteCollectionOptions::with_error_handler(...)`
    /// to make a per-error continue-or-stop decision.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write<W, H>(
        &self,
        write: W,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Collector<W>
    where
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        consumer::collect_lines_into_write(
            self.name(),
            self.subscribe_normal(),
            write,
            options,
            mode,
            write_options,
        )
    }

    /// Collects chunks into an async writer after mapping them with the provided function.
    ///
    /// Sink write failures are handled according to `write_options`. Use
    /// `WriteCollectionOptions::fail_fast()` to stop collection and return
    /// [`crate::CollectorError::SinkWrite`] from [`Collector::wait`] or
    /// [`Collector::cancel`], `WriteCollectionOptions::log_and_continue()` to log each
    /// failure and keep collecting, or `WriteCollectionOptions::with_error_handler(...)`
    /// to make a per-error continue-or-stop decision.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write_mapped<W, B, H>(
        &self,
        write: W,
        mapper: impl Fn(Chunk) -> B + Send + Sync + Copy + 'static,
        write_options: WriteCollectionOptions<H>,
    ) -> Collector<W>
    where
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
        H: SinkWriteErrorHandler,
    {
        consumer::collect_chunks_into_write_mapped(
            self.name(),
            self.subscribe_normal(),
            write,
            mapper,
            write_options,
        )
    }

    /// Collects lines into an async writer after mapping them with the provided function.
    ///
    /// Sink write failures are handled according to `write_options`. Use
    /// `WriteCollectionOptions::fail_fast()` to stop collection and return
    /// [`crate::CollectorError::SinkWrite`] from [`Collector::wait`] or
    /// [`Collector::cancel`], `WriteCollectionOptions::log_and_continue()` to log each
    /// failure and keep collecting, or `WriteCollectionOptions::with_error_handler(...)`
    /// to make a per-error continue-or-stop decision.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write_mapped<W, B, H>(
        &self,
        write: W,
        mapper: impl Fn(Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Collector<W>
    where
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
        H: SinkWriteErrorHandler,
    {
        consumer::collect_lines_into_write_mapped(
            self.name(),
            self.subscribe_normal(),
            write,
            mapper,
            options,
            mode,
            write_options,
        )
    }

    /// Waits for a line that matches the given predicate.
    ///
    /// The waiter starts at the earliest output currently available to new consumers. With replay
    /// enabled and unsealed, that can include retained past output; otherwise it starts at live
    /// output.
    #[must_use]
    pub fn wait_for_line(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> LineWaiter {
        let subscription = self.subscribe_normal();
        wait_for_line(subscription, predicate, options, None)
    }

    /// Waits for a line that matches the given predicate, with a timeout.
    ///
    /// The waiter starts at the earliest output currently available to new consumers. With replay
    /// enabled and unsealed, that can include retained past output; otherwise it starts at live
    /// output.
    #[must_use]
    pub fn wait_for_line_with_timeout(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
        timeout: Duration,
    ) -> LineWaiter {
        let subscription = self.subscribe_normal();
        wait_for_line(subscription, predicate, options, Some(timeout))
    }
}

#[cfg(test)]
mod tests;
