use super::visitor::{
    CollectChunks, CollectChunksAsync, CollectLines, CollectLinesAsync, drive_async, drive_sync,
};
use crate::StreamReadError;
use crate::output_stream::event::Chunk;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::num_bytes::NumBytes;
use crate::output_stream::{Next, Subscription};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Controls which output is retained once a bounded in-memory collection reaches its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollectionOverflowBehavior {
    /// Keep the first retained output and discard additional output.
    #[default]
    DropAdditionalData,

    /// Keep the newest retained output by evicting older retained output.
    DropOldestData,
}

/// Options for collecting raw output bytes into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawCollectionOptions {
    /// Retain at most `max_bytes` bytes in memory.
    Bounded {
        /// Maximum number of bytes retained in memory.
        max_bytes: NumBytes,

        /// Which retained bytes to keep when more output is observed.
        overflow_behavior: CollectionOverflowBehavior,
    },

    /// Retain all observed bytes in memory without a total output cap.
    ///
    /// Use only when the output source and its output volume are trusted.
    TrustedUnbounded,
}

/// Options for collecting parsed output lines into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCollectionOptions {
    /// Retain at most `max_bytes` total line bytes and at most `max_lines` lines in memory.
    Bounded {
        /// Maximum total bytes retained across all collected lines.
        max_bytes: NumBytes,

        /// Maximum number of lines retained in memory.
        max_lines: usize,

        /// Which retained lines to keep when more output is observed.
        overflow_behavior: CollectionOverflowBehavior,
    },

    /// Retain all observed lines in memory without a total output cap.
    ///
    /// Use only when the output source and its output volume are trusted.
    TrustedUnbounded,
}

/// Raw bytes collected from an output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedBytes {
    /// Retained output bytes.
    pub bytes: Vec<u8>,

    /// Whether any bytes were discarded because the configured limit was exceeded.
    pub truncated: bool,
}

impl CollectedBytes {
    /// Creates an empty collected byte buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    pub(crate) fn push_chunk(&mut self, chunk: &[u8], options: RawCollectionOptions) {
        match options {
            RawCollectionOptions::TrustedUnbounded => self.bytes.extend_from_slice(chunk),
            RawCollectionOptions::Bounded {
                max_bytes,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            } => {
                let max_bytes = max_bytes.bytes();
                let remaining = max_bytes.saturating_sub(self.bytes.len());
                if chunk.len() > remaining {
                    self.truncated = true;
                }
                self.bytes
                    .extend_from_slice(&chunk[..remaining.min(chunk.len())]);
            }
            RawCollectionOptions::Bounded {
                max_bytes,
                overflow_behavior: CollectionOverflowBehavior::DropOldestData,
            } => {
                let max_bytes = max_bytes.bytes();
                if chunk.len() > max_bytes {
                    self.bytes.clear();
                    self.bytes
                        .extend_from_slice(&chunk[chunk.len().saturating_sub(max_bytes)..]);
                    self.truncated = true;
                    return;
                }

                let required = self.bytes.len() + chunk.len();
                if required > max_bytes {
                    self.bytes.drain(0..required - max_bytes);
                    self.truncated = true;
                }
                self.bytes.extend_from_slice(chunk);
            }
        }
    }
}

impl Default for CollectedBytes {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for CollectedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

/// Parsed lines collected from an output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedLines {
    lines: VecDeque<String>,
    truncated: bool,
    retained_bytes: usize,
}

impl CollectedLines {
    /// Creates an empty collected line buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            truncated: false,
            retained_bytes: 0,
        }
    }

    /// Retained output lines.
    #[must_use]
    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }

    /// Whether any lines were discarded because the configured limit was exceeded.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Converts this collection into its retained output lines.
    #[must_use]
    pub fn into_lines(self) -> VecDeque<String> {
        self.lines
    }

    /// Converts this collection into its retained output lines and truncation flag.
    #[must_use]
    pub fn into_parts(self) -> (VecDeque<String>, bool) {
        (self.lines, self.truncated)
    }

    pub(crate) fn push_line(&mut self, line: String, options: LineCollectionOptions) {
        match options {
            LineCollectionOptions::TrustedUnbounded => self.push_back(line),
            LineCollectionOptions::Bounded {
                max_bytes,
                max_lines,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            } => {
                let line_len = line.len();
                let max_bytes = max_bytes.bytes();
                if self.lines.len() >= max_lines
                    || line_len > max_bytes
                    || line_len > max_bytes.saturating_sub(self.retained_bytes)
                {
                    self.truncated = true;
                    return;
                }
                self.push_back(line);
            }
            LineCollectionOptions::Bounded {
                max_bytes,
                max_lines,
                overflow_behavior: CollectionOverflowBehavior::DropOldestData,
            } => {
                let line_len = line.len();
                let max_bytes = max_bytes.bytes();
                if max_lines == 0 {
                    self.truncated = true;
                    return;
                }
                if line_len > max_bytes {
                    self.truncated = true;
                    return;
                }

                while self.lines.len() >= max_lines
                    || line_len > max_bytes.saturating_sub(self.retained_bytes)
                {
                    self.pop_front()
                        .expect("line buffer to contain an evictable line");
                    self.truncated = true;
                }
                self.push_back(line);
            }
        }
    }

    fn push_back(&mut self, line: String) {
        self.retained_bytes += line.len();
        self.lines.push_back(line);
    }

    fn pop_front(&mut self) -> Option<String> {
        let line = self.lines.pop_front()?;
        self.retained_bytes -= line.len();
        Some(line)
    }
}

