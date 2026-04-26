use crate::StreamReadError;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Errors that can occur when inspecting stream data.
#[derive(Debug, Error)]
pub enum InspectorError {
    /// The inspector task could not be joined/terminated.
    #[error("Failed to join/terminate the inspector task over stream '{stream_name}': {source}")]
    TaskJoin {
        /// The name of the stream this inspector operates on.
        stream_name: &'static str,

        /// The source error.
        #[source]
        source: tokio::task::JoinError,
    },

    /// The underlying stream failed while being read.
    #[error("Failed to read from stream '{stream_name}': {source}")]
    StreamRead {
        /// The name of the stream this inspector operates on.
        stream_name: &'static str,

        /// The source error.
        #[source]
        source: StreamReadError,
    },
}

/// The result of [`Inspector::cancel_or_abort_after`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorCancelOutcome {
    /// The inspector observed cooperative cancellation before the timeout.
    Cancelled,

    /// The timeout elapsed, so the inspector task was aborted.
    Aborted,
}

/// An inspector for stream data, inspecting it chunk by chunk.
///
/// See the `inspect_*` functions on `BroadcastOutputStream` and `SingleOutputStream`.
///
/// For proper cleanup, call
/// - `wait()`, which waits for the collection task to complete.
/// - `cancel()`, which asks the inspector to stop and then waits for cooperative completion.
/// - `abort()`, which forcefully aborts the inspector task.
/// - `cancel_or_abort_after()`, which tries cooperative cancellation first and aborts on timeout.
///
/// If not cleaned up, the termination signal will be sent when dropping this inspector,
/// but the task will be aborted (forceful, not waiting for its regular completion).
pub struct Inspector {
    /// The name of the stream this inspector operates on.
    pub(crate) stream_name: &'static str,

    pub(crate) task: Option<JoinHandle<Result<(), StreamReadError>>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

/// Owns an inspector task while [`Inspector::wait`] is pending.
///
/// `Inspector::wait` consumes the inspector and then awaits its task. Without this guard, dropping
/// that wait future after the task handle has been taken would detach the task instead of applying
/// the same cleanup behavior as dropping an unused inspector. The guard makes `wait` cancellation
/// safe by signalling termination and aborting the task if the wait future is dropped early.
struct InspectorWaitGuard {
    task: Option<JoinHandle<Result<(), StreamReadError>>>,
    task_termination_sender: Option<Sender<()>>,
}

impl InspectorWaitGuard {
    fn cancel(&mut self) {
        let _res = self
            .task_termination_sender
            .take()
            .expect("`task_termination_sender` to be present.")
            .send(());
    }

    async fn wait(&mut self, stream_name: &'static str) -> Result<(), InspectorError> {
        self.task
            .as_mut()
            .expect("`task` to be present.")
            .await
            .map_err(|err| InspectorError::TaskJoin {
                stream_name,
                source: err,
            })?
            .map_err(|err| InspectorError::StreamRead {
                stream_name,
                source: err,
            })?;

        self.task = None;
        self.task_termination_sender = None;

        Ok(())
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

impl Drop for InspectorWaitGuard {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            let _res = task_termination_sender.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Inspector {
    /// Returns whether the inspector task has finished.
    ///
    /// This is a non-blocking task-state check. A finished inspector still owns its task result
    /// until [`wait`](Self::wait), [`cancel`](Self::cancel), [`abort`](Self::abort), or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after) consumes it.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the inspector to terminate naturally.
    ///
    /// An inspector will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this can hang forever. `wait` also waits for any
    /// in-flight async inspector callback to complete.
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined, or
    /// [`InspectorError::StreamRead`] if the underlying stream fails while being read.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal task has already been taken.
    pub async fn wait(mut self) -> Result<(), InspectorError> {
        let mut guard = InspectorWaitGuard {
            task: self.task.take(),
            task_termination_sender: self.task_termination_sender.take(),
        };

        guard.wait(self.stream_name).await
    }

    /// Sends a cooperative cancellation event to the inspector and waits for it to stop.
    ///
    /// Cancellation is observed only between stream events. If an async inspector callback is
    /// already in progress, `cancel` waits for that callback to finish before the inspector can
    /// observe cancellation. As a result, `cancel` can hang if the in-flight callback future
    /// hangs.
    ///
    /// For bounded cleanup of a potentially stuck inspector, use [`abort`](Self::abort) or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after).
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined, or
    /// [`InspectorError::StreamRead`] if the underlying stream fails while being read before the
    /// cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal cancellation sender has already been taken.
    pub async fn cancel(mut self) -> Result<(), InspectorError> {
        let mut guard = InspectorWaitGuard {
            task: self.task.take(),
            task_termination_sender: self.task_termination_sender.take(),
        };

        guard.cancel();
        guard.wait(self.stream_name).await
    }

    /// Forcefully aborts the inspector task.
    ///
    /// This drops any pending async inspector callback future and releases the stream
    /// subscription. It cannot preempt blocking synchronous code that never yields to the async
    /// runtime.
    ///
    /// For single-subscriber streams, the consumer claim is released after the aborted task has
    /// been joined during this method.
    pub async fn abort(mut self) {
        let mut guard = InspectorWaitGuard {
            task: self.task.take(),
            task_termination_sender: self.task_termination_sender.take(),
        };

        guard.abort().await;
    }

    /// Cooperatively cancels the inspector, aborting it if `timeout` elapses first.
    ///
    /// Returns [`InspectorCancelOutcome::Cancelled`] when the inspector observes cancellation and
    /// exits normally before the timeout. Returns [`InspectorCancelOutcome::Aborted`] when the
    /// timeout elapses; in that case the task is aborted and any pending callback future is
    /// dropped.
    ///
    /// Cancellation is still cooperative until the timeout boundary: an in-flight async callback
    /// must finish before cancellation can be observed. For single-subscriber streams, the
    /// consumer claim is released before this method returns, both after successful cooperative
    /// cancellation and after timeout-driven abort.
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined before the
    /// timeout, or [`InspectorError::StreamRead`] if the underlying stream fails while being read
    /// before cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal cancellation sender has already been taken.
    pub async fn cancel_or_abort_after(
        mut self,
        timeout: Duration,
    ) -> Result<InspectorCancelOutcome, InspectorError> {
        let mut guard = InspectorWaitGuard {
            task: self.task.take(),
            task_termination_sender: self.task_termination_sender.take(),
        };

        guard.cancel();
        let timeout = sleep_until(Instant::now() + timeout);
        tokio::pin!(timeout);

        tokio::select! {
            result = guard.wait(self.stream_name) => {
                result?;
                Ok(InspectorCancelOutcome::Cancelled)
            }
            () = &mut timeout => {
                guard.abort().await;
                Ok(InspectorCancelOutcome::Aborted)
            }
        }
    }
}

impl Drop for Inspector {
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
