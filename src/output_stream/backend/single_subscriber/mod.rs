use crate::collector::{AsyncChunkCollector, AsyncLineCollector, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::consumer;
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    Chunk, CollectedBytes, CollectedLines, Delivery, LineCollectionOptions, LineWriteMode, Next,
    OutputStream, RawCollectionOptions, Replay, ReplayRetention, SinkWriteErrorHandler,
    StreamConfig, StreamEvent, WriteCollectionOptions,
};
use crate::{LineParsingOptions, NumBytes};
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

#[cfg(test)]
use crate::DeliveryGuarantee;
pub use crate::output_stream::line_waiter::LineWaiter;
use reader::read_chunked;
use state::{ActiveSubscriber, ConfiguredShared};
use subscription::{ConfiguredSubscription, SingleSubscription};

impl EventSubscription for mpsc::Receiver<StreamEvent> {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        self.recv()
    }
}

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one active consumer at a time.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::backend::broadcast::BroadcastOutputStream`.
pub struct SingleSubscriberOutputStream {
    /// The task that reads output from the underlying stream and routes it to the active
    /// subscriber, replay storage, or discard path.
    stream_reader: JoinHandle<()>,

    /// The maximum size of every chunk read by the backing `stream_reader`.
    read_chunk_size: NumBytes,

    /// The maximum capacity of the channel caching the chunks before being processed.
    max_buffered_chunks: usize,

    /// Shared replay state for typed single-subscriber configurations.
    configured_shared: Option<Arc<ConfiguredShared>>,

    /// Replay retention for typed single-subscriber configurations.
    replay_retention: Option<ReplayRetention>,

    /// Whether replay-specific APIs are enabled.
    replay_enabled: bool,

    /// Name of this stream.
    name: &'static str,
}

impl OutputStream for SingleSubscriberOutputStream {
    fn read_chunk_size(&self) -> NumBytes {
        self.read_chunk_size
    }

    fn max_buffered_chunks(&self) -> usize {
        self.max_buffered_chunks
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl Drop for SingleSubscriberOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
    }
}

impl Debug for SingleSubscriberOutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleSubscriberOutputStream")
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field("configured", &self.configured_shared.is_some())
            .finish_non_exhaustive()
    }
}

impl SingleSubscriberOutputStream {
    #[cfg(test)]
    pub(crate) fn from_stream_with_delivery_guarantee<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        stream_name: &'static str,
        delivery_guarantee: DeliveryGuarantee,
        read_chunk_size: NumBytes,
        max_buffered_chunks: usize,
    ) -> SingleSubscriberOutputStream {
        match delivery_guarantee {
            DeliveryGuarantee::BestEffort => Self::from_stream(
                stream,
                stream_name,
                StreamConfig::builder()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(read_chunk_size)
                    .max_buffered_chunks(max_buffered_chunks)
                    .build(),
            ),
            DeliveryGuarantee::ReliableForActiveSubscribers => Self::from_stream(
                stream,
                stream_name,
                StreamConfig::builder()
                    .reliable_for_active_subscribers()
                    .no_replay()
                    .read_chunk_size(read_chunk_size)
                    .max_buffered_chunks(max_buffered_chunks)
                    .build(),
            ),
        }
    }

    /// Creates a new single-subscriber output stream from an async read stream and typed stream config.
    pub fn from_stream<S, D, R>(
        stream: S,
        stream_name: &'static str,
        options: StreamConfig<D, R>,
    ) -> SingleSubscriberOutputStream
    where
        S: AsyncRead + Unpin + Send + 'static,
        D: Delivery,
        R: Replay,
    {
        options.assert_valid("options");

        let shared = Arc::new(ConfiguredShared::new());
        let active_rx = shared.subscribe_active();
        let delivery_guarantee = options.delivery_guarantee();
        let replay_retention = options.replay_retention();
        let replay_enabled = options.replay_enabled();

        let stream_reader = tokio::spawn(read_chunked(
            stream,
            Arc::clone(&shared),
            active_rx,
            options.read_chunk_size,
            delivery_guarantee,
            replay_retention,
            stream_name,
        ));

        SingleSubscriberOutputStream {
            stream_reader,
            read_chunk_size: options.read_chunk_size,
            max_buffered_chunks: options.max_buffered_chunks,
            configured_shared: Some(shared),
            replay_retention,
            replay_enabled,
            name: stream_name,
        }
    }

    /// Returns whether replay-specific APIs are enabled for this stream.
    #[must_use]
    pub fn replay_enabled(&self) -> bool {
        self.replay_enabled
    }

    /// Returns the configured replay retention.
    #[must_use]
    pub fn replay_retention(&self) -> Option<ReplayRetention> {
        self.replay_retention
    }

    fn panic_on_multiple_consumers(&self) -> ! {
        panic!(
            "Cannot create multiple active consumers on SingleSubscriberOutputStream (stream: '{}'). \
            Only one active inspector, collector, or line waiter can be active at a time. \
            Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.",
            self.name
        )
    }

    fn take_subscription(&self) -> SingleSubscription {
        self.take_configured_subscription()
    }

    fn take_configured_subscription(&self) -> SingleSubscription {
        let Some(shared) = &self.configured_shared else {
            panic!("configured single-subscriber subscription requested without shared state");
        };

        let (sender, receiver) = mpsc::channel(self.max_buffered_chunks);
        let (id, replay, terminal_event) = {
            let mut state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");

            if state.active_id.is_some() {
                drop(state);
                self.panic_on_multiple_consumers();
            }

            let replay = if state.replay_sealed || self.replay_retention.is_none() {
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

        ConfiguredSubscription {
            id,
            shared: Arc::clone(shared),
            replay,
            terminal_event,
            live_receiver: Some(receiver),
        }
    }

    /// Seals replay history for future subscribers.
    ///
    /// This is a one-way, idempotent operation.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    pub fn seal_replay(&self) {
        let Some(shared) = &self.configured_shared else {
            return;
        };
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.replay_sealed = true;
        state.trim_replay_window(self.replay_retention);
    }

    /// Returns `true` once replay history has been sealed.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    #[must_use]
    pub fn is_replay_sealed(&self) -> bool {
        let Some(shared) = &self.configured_shared else {
            return false;
        };
        shared
            .state
            .lock()
            .expect("single-subscriber state poisoned")
            .replay_sealed
    }
}

// Impls for inspecting the output of the stream.
impl SingleSubscriberOutputStream {
    /// Inspects chunks of output from the stream without storing them.
    ///
    /// The provided closure is called for each chunk of data. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&self, f: impl FnMut(Chunk) -> Next + Send + 'static) -> Inspector {
        consumer::inspect_chunks(self.name(), self.take_subscription(), f)
    }

    /// Inspects lines of output from the stream without storing them.
    ///
    /// The provided closure is called for each line. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector {
        consumer::inspect_lines(self.name(), self.take_subscription(), f, options)
    }

    /// Inspects lines of output from the stream without storing them, using an async closure.
    ///
    /// The provided async closure is called for each line. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &self,
        f: impl FnMut(Cow<'_, str>) -> Fut + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        consumer::inspect_lines_async(self.name(), self.take_subscription(), f, options)
    }
}

// Impls for collecting the output of the stream.
impl SingleSubscriberOutputStream {
    /// Collects chunks from the stream into a sink.
    ///
    /// The provided closure is called for each chunk, with mutable access to the sink.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Chunk, &mut S) + Send + 'static,
    ) -> Collector<S> {
        consumer::collect_chunks(self.name(), self.take_subscription(), into, collect)
    }

    /// Collects chunks from the stream into a sink using an async collector.
    ///
    /// The provided async collector is called for each chunk, with mutable access to the sink.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use tokio_process_tools::{
    ///     AsyncChunkCollector, Chunk, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Next,
    ///     Process,
    /// };
    ///
    /// struct ExtendChunks;
    ///
    /// impl AsyncChunkCollector<Vec<u8>> for ExtendChunks {
    ///     async fn collect<'a>(&'a mut self, chunk: Chunk, bytes: &'a mut Vec<u8>) -> Next {
    ///         bytes.extend_from_slice(chunk.as_ref());
    ///         Next::Continue
    ///     }
    /// }
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let process = Process::new(tokio::process::Command::new("some-command"))
    ///     .auto_name()
    ///     .stdout_and_stderr(|stream| {
    ///         stream
    ///             .single_subscriber()
    ///             .best_effort_delivery()
    ///             .no_replay()
    ///             .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
    ///             .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
    ///     })
    ///     .spawn()?;
    /// let collector = process.stdout().collect_chunks_async(Vec::new(), ExtendChunks);
    /// # drop(collector);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, C>(&self, into: S, collect: C) -> Collector<S>
    where
        S: Sink,
        C: AsyncChunkCollector<S>,
    {
        consumer::collect_chunks_async(self.name(), self.take_subscription(), into, collect)
    }

    /// Collects lines from the stream into a sink.
    ///
    /// The provided closure is called for each line, with mutable access to the sink.
    /// Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        collect: impl FnMut(Cow<'_, str>, &mut S) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Collector<S> {
        consumer::collect_lines(
            self.name(),
            self.take_subscription(),
            into,
            collect,
            options,
        )
    }

    /// Collects lines from the stream into a sink using an async collector.
    ///
    /// The provided async collector is called for each line, with mutable access to the sink.
    /// Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::borrow::Cow;
    /// use tokio_process_tools::{
    ///     AsyncLineCollector, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
    ///     LineParsingOptions, Next, Process,
    /// };
    ///
    /// struct PushLines;
    ///
    /// impl AsyncLineCollector<Vec<String>> for PushLines {
    ///     async fn collect<'a>(
    ///         &'a mut self,
    ///         line: Cow<'a, str>,
    ///         lines: &'a mut Vec<String>,
    ///     ) -> Next {
    ///         lines.push(line.into_owned());
    ///         Next::Continue
    ///     }
    /// }
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let process = Process::new(tokio::process::Command::new("some-command"))
    ///     .auto_name()
    ///     .stdout_and_stderr(|stream| {
    ///         stream
    ///             .single_subscriber()
    ///             .best_effort_delivery()
    ///             .no_replay()
    ///             .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
    ///             .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
    ///     })
    ///     .spawn()?;
    /// let collector = process.stdout().collect_lines_async(
    ///     Vec::new(),
    ///     PushLines,
    ///     LineParsingOptions::default(),
    /// );
    /// # drop(collector);
    /// # Ok(())
    /// # }
    /// ```
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
        consumer::collect_lines_async(
            self.name(),
            self.take_subscription(),
            into,
            collect,
            options,
        )
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
            self.take_subscription(),
            write,
            write_options,
        )
    }

    /// Collects lines into an async writer.
    ///
    /// Parsed lines no longer include their trailing newline byte, so `mode` controls whether a
    /// `\n` delimiter should be reintroduced for each emitted line.
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
            self.take_subscription(),
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
            self.take_subscription(),
            write,
            mapper,
            write_options,
        )
    }

    /// Collects lines into an async writer after mapping them with the provided function.
    ///
    /// `mode` applies after `mapper`: choose [`LineWriteMode::AsIs`] when the mapped output
    /// already contains delimiters, or [`LineWriteMode::AppendLf`] to append `\n` after each
    /// mapped line.
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
            self.take_subscription(),
            write,
            mapper,
            options,
            mode,
            write_options,
        )
    }
}