impl Default for CollectedLines {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for CollectedLines {
    type Target = VecDeque<String>;

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

/// Errors that can occur when collecting stream data.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CollectorError {
    /// The collector task could not be joined/terminated.
    #[error("Failed to join/terminate the collector task over stream '{stream_name}': {source}")]
    TaskJoin {
        /// The name of the stream this collector operates on.
        stream_name: &'static str,

        /// The source error.
        #[source]
        source: tokio::task::JoinError,
    },

    /// The underlying stream failed while being read.
    #[error("{source}")]
    StreamRead {
        /// The source error.
        #[source]
        source: StreamReadError,
    },

    /// The sink rejected output written by a writer-backed collector.
    #[error("Failed to write collected output from stream '{stream_name}' to sink: {source}")]
    SinkWrite {
        /// The name of the stream this collector operates on.
        stream_name: &'static str,

        /// The source error.
        #[source]
        source: io::Error,
    },
}

#[derive(Debug)]
pub(crate) enum CollectorTaskError {
    StreamRead(StreamReadError),
    SinkWrite(io::Error),
}

impl From<StreamReadError> for CollectorTaskError {
    fn from(source: StreamReadError) -> Self {
        Self::StreamRead(source)
    }
}

/// A trait for types that can act as sinks for collected stream data.
///
/// This is automatically implemented for any type that is `Send + 'static`.
pub trait Sink: Send + 'static {}

impl<T> Sink for T where T: Send + 'static {}

/// The result of [`Collector::cancel_or_abort_after`].
#[derive(Debug)]
pub enum CollectorCancelOutcome<S: Sink> {
    /// The collector observed cooperative cancellation before the timeout and returned its sink.
    Cancelled(S),

    /// The timeout elapsed, so the collector task was aborted and its sink was dropped.
    Aborted,
}

/// An async collector for raw output chunks.
///
/// The collector itself may hold state via `&mut self`, but only the sink `S` is returned from
/// [`Collector::wait`] or [`Collector::cancel`].
///
/// This trait-based API avoids allocating a boxed future for every collected item while still
/// letting the returned future borrow `chunk` and `sink` across `.await`.
///
/// This uses a trait rather than `std::ops::AsyncFn` because stable Rust can express the lending
/// async callback shape, but cannot yet express the `Send` bound required on an `AsyncFn`
/// callback's returned future for use inside `tokio::spawn`.
pub trait AsyncChunkCollector<S: Sink>: Send + 'static {
    /// Collect a single chunk into `sink`.
    fn collect<'a>(
        &'a mut self,
        chunk: Chunk,
        sink: &'a mut S,
    ) -> impl Future<Output = Next> + Send + 'a;
}

/// An async collector for parsed output lines.
///
/// The collector itself may hold state via `&mut self`, but only the sink `S` is returned from
/// [`Collector::wait`] or [`Collector::cancel`].
///
/// This uses a trait rather than `std::ops::AsyncFn` because stable Rust can express the lending
/// async callback shape, but cannot yet express the `Send` bound required on an `AsyncFn`
/// callback's returned future for use inside `tokio::spawn`. Once that bound is expressible on
/// stable Rust, this API can move back toward async-closure ergonomics.
pub trait AsyncLineCollector<S: Sink>: Send + 'static {
    /// Collect a single parsed line into `sink`.
    fn collect<'a>(
        &'a mut self,
        line: Cow<'a, str>,
        sink: &'a mut S,
    ) -> impl Future<Output = Next> + Send + 'a;
}

