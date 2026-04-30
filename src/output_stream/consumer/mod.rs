//! Tokio runtime adapter. Drives a [`StreamVisitor`](crate::StreamVisitor) over a
//! [`Subscription`](crate::output_stream::Subscription) on a tokio task and exposes the
//! [`Consumer<S>`] handle with cooperative-cancel / abort semantics. Required machinery;
//! tokio-bound by construction. The visitor traits this module drives are runtime-agnostic and
//! live one level up at [`crate::output_stream::visitor`].

pub(crate) mod driver;

pub(crate) use driver::{spawn_consumer_async, spawn_consumer_sync};

use crate::StreamReadError;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Errors that the [`Consumer`] infrastructure itself can raise while driving its stream.
///
/// These describe failures of the consumer task — joining, or reading the underlying stream.
/// Visitor-specific failures (for example, a write-backed visitor's sink rejecting bytes) live
/// in the visitor's own [`StreamVisitor::Output`](crate::StreamVisitor::Output) /
/// [`AsyncStreamVisitor::Output`](crate::AsyncStreamVisitor::Output) type, not here. So a
/// writer-backed consumer's `wait` returns
/// `Result<Result<W, SinkWriteError>, ConsumerError>`: the outer result is what `ConsumerError`
/// describes, the inner is the writer visitor's own outcome.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConsumerError {
    /// The consumer task could not be joined/terminated.
    #[error("Failed to join/terminate the consumer task over stream '{stream_name}': {source}")]
    TaskJoin {
        /// The name of the stream this consumer operates on.
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
}

/// A trait for types that can act as sinks for collected stream data.
///
/// This is automatically implemented for any type that is `Send + 'static`.
pub trait Sink: Send + 'static {}

impl<T> Sink for T where T: Send + 'static {}

/// The result of [`Consumer::cancel_or_abort_after`].
#[derive(Debug)]
pub enum ConsumerCancelOutcome<S: Sink> {
    /// The consumer observed cooperative cancellation before the timeout and returned its sink.
    Cancelled(S),

    /// The timeout elapsed, so the consumer task was aborted and its sink was dropped.
    Aborted,
}

/// A handle for a tokio task that consumes a stream by driving a visitor over its events.
///
/// Consumers are produced by the `inspect_*`, `collect_*`, and `wait_for_line` factory methods on
/// [`BroadcastOutputStream`](crate::BroadcastOutputStream) and
/// [`SingleSubscriberOutputStream`](crate::SingleSubscriberOutputStream). The type parameter `S`
/// is the visitor's output (a sink, a writer, `()`, or another value the visitor returns when the
/// stream ends).
///
/// For proper cleanup, call
/// - `wait()`, which waits for the consumer task to complete.
/// - `cancel()`, which asks the consumer to stop and then waits for cooperative completion.
/// - `abort()`, which forcefully aborts the consumer task.
/// - `cancel_or_abort_after()`, which tries cooperative cancellation first and aborts on timeout.
///
/// If not cleaned up, the termination signal will be sent when dropping this consumer,
/// but the task will be aborted (forceful, not waiting for its regular completion).
pub struct Consumer<S: Sink> {
    /// The name of the stream this consumer operates on.
    pub(crate) stream_name: &'static str,

    pub(crate) task: Option<JoinHandle<Result<S, StreamReadError>>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

pub(crate) struct ConsumerWait<S: Sink> {
    stream_name: &'static str,
    guard: ConsumerWaitGuard<S>,
}

/// Owns a consumer task while [`Consumer::wait`] is pending.
///
/// `Consumer::wait` consumes the [`Consumer`] and then awaits its task. Without this guard,
/// dropping that wait future after the task handle has been taken would detach the task instead
/// of applying the same cleanup behavior as dropping an unused [`Consumer`]. The guard makes
/// `wait` cancellation safe by signalling termination and aborting the task if the wait future is
/// dropped early.
struct ConsumerWaitGuard<S: Sink> {
    task: Option<JoinHandle<Result<S, StreamReadError>>>,
    task_termination_sender: Option<Sender<()>>,
}

impl<S: Sink> ConsumerWaitGuard<S> {
    fn cancel(&mut self) {
        let _res = self
            .task_termination_sender
            .take()
            .expect("`task_termination_sender` to be present.")
            .send(());
    }