// Impls for waiting for a specific line of output.
impl SingleSubscriberOutputStream {
    /// Waits for a line that matches the given predicate.
    ///
    /// Returns `Ok(`[`crate::WaitForLineResult::Matched`]`)` if a matching line is found, or
    /// `Ok(`[`crate::WaitForLineResult::StreamClosed`]`)` if the stream ends first.
    /// This method never returns [`crate::WaitForLineResult::Timeout`]; use
    /// [`SingleSubscriberOutputStream::wait_for_line_with_timeout`] if you need a bounded wait.
    ///
    /// This method claims the single-subscriber stream while the returned waiter is active. Once
    /// the waiter completes or is dropped, another consumer can attach. Use the broadcast stream
    /// implementation if you need multiple concurrent consumers.
    /// The waiter starts at the earliest output currently available to new consumers. With replay
    /// enabled and unsealed, that can include retained past output; otherwise it starts at live
    /// output.
    ///
    /// When chunks are dropped in [`crate::DeliveryGuarantee::BestEffort`] mode, this waiter
    /// discards any partial line in progress and resynchronizes at the next newline instead of
    /// matching across the gap.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StreamReadError`] if the underlying stream fails while being read.
    pub fn wait_for_line(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> LineWaiter {
        let subscription = self.take_subscription();
        LineWaiter::new(consumer::wait_for_line(subscription, predicate, options))
    }

    /// Waits for a line that matches the given predicate, with a timeout.
    ///
    /// Returns `Ok(`[`crate::WaitForLineResult::Matched`]`)` if a matching line is found,
    /// `Ok(`[`crate::WaitForLineResult::StreamClosed`]`)` if the stream ends first, or
    /// `Ok(`[`crate::WaitForLineResult::Timeout`]`)` if the timeout expires first.
    /// This is the only line-wait variant on this type that can return
    /// [`crate::WaitForLineResult::Timeout`].
    ///
    /// This method claims the single-subscriber stream while the returned waiter is active. Once
    /// the waiter completes, times out, or is dropped, another consumer can attach. Use the
    /// broadcast stream implementation if you need multiple concurrent consumers.
    /// The waiter starts at the earliest output currently available to new consumers. With replay
    /// enabled and unsealed, that can include retained past output; otherwise it starts at live
    /// output.
    ///
    /// When chunks are dropped in [`crate::DeliveryGuarantee::BestEffort`] mode, this waiter
    /// discards any partial line in progress and resynchronizes at the next newline instead of
    /// matching across the gap.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StreamReadError`] if the underlying stream fails while being read.
    pub fn wait_for_line_with_timeout(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
        timeout: Duration,
    ) -> LineWaiter {
        let subscription = self.take_subscription();
        LineWaiter::new(consumer::wait_for_line_with_optional_timeout(
            subscription,
            predicate,
            options,
            Some(timeout),
        ))
    }
}

#[cfg(test)]
mod tests;