/// A collector for stream data, inspecting it chunk by chunk but also providing mutable access
/// to a sink in which the data can be stored.
///
/// See the `collect_*` functions on `BroadcastOutputStream` and `SingleSubscriberOutputStream`.
///
/// For proper cleanup, call
/// - `wait()`, which waits for the collection task to complete.
/// - `cancel()`, which asks the collector to stop and then waits for cooperative completion.
/// - `abort()`, which forcefully aborts the collector task.
/// - `cancel_or_abort_after()`, which tries cooperative cancellation first and aborts on timeout.
///
/// If not cleaned up, the termination signal will be sent when dropping this collector,
/// but the task will be aborted (forceful, not waiting for its regular completion).
pub struct Collector<S: Sink> {
    /// The name of the stream this collector operates on.
    pub(crate) stream_name: &'static str,

    pub(crate) task: Option<JoinHandle<Result<S, CollectorTaskError>>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

pub(crate) struct CollectorWait<S: Sink> {
    stream_name: &'static str,
    guard: CollectorWaitGuard<S>,
}

/// Owns a collector task while [`Collector::wait`] is pending.
///
/// `Collector::wait` consumes the collector and then awaits its task. Without this guard, dropping
/// that wait future after the task handle has been taken would detach the task instead of applying
/// the same cleanup behavior as dropping an unused collector. The guard makes `wait` cancellation
/// safe by signalling termination and aborting the task if the wait future is dropped early.
struct CollectorWaitGuard<S: Sink> {
    task: Option<JoinHandle<Result<S, CollectorTaskError>>>,
    task_termination_sender: Option<Sender<()>>,
}

impl<S: Sink> CollectorWaitGuard<S> {
    fn cancel(&mut self) {
        let _res = self
            .task_termination_sender
            .take()
            .expect("`task_termination_sender` to be present.")
            .send(());
    }

    async fn wait(&mut self, stream_name: &'static str) -> Result<S, CollectorError> {
        let sink = self
            .task
            .as_mut()
            .expect("`task` to be present.")
            .await
            .map_err(|err| CollectorError::TaskJoin {
                stream_name,
                source: err,
            })?
            .map_err(|err| match err {
                CollectorTaskError::StreamRead(source) => CollectorError::StreamRead { source },
                CollectorTaskError::SinkWrite(source) => CollectorError::SinkWrite {
                    stream_name,
                    source,
                },
            });

        self.task = None;
        self.task_termination_sender = None;

        sink
    }

    async fn abort(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            let _res = task_termination_sender.send(());
        }
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(task) = self.task.as_mut() {
            let _res = task.await;
        }
        self.task = None;
    }
}

impl<S: Sink> Drop for CollectorWaitGuard<S> {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            let _res = task_termination_sender.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl<S: Sink> Collector<S> {
    pub(crate) fn into_wait(mut self) -> CollectorWait<S> {
        CollectorWait {
            stream_name: self.stream_name,
            guard: CollectorWaitGuard {
                task: self.task.take(),
                task_termination_sender: self.task_termination_sender.take(),
            },
        }
    }

    /// Returns whether the collector task has finished.
    ///
    /// This is a non-blocking task-state check. A finished collector still owns its task result
    /// until [`wait`](Self::wait), [`cancel`](Self::cancel), [`abort`](Self::abort), or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after) consumes it.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the collector to terminate naturally and returns its sink.
    ///
    /// A collector will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this can hang forever. `wait` also waits for any
    /// in-flight async collector callback or writer call to complete.
    ///
    /// The stdout/stderr streams naturally close when the process is terminated, so `wait`ing
    /// on a collector after termination is fine:
    ///
    /// ```rust, no_run
    /// # async fn test() {
    /// # use std::time::Duration;
    /// # use tokio_process_tools::{
    /// #     AutoName, CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
    /// #     LineCollectionOptions, LineParsingOptions, NumBytesExt, Process,
    /// # };
    ///
    /// # let cmd = tokio::process::Command::new("ls");
    /// let mut process = Process::new(cmd)
    ///     .name(AutoName::program_only())
    ///     .stdout_and_stderr(|stream| {
    ///         stream
    ///             .broadcast()
    ///             .best_effort_delivery()
    ///             .no_replay()
    ///             .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
    ///             .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
    ///     })
    ///     .spawn()
    ///     .unwrap();
    /// let collector = process.stdout().collect_lines_into_vec(
    ///     LineParsingOptions::default(),
    ///     LineCollectionOptions::Bounded {
    ///         max_bytes: 1.megabytes(),
    ///         max_lines: 1024,
    ///         overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    ///     },
    /// );
    /// process.terminate(Duration::from_secs(1), Duration::from_secs(1)).await.unwrap();
    /// let collected = collector.wait().await.unwrap(); // This will return immediately.
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`CollectorError::TaskJoin`] if the collector task cannot be joined,
    /// [`CollectorError::StreamRead`] if the underlying stream fails while being read, or
    /// [`CollectorError::SinkWrite`] if a writer-backed collector stops after a sink write
    /// failure.
    ///
    /// # Panics
    ///
    /// Panics if the collector's internal task has already been taken.
    pub async fn wait(self) -> Result<S, CollectorError> {
        self.into_wait().wait().await
    }

