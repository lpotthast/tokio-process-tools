use crate::StreamReadError;
use crate::output_stream::{Chunk, Next};
use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Errors that can occur when collecting stream data.
#[derive(Debug, Error)]
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
    #[error("Failed to read from stream '{stream_name}': {source}")]
    StreamRead {
        /// The name of the stream this collector operates on.
        stream_name: &'static str,

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
/// See the `collect_*` functions on `BroadcastOutputStream` and `SingleOutputStream`.
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
                CollectorTaskError::StreamRead(source) => CollectorError::StreamRead {
                    stream_name,
                    source,
                },
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
