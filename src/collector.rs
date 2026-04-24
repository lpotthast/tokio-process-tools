use crate::StreamReadError;
use crate::output_stream::{Chunk, Next};
use std::borrow::Cow;
use std::future::Future;
use std::io;
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
/// - `cancel()`, which sends a termination signal and then waits for the collection task to complete.
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

    /// Checks if this task has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for the collector to terminate naturally.
    ///
    /// A collector will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this could/will hang forever!
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
    ///     LineCollectionOptions::builder()
    ///         .max_bytes(1.megabytes())
    ///         .max_lines(1024)
    ///         .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
    ///         .build(),
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

    /// Sends a cancellation event to the collector, letting it shut down.
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
    pub async fn cancel(mut self) -> Result<S, CollectorError> {
        // We ignore any potential error here.
        // Sending may fail if the task is already terminated (for example, by reaching EOF),
        // which in turn dropped the receiver end!
        let _res = self
            .task_termination_sender
            .take()
            .expect("`task_termination_sender` to be present.")
            .send(());

        self.wait().await
    }
}

impl<S: Sink> CollectorWait<S> {
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
