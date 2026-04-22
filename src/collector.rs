use crate::StreamReadError;
use crate::output_stream::{Chunk, Next};
use std::borrow::Cow;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

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
/// This is automatically implemented for any type that is `Debug + Send + Sync + 'static`.
pub trait Sink: Debug + Send + Sync + 'static {}

impl<T> Sink for T where T: Debug + Send + Sync + 'static {}

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

impl<S: Sink> Collector<S> {
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
    /// #     CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
    /// #     LineCollectionOptions, LineParsingOptions, NumBytesExt, Process,
    /// # };
    ///
    /// # let cmd = tokio::process::Command::new("ls");
    /// let mut process = Process::new(cmd)
    ///     .auto_name()
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
    pub async fn wait(mut self) -> Result<S, CollectorError> {
        // Take the `task_termination_sender`. Let's make sure nobody can ever interfere with us
        // waiting here. DO NOT drop it, or the task will terminate (at least if it also takes the
        // receive-error as a signal to terminate)!
        let tts = self.task_termination_sender.take();

        let sink = self
            .task
            .take()
            .expect("`task` to be present.")
            .await
            .map_err(|err| CollectorError::TaskJoin {
                stream_name: self.stream_name,
                source: err,
            })?
            .map_err(|err| match err {
                CollectorTaskError::StreamRead(source) => CollectorError::StreamRead {
                    stream_name: self.stream_name,
                    source,
                },
                CollectorTaskError::SinkWrite(source) => CollectorError::SinkWrite {
                    stream_name: self.stream_name,
                    source,
                },
            });

        // Drop the termination sender, we don't need it. Task is now terminated.
        drop(tts);

        sink
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