    async fn wait(&mut self, stream_name: &'static str) -> Result<S, ConsumerError> {
        let sink = self
            .task
            .as_mut()
            .expect("`task` to be present.")
            .await
            .map_err(|err| ConsumerError::TaskJoin {
                stream_name,
                source: err,
            })?
            .map_err(|source| ConsumerError::StreamRead { source });

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

impl<S: Sink> Drop for ConsumerWaitGuard<S> {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            let _res = task_termination_sender.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl<S: Sink> Consumer<S> {
    pub(crate) fn into_wait(mut self) -> ConsumerWait<S> {
        ConsumerWait {
            stream_name: self.stream_name,
            guard: ConsumerWaitGuard {
                task: self.task.take(),
                task_termination_sender: self.task_termination_sender.take(),
            },
        }
    }

    /// Returns whether the consumer task has finished.
    ///
    /// This is a non-blocking task-state check. A finished consumer still owns its task result
    /// until [`wait`](Self::wait), [`cancel`](Self::cancel), [`abort`](Self::abort), or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after) consumes it.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the consumer to terminate naturally and returns its sink.
    ///
    /// A consumer will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this can hang forever. `wait` also waits for any
    /// in-flight async visitor callback or writer call to complete.
    ///
    /// The stdout/stderr streams naturally close when the process is terminated, so `wait`ing
    /// on a consumer after termination is fine:
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
    /// let consumer = process.stdout().collect_lines_into_vec(
    ///     LineParsingOptions::default(),
    ///     LineCollectionOptions::Bounded {
    ///         max_bytes: 1.megabytes(),
    ///         max_lines: 1024,
    ///         overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    ///     },
    /// );
    /// process.terminate(Duration::from_secs(1), Duration::from_secs(1)).await.unwrap();
    /// let collected = consumer.wait().await.unwrap(); // This will return immediately.
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ConsumerError::TaskJoin`] if the consumer task cannot be joined, or
    /// [`ConsumerError::StreamRead`] if the underlying stream fails while being read.
    /// Visitor-specific outcomes (e.g. a writer-backed visitor's sink failure) appear inside
    /// the returned `S`, not in [`ConsumerError`].
    ///
    /// # Panics
    ///
    /// Panics if the consumer's internal task has already been taken.
    pub async fn wait(self) -> Result<S, ConsumerError> {
        self.into_wait().wait().await
    }

    /// Sends a cooperative cancellation event to the consumer and returns its sink.
    ///
    /// Cancellation is observed only between stream events. If an async visitor callback or
    /// writer call is already in progress, `cancel` waits for that work to finish before the
    /// consumer can observe cancellation. As a result, `cancel` can hang if the in-flight
    /// callback or writer future hangs.
    ///
    /// The sink is preserved and returned only after the consumer task exits normally. For
    /// bounded cleanup that can sacrifice sink recovery, use [`abort`](Self::abort) or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after).
    ///
    /// # Errors
    ///
    /// Returns [`ConsumerError::TaskJoin`] if the consumer task cannot be joined, or
    /// [`ConsumerError::StreamRead`] if the underlying stream fails while being read before the
    /// cancellation is observed. Visitor-specific outcomes appear inside the returned `S`.
    ///
    /// # Panics
    ///
    /// Panics if the consumer's internal cancellation sender has already been taken.
    pub async fn cancel(self) -> Result<S, ConsumerError> {
        let mut wait = self.into_wait();
        wait.cancel();
        wait.wait().await
    }

    /// Forcefully aborts the consumer task.
    ///
    /// This drops any pending async visitor callback or writer future, releases the stream
    /// subscription, and drops the sink/writer instead of returning it. It cannot preempt blocking
    /// synchronous code that never yields to the async runtime.
    ///
    /// For single-subscriber streams, the consumer claim is released after the aborted task has
    /// been joined during this method.
    pub async fn abort(self) {
        self.into_wait().abort().await;
    }

    /// Cooperatively cancels the consumer, aborting it if `timeout` elapses first.
    ///
    /// Returns [`ConsumerCancelOutcome::Cancelled`] with the sink when the consumer observes
    /// cancellation and exits normally before the timeout. Returns
    /// [`ConsumerCancelOutcome::Aborted`] when the timeout elapses; in that case the task is
    /// aborted, any pending callback/write future is dropped, and the sink/writer is not returned.
    ///
    /// Cancellation is still cooperative until the timeout boundary: an in-flight async callback
    /// or writer call must finish before cancellation can be observed. For single-subscriber
    /// streams, the consumer claim is released before this method returns, both after successful
    /// cooperative cancellation and after timeout-driven abort.
    ///
    /// # Errors
    ///
    /// Returns [`ConsumerError::TaskJoin`] if the consumer task cannot be joined before the
    /// timeout, or [`ConsumerError::StreamRead`] if the underlying stream fails while being read
    /// before cancellation is observed. Visitor-specific outcomes appear inside the returned
    /// `S` (carried by [`ConsumerCancelOutcome::Cancelled`]).
    ///
    /// # Panics
    ///
    /// Panics if the consumer's internal cancellation sender has already been taken.
    pub async fn cancel_or_abort_after(
        self,
        timeout: Duration,
    ) -> Result<ConsumerCancelOutcome<S>, ConsumerError> {
        let mut wait = self.into_wait();
        wait.cancel();
        match wait.wait_until(Instant::now() + timeout).await? {
            Some(sink) => Ok(ConsumerCancelOutcome::Cancelled(sink)),
            None => Ok(ConsumerCancelOutcome::Aborted),
        }
    }
}

impl<S: Sink> ConsumerWait<S> {
    pub(crate) fn cancel(&mut self) {
        self.guard.cancel();
    }

    pub(crate) async fn wait(&mut self) -> Result<S, ConsumerError> {
        self.guard.wait(self.stream_name).await
    }

    pub(crate) async fn wait_until(
        &mut self,
        deadline: Instant,
    ) -> Result<Option<S>, ConsumerError> {
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

impl<S: Sink> Drop for Consumer<S> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;
    use std::io;
    use tokio::sync::oneshot;

    #[test]
    fn stream_read_display_uses_source_context() {
        let source = StreamReadError::new("stdout", io::Error::from(io::ErrorKind::BrokenPipe));
        let expected = source.to_string();
        let err = ConsumerError::StreamRead { source };

        assert_that!(err.to_string()).is_equal_to(expected);
    }

    #[tokio::test]
    async fn cancel_or_abort_after_returns_cancelled_when_cooperative() {
        let (task_termination_sender, task_termination_receiver) = oneshot::channel();
        let consumer = Consumer {
            stream_name: "custom",
            task: Some(tokio::spawn(async move {
                let _res = task_termination_receiver.await;
                Ok(Vec::<u8>::new())
            })),
            task_termination_sender: Some(task_termination_sender),
        };

        let outcome = consumer
            .cancel_or_abort_after(Duration::from_secs(1))
            .await
            .unwrap();

        match outcome {
            ConsumerCancelOutcome::Cancelled(bytes) => {
                assert_that!(bytes).is_empty();
            }
            ConsumerCancelOutcome::Aborted => {
                assert_that!(()).fail("expected cooperative cancellation");
            }
        }
    }
}
