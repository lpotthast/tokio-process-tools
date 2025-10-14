use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

/// Errors that can occur when inspecting stream data.
#[derive(Debug, Error)]
pub enum InspectorError {
    /// The inspector task could not be joined/terminated.
    #[error("The inspector task could not be joined/terminated: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// A collector for stream data, inspecting it chunk by chunk.
///
/// See the `inspect_*` functions on `BroadcastOutputStream` and `SingleOutputStream`.
///
/// For proper cleanup, call
/// - `wait()`, which waits for the collection task to complete.
/// - `cancel()`, which sends a termination signal and then waits for the collection task to complete.
///
/// If not cleaned up, the termination signal will be sent when dropping this collector,
/// but the task will be aborted (forceful, not waiting for its regular completion).
pub struct Inspector {
    pub(crate) task: Option<JoinHandle<()>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

impl Inspector {
    /// Checks if this task has finished.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
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
    pub async fn wait(mut self) -> Result<(), InspectorError> {
        // Take the `task_termination_sender`. Let's make sure nobody can ever interfere with us
        // waiting here. DO NOT drop it, or the task will terminate (at least if it also takes the
        // receive-error as a signal to terminate)!
        let tts = self.task_termination_sender.take();

        let result = self
            .task
            .take()
            .expect("`task` to be present.")
            .await
            .map_err(InspectorError::TaskJoin);

        // Drop the termination sender, we don't need it. Task is now terminated.
        drop(tts);

        result
    }

    /// Sends a cancellation event to the inspector, letting it shut down.
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
