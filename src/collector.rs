use crate::output_stream::Next;
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

// NOTE: We use Pin<Box> here to force usage of Higher-Rank Trait Bounds (HRTBs).
// The returned futures will most-likely capture the `&mut T`and are therefore poised
// by its lifetime. Without the trait-object usage, this would not work.
pub type AsyncCollectFn<'a> = Pin<Box<dyn Future<Output = Next> + Send + 'a>>;

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
    pub(crate) task: Option<JoinHandle<S>>,
    pub(crate) task_termination_sender: Option<Sender<()>>,
}

impl<S: Sink> Collector<S> {
    /// Checks if this task has finished.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().map(|t| t.is_finished()).unwrap_or(true)
    }

    /// Wait for the collector to terminate naturally.
    ///
    /// A collector will automatically terminate when either:
    ///
    /// 1. The underlying stream (write-side) is closed.
    /// 2. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this could/will hang forever!
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
            .map_err(CollectorError::TaskJoin);

        // Drop the termination sender, we don't need it. Task is now terminated.
        drop(tts);

        sink
    }

    /// Sends a cancellation event to the collector, letting it shut down.
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