    /// Sends a cooperative cancellation event to the collector and returns its sink.
    ///
    /// Cancellation is observed only between stream events. If an async collector callback or
    /// writer call is already in progress, `cancel` waits for that work to finish before the
    /// collector can observe cancellation. As a result, `cancel` can hang if the in-flight
    /// callback or writer future hangs.
    ///
    /// The sink is preserved and returned only after the collector task exits normally. For
    /// bounded cleanup that can sacrifice sink recovery, use [`abort`](Self::abort) or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after).
    ///
    /// # Errors
    ///
    /// Returns [`CollectorError::TaskJoin`] if the collector task cannot be joined,
    /// [`CollectorError::StreamRead`] if the underlying stream fails while being read before the
    /// cancellation is observed, or [`CollectorError::SinkWrite`] if a writer-backed collector
    /// stops after a sink write failure before cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the collector's internal cancellation sender has already been taken.
    pub async fn cancel(self) -> Result<S, CollectorError> {
        let mut wait = self.into_wait();
        wait.cancel();
        wait.wait().await
    }

    /// Forcefully aborts the collector task.
    ///
    /// This drops any pending async collector callback or writer future, releases the stream
    /// subscription, and drops the sink/writer instead of returning it. It cannot preempt blocking
    /// synchronous code that never yields to the async runtime.
    ///
    /// For single-subscriber streams, the consumer claim is released after the aborted task has
    /// been joined during this method.
    pub async fn abort(self) {
        self.into_wait().abort().await;
    }

    /// Cooperatively cancels the collector, aborting it if `timeout` elapses first.
    ///
    /// Returns [`CollectorCancelOutcome::Cancelled`] with the sink when the collector observes
    /// cancellation and exits normally before the timeout. Returns
    /// [`CollectorCancelOutcome::Aborted`] when the timeout elapses; in that case the task is
    /// aborted, any pending callback/write future is dropped, and the sink/writer is not returned.
    ///
    /// Cancellation is still cooperative until the timeout boundary: an in-flight async callback
    /// or writer call must finish before cancellation can be observed. For single-subscriber
    /// streams, the consumer claim is released before this method returns, both after successful
    /// cooperative cancellation and after timeout-driven abort.
    ///
    /// # Errors
    ///
    /// Returns [`CollectorError::TaskJoin`] if the collector task cannot be joined before the
    /// timeout, [`CollectorError::StreamRead`] if the underlying stream fails while being read
    /// before cancellation is observed, or [`CollectorError::SinkWrite`] if a writer-backed
    /// collector stops after a sink write failure before cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the collector's internal cancellation sender has already been taken.
    pub async fn cancel_or_abort_after(
        self,
        timeout: Duration,
    ) -> Result<CollectorCancelOutcome<S>, CollectorError> {
        let mut wait = self.into_wait();
        wait.cancel();
        match wait.wait_until(Instant::now() + timeout).await? {
            Some(sink) => Ok(CollectorCancelOutcome::Cancelled(sink)),
            None => Ok(CollectorCancelOutcome::Aborted),
        }
    }
}

impl<S: Sink> CollectorWait<S> {
    pub(crate) fn cancel(&mut self) {
        self.guard.cancel();
    }

    pub(crate) async fn wait(&mut self) -> Result<S, CollectorError> {
        self.guard.wait(self.stream_name).await
    }

