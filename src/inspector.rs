use crate::StreamReadError;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

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

/// An inspector for stream data, inspecting it chunk by chunk.
///
/// See the `inspect_*` functions on `BroadcastOutputStream` and `SingleOutputStream`.
///
/// For proper cleanup, call
/// - `wait()`, which waits for the collection task to complete.
/// - `cancel()`, which sends a termination signal and then waits for the collection task to complete.
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
    /// Checks if this task has finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for the inspector to terminate naturally.
    ///
    /// An inspector will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this could/will hang forever!
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

    /// Sends a cancellation event to the inspector, letting it shut down.
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
