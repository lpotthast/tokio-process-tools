use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum InspectorError {
    #[error("The inspector task could not be joined/terminated: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// An inspector for output lines.
///
/// For proper cleanup, call `abort()` which gracefully waits for the collecting task to complete.
/// It should terminate fast, as an internal termination signal is send to it.
/// If dropped without calling `abort()`, the termination will be sent as well,
/// but the task will be aborted (forceful, not waiting for completion).
pub struct Inspector {
    pub(crate) task: Option<JoinHandle<()>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

impl Inspector {
    pub async fn abort(mut self) -> Result<(), InspectorError> {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // Safety: In normal (non-panic-) scenarios, this call SHOULD never fail.
            // The receiver lives in the tokio task, and is only dropped after once receiving
            // the termination signal.
            // The task is only awaited-and-dropped after THIS send and only ONCE, gated by taking
            // it out the `Option`, which can only be done once.
            // It might fail when a panic occurs.
            if let Err(()) = task_termination_sender.send(()) {
                tracing::trace!(
                    "Unexpected failure when sending termination signal to inspector task."
                );
            };
        }
        if let Some(task) = self.task.take() {
            return task.await.map_err(InspectorError::TaskJoin);
        }
        unreachable!("The inspector task was already aborted");
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // Safety: In normal (non-panic-) scenarios, this call SHOULD never fail.
            // The receiver lives in the tokio task, and is only dropped after once receiving
            // the termination signal.
            // The task is only awaited-and-dropped after THIS send and only ONCE, gated by taking
            // it out the `Option`, which can only be done once.
            // It might fail when a panic occurs.
            if let Err(()) = task_termination_sender.send(()) {
                tracing::trace!(
                    "Unexpected failure when sending termination signal to inspector task."
                );
            }
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
