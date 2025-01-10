use std::error::Error;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;
use tokio::sync::oneshot::Sender;
use tokio::task::JoinHandle;

#[derive(Debug, Error)]
pub enum CollectorError {
    #[error("The collector task could not be joined/terminated: {0}")]
    TaskJoin(#[source] tokio::task::JoinError),
}

pub trait Sink: Debug + Send + Sync + 'static {}

impl<T> Sink for T where T: Debug + Send + Sync + 'static {}

pub type CollectError = Box<dyn Error + Send + Sync>;

pub type AsyncCollectFn<'a> = Pin<Box<dyn Future<Output = Result<(), CollectError>> + Send + 'a>>;

/// A collector for output lines.
///
/// For proper cleanup, call `abort()` which gracefully waits for the collecting task to complete.
/// It should terminate fast, as an internal termination signal is send to it.
/// If dropped without calling `abort()`, the termination will be sent as well,
/// but the task will be aborted (forceful, not waiting for completion).
pub struct Collector<T: Sink> {
    pub(crate) task: Option<JoinHandle<T>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

impl<T: Sink> Collector<T> {
    pub async fn abort(mut self) -> Result<T, CollectorError> {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // Safety: This `expect` call SHOULD neve fail. The receiver lives in the tokio task,
            // and is only dropped after receiving the termination signal.
            // The task is only awaited-and-dropped after THIS send and only ONCE, gated by taking
            // it out the `Option`, which can only be done once.
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to collector task."
                );
            };
        }
        if let Some(task) = self.task.take() {
            return task.await.map_err(CollectorError::TaskJoin);
        }
        unreachable!("The collector task was already aborted");
    }
}

impl<T: Sink> Drop for Collector<T> {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to collector task."
                );
            }
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