    pub(crate) async fn wait_until(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<S>, CollectorError> {
        let timeout = sleep_until(deadline);
        tokio::pin!(timeout);

        tokio::select! {
            result = self.wait() => result.map(Some),
            () = &mut timeout => {
                self.abort().await;
                Ok(None)
            }
        }
    }

    pub(crate) async fn abort(&mut self) {
        self.guard.abort().await;
    }
}

impl<S: Sink> Drop for Collector<S> {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // We ignore any potential error here.
            // Sending may fail if the task is already terminated (for example, by reaching EOF),
            // which in turn dropped the receiver end!
            let _res = task_termination_sender.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub(crate) fn collect_chunks<S, T, F>(
    stream_name: &'static str,
    subscription: S,
    into: T,
    collect: F,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    F: FnMut(Chunk, &mut T) + Send + 'static,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = drive_sync(
        subscription,
        CollectChunks {
            sink: into,
            f: collect,
        },
        term_sig_rx,
    );
    Collector {
        stream_name,
        task: Some(tokio::spawn(
            async move { driver.await.map_err(Into::into) },
        )),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_chunks_into_vec<S>(
    stream_name: &'static str,
    subscription: S,
    options: RawCollectionOptions,
) -> Collector<CollectedBytes>
where
    S: Subscription,
{
    collect_chunks(
        stream_name,
        subscription,
        CollectedBytes::new(),
        move |chunk, collected| {
            collected.push_chunk(chunk.as_ref(), options);
        },
    )
}

pub(crate) fn collect_chunks_async<S, T, C>(
    stream_name: &'static str,
    subscription: S,
    into: T,
    collect: C,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    C: AsyncChunkCollector<T>,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = drive_async(
        subscription,
        CollectChunksAsync {
            sink: into,
            collector: collect,
        },
        term_sig_rx,
    );
    Collector {
        stream_name,
        task: Some(tokio::spawn(
            async move { driver.await.map_err(Into::into) },
        )),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines<S, T, F>(
    stream_name: &'static str,
    subscription: S,
    into: T,
    collect: F,
    options: LineParsingOptions,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    F: FnMut(Cow<'_, str>, &mut T) -> Next + Send + 'static,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = drive_sync(
        subscription,
        CollectLines {
            parser: LineParserState::new(),
            options,
            sink: into,
            f: collect,
        },
        term_sig_rx,
    );
    Collector {
        stream_name,
        task: Some(tokio::spawn(
            async move { driver.await.map_err(Into::into) },
        )),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_into_vec<S>(
    stream_name: &'static str,
    subscription: S,
    parsing_options: LineParsingOptions,
    collection_options: LineCollectionOptions,
) -> Collector<CollectedLines>
where
    S: Subscription,
{
    assert!(
        parsing_options.max_line_length.bytes() > 0
            || matches!(collection_options, LineCollectionOptions::TrustedUnbounded),
        "parsing_options.max_line_length must be greater than zero unless line collection is trusted-unbounded"
    );
    collect_lines(
        stream_name,
        subscription,
        CollectedLines::new(),
        move |line, collected| {
            collected.push_line(line.into_owned(), collection_options);
            Next::Continue
        },
        parsing_options,
    )
}

pub(crate) fn collect_lines_async<S, T, C>(
    stream_name: &'static str,
    subscription: S,
    into: T,
    collect: C,
    options: LineParsingOptions,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    C: AsyncLineCollector<T>,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = drive_async(
        subscription,
        CollectLinesAsync {
            parser: LineParserState::new(),
            options,
            sink: into,
            collector: collect,
        },
        term_sig_rx,
    );
    Collector {
        stream_name,
        task: Some(tokio::spawn(
            async move { driver.await.map_err(Into::into) },
        )),
        task_termination_sender: Some(term_sig_tx),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::event_receiver;
    use super::*;
    use crate::CollectorError;
    use crate::output_stream::event::StreamEvent;
    use crate::output_stream::num_bytes::NumBytesExt;
    use crate::{AsyncChunkCollector, AsyncLineCollector};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::borrow::Cow;
    use std::io;
    use tokio::sync::oneshot;

    #[test]
    fn stream_read_display_uses_source_context() {
        let source =
            crate::StreamReadError::new("stdout", io::Error::from(io::ErrorKind::BrokenPipe));
        let expected = source.to_string();
        let err = CollectorError::StreamRead { source };

        assert_that!(err.to_string()).is_equal_to(expected);
    }

    #[tokio::test]
    async fn cancel_or_abort_after_returns_cancelled_when_cooperative() {
        let (task_termination_sender, task_termination_receiver) = oneshot::channel();
        let collector = Collector {
            stream_name: "custom",
            task: Some(tokio::spawn(async move {
                let _res = task_termination_receiver.await;
                Ok(Vec::<u8>::new())
            })),
            task_termination_sender: Some(task_termination_sender),
        };

        let outcome = collector
            .cancel_or_abort_after(Duration::from_secs(1))
            .await
            .unwrap();

        match outcome {
            CollectorCancelOutcome::Cancelled(bytes) => {
                assert_that!(bytes).is_empty();
            }
            CollectorCancelOutcome::Aborted => {
                assert_that!(()).fail("expected cooperative cancellation");
            }
        }
    }

    fn drop_oldest_options(max_bytes: usize, max_lines: usize) -> LineCollectionOptions {
        LineCollectionOptions::Bounded {
            max_bytes: max_bytes.bytes(),
            max_lines,
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        }
    }

    fn assert_retained_bytes_match_lines(collected: &CollectedLines) {
        assert_that!(collected.retained_bytes)
            .is_equal_to(collected.lines.iter().map(String::len).sum::<usize>());
    }

    struct ChunkCase {
        name: &'static str,
        overflow: CollectionOverflowBehavior,
        max_bytes: usize,
        chunks: &'static [&'static [u8]],
        expected_bytes: &'static [u8],
        expected_truncated: bool,
    }

    const CHUNK_BOUNDARY_CASES: &[ChunkCase] = &[
        ChunkCase {
            name: "drop_additional/empty_chunk_is_no_op",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            chunks: &[b""],
            expected_bytes: b"",
            expected_truncated: false,
        },
        ChunkCase {
            name: "drop_additional/single_chunk_exactly_fills_buffer",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            chunks: &[b"abcde"],
            expected_bytes: b"abcde",
            expected_truncated: false,
        },
        ChunkCase {
            name: "drop_additional/single_chunk_overshoots_by_one_byte",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            chunks: &[b"abcdef"],
            expected_bytes: b"abcde",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_additional/second_chunk_straddles_limit",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            chunks: &[b"abc", b"def"],
            expected_bytes: b"abcde",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_additional/first_chunk_exactly_fills_then_second_chunk_rejected",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            chunks: &[b"abcde", b"f"],
            expected_bytes: b"abcde",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_oldest/empty_chunk_is_no_op",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b""],
            expected_bytes: b"",
            expected_truncated: false,
        },
        ChunkCase {
            name: "drop_oldest/single_chunk_exactly_fills_buffer",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"abcde"],
            expected_bytes: b"abcde",
            expected_truncated: false,
        },
        ChunkCase {
            name: "drop_oldest/single_chunk_overshoots_by_one_byte_into_empty",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"abcdef"],
            expected_bytes: b"bcdef",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_oldest/second_chunk_straddles_limit_evicts_front",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"abc", b"def"],
            expected_bytes: b"bcdef",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_oldest/oversized_chunk_into_empty_clears_and_keeps_tail",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"abcdefgh"],
            expected_bytes: b"defgh",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_oldest/oversized_chunk_into_partial_clears_existing",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"ab", b"cdefgh"],
            expected_bytes: b"defgh",
            expected_truncated: true,
        },
        ChunkCase {
            name: "drop_oldest/first_chunk_exactly_fills_then_second_evicts_front",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            chunks: &[b"abcde", b"f"],
            expected_bytes: b"bcdef",
            expected_truncated: true,
        },
    ];

    #[test]
    fn push_chunk_boundary_matrix() {
        for case in CHUNK_BOUNDARY_CASES {
            let mut collected = CollectedBytes::new();
            let options = RawCollectionOptions::Bounded {
                max_bytes: case.max_bytes.bytes(),
                overflow_behavior: case.overflow,
            };
            for chunk in case.chunks {
                collected.push_chunk(chunk, options);
            }

            assert_that!(collected.bytes.as_slice())
                .with_detail_message(format!("case: {}", case.name))
                .is_equal_to(case.expected_bytes);
            assert_that!(collected.truncated)
                .with_detail_message(format!("case: {}", case.name))
                .is_equal_to(case.expected_truncated);
        }
    }

    struct LineCase {
        name: &'static str,
        overflow: CollectionOverflowBehavior,
        max_bytes: usize,
        max_lines: usize,
        push: &'static [&'static str],
        expected_lines: &'static [&'static str],
        expected_truncated: bool,
    }

    const LINE_BOUNDARY_CASES: &[LineCase] = &[
        LineCase {
            name: "drop_additional/line_exactly_fills_byte_budget_with_slot_left",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            max_lines: 2,
            push: &["abcde"],
            expected_lines: &["abcde"],
            expected_truncated: false,
        },
        LineCase {
            name: "drop_additional/max_lines_reached_before_max_bytes",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 100,
            max_lines: 2,
            push: &["a", "b", "c"],
            expected_lines: &["a", "b"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_additional/max_bytes_reached_before_max_lines",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 4,
            max_lines: 10,
            push: &["aa", "bb", "cc"],
            expected_lines: &["aa", "bb"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_additional/line_equal_to_remaining_budget_accepted",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 6,
            max_lines: 10,
            push: &["abc", "def"],
            expected_lines: &["abc", "def"],
            expected_truncated: false,
        },
        LineCase {
            name: "drop_additional/line_one_byte_over_remaining_rejected",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 6,
            max_lines: 10,
            push: &["abc", "defg"],
            expected_lines: &["abc"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_additional/line_strictly_larger_than_max_bytes_rejected",
            overflow: CollectionOverflowBehavior::DropAdditionalData,
            max_bytes: 5,
            max_lines: 10,
            push: &["abc", "xxxxxxxxx"],
            expected_lines: &["abc"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_oldest/line_exactly_fills_byte_budget_with_slot_left",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            max_lines: 2,
            push: &["abcde"],
            expected_lines: &["abcde"],
            expected_truncated: false,
        },
        LineCase {
            name: "drop_oldest/max_lines_reached_before_max_bytes_evicts_one",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 100,
            max_lines: 2,
            push: &["a", "b", "c"],
            expected_lines: &["b", "c"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_oldest/max_bytes_reached_before_max_lines_evicts_one",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 6,
            max_lines: 10,
            push: &["aaa", "bbb", "ccc"],
            expected_lines: &["bbb", "ccc"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_oldest/incoming_line_requires_evicting_multiple_lines",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 8,
            max_lines: 100,
            push: &["a", "b", "cc", "dddd", "eeeeee"],
            expected_lines: &["eeeeee"],
            expected_truncated: true,
        },
        LineCase {
            name: "drop_oldest/line_strictly_larger_than_max_bytes_rejected",
            overflow: CollectionOverflowBehavior::DropOldestData,
            max_bytes: 5,
            max_lines: 10,
            push: &["abc", "xxxxxxxxx"],
            expected_lines: &["abc"],
            expected_truncated: true,
        },
    ];

    #[test]
    fn push_line_boundary_matrix() {
        for case in LINE_BOUNDARY_CASES {
            let mut collected = CollectedLines::new();
            let options = LineCollectionOptions::Bounded {
                max_bytes: case.max_bytes.bytes(),
                max_lines: case.max_lines,
                overflow_behavior: case.overflow,
            };
            for line in case.push {
                collected.push_line((*line).to_string(), options);
            }

            let actual_lines: Vec<&str> = collected.lines().iter().map(String::as_str).collect();
            assert_that!(actual_lines)
                .with_detail_message(format!("case: {}", case.name))
                .is_equal_to(case.expected_lines.to_vec());
            assert_that!(collected.truncated())
                .with_detail_message(format!("case: {}", case.name))
                .is_equal_to(case.expected_truncated);
            assert_that!(collected.retained_bytes)
                .with_detail_message(format!("case: {} (retained_bytes)", case.name))
                .is_equal_to(
                    case.expected_lines
                        .iter()
                        .map(|line| line.len())
                        .sum::<usize>(),
                );
        }
    }

    #[test]
    fn raw_collection_keeps_expected_bytes_when_truncated() {
        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::Bounded {
            max_bytes: 5.bytes(),
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"abcde".as_slice());
        assert_that!(collected.truncated).is_true();

        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::Bounded {
            max_bytes: 5.bytes(),
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        };

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"bcdef".as_slice());
        assert_that!(collected.truncated).is_true();
    }

    #[test]
    fn basic_line_collection_limit_modes() {
        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::Bounded {
            max_bytes: 7.bytes(),
            max_lines: 2,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };

        collected.push_line("one".to_string(), options);
        collected.push_line("two".to_string(), options);
        collected.push_line("three".to_string(), options);

        assert_that!(
            collected
                .lines()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["one", "two"]);
        assert_that!(collected.truncated()).is_true();

        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::Bounded {
            max_bytes: 6.bytes(),
            max_lines: 2,
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        };

        collected.push_line("one".to_string(), options);
        collected.push_line("two".to_string(), options);
        collected.push_line("six".to_string(), options);

        assert_that!(
            collected
                .lines()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["two", "six"]);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn retained_bytes_tracks_appended_lines() {
        let options = LineCollectionOptions::Bounded {
            max_bytes: 100.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);

        assert_that!(collected.retained_bytes).is_equal_to(7);
        assert_retained_bytes_match_lines(&collected);
    }

    #[test]
    fn drop_oldest_preserves_retained_lines_when_oversized_line_arrives() {
        let options = drop_oldest_options(10, 100);
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbb".to_string(), options);
        collected.push_line("x".repeat(13), options);

        assert_that!(collected.lines())
            .with_detail_message(
                "previously-retained lines must survive an oversized incoming line",
            )
            .is_equal_to(VecDeque::from(["aaa".to_string(), "bbb".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(6);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_evicts_old_lines_when_new_line_fits_but_budget_is_exceeded() {
        let options = drop_oldest_options(10, 100);
        let mut collected = CollectedLines::new();

        collected.push_line("aaaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);
        collected.push_line("cccc".to_string(), options);

        assert_that!(collected.lines())
            .is_equal_to(VecDeque::from(["bbbb".to_string(), "cccc".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(8);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_updates_retained_bytes_when_evicting_by_line_count() {
        let options = drop_oldest_options(100, 2);
        let mut collected = CollectedLines::new();

        collected.push_line("a".to_string(), options);
        collected.push_line("bb".to_string(), options);
        collected.push_line("ccc".to_string(), options);

        assert_that!(collected.lines())
            .is_equal_to(VecDeque::from(["bb".to_string(), "ccc".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(5);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_with_zero_max_lines_retains_nothing() {
        let options = drop_oldest_options(100, 0);
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);

        assert_that!(collected.lines().is_empty()).is_true();
        assert_that!(collected.retained_bytes).is_equal_to(0);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_additional_preserves_retained_lines_when_oversized_line_arrives() {
        let options = LineCollectionOptions::Bounded {
            max_bytes: 10.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("x".repeat(13), options);

        assert_that!(collected.lines()).is_equal_to(VecDeque::from(["aaa".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(3);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_additional_preserves_retained_bytes_when_limit_rejects_line() {
        let options = LineCollectionOptions::Bounded {
            max_bytes: 6.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);

        assert_that!(collected.lines()).is_equal_to(VecDeque::from(["aaa".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(3);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[tokio::test]
    async fn collectors_return_stream_read_error() {
        let error =
            crate::StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"complete\npartial"))),
                StreamEvent::ReadError(error),
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        match collector.wait().await {
            Err(CollectorError::StreamRead { source }) => {
                assert_that!(source.stream_name()).is_equal_to("custom");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected collector stream read error, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn collectors_skip_gaps_and_keep_final_unterminated_line() {
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\npar"))),
                StreamEvent::Gap,
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"\ntwo\nfinal"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let lines = collector.wait().await.unwrap();
        assert_that!(lines).contains_exactly(["one", "two", "final"]);
    }

    struct ExtendChunks;

    impl AsyncChunkCollector<Vec<u8>> for ExtendChunks {
        async fn collect<'a>(&'a mut self, chunk: Chunk, seen: &'a mut Vec<u8>) -> Next {
            seen.extend_from_slice(chunk.as_ref());
            Next::Continue
        }
    }

    #[tokio::test]
    async fn chunk_collector_async_extends_sink_until_eof() {
        let collector = collect_chunks_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            ExtendChunks,
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).is_equal_to(b"abcdef".to_vec());
    }

    #[tokio::test]
    async fn chunk_collector_accepts_stateful_callback() {
        let mut chunk_index = 0;
        let collector = collect_chunks(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |chunk, indexed_chunks| {
                chunk_index += 1;
                indexed_chunks.push((chunk_index, chunk.as_ref().to_vec()));
            },
        );

        let indexed_chunks = collector.wait().await.unwrap();
        assert_that!(indexed_chunks).is_equal_to(vec![
            (1, b"ab".to_vec()),
            (2, b"cd".to_vec()),
            (3, b"ef".to_vec()),
        ]);
    }

    #[tokio::test]
    async fn line_collector_accepts_stateful_callback() {
        let mut line_index = 0;
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"alpha\nbeta\ngamma\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |line, indexed_lines| {
                line_index += 1;
                indexed_lines.push(format!("{line_index}:{line}"));
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let indexed_lines = collector.wait().await.unwrap();
        assert_that!(indexed_lines).is_equal_to(vec![
            "1:alpha".to_string(),
            "2:beta".to_string(),
            "3:gamma".to_string(),
        ]);
    }

    struct BreakOnLine;

    impl AsyncLineCollector<Vec<String>> for BreakOnLine {
        async fn collect<'a>(&'a mut self, line: Cow<'a, str>, seen: &'a mut Vec<String>) -> Next {
            if line == "break" {
                seen.push(line.into_owned());
                Next::Break
            } else {
                seen.push(line.into_owned());
                Next::Continue
            }
        }
    }

    #[tokio::test]
    async fn line_collector_async_break_stops_after_requested_line() {
        let collector = collect_lines_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"start\nbreak\nend\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            BreakOnLine,
            LineParsingOptions::default(),
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).contains_exactly(["start", "break"]);
    }
}
