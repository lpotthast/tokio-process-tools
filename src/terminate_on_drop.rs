use crate::process_handle::ProcessHandle;
use crate::OutputStream;
use std::ops::Deref;
use std::time::Duration;

/// A wrapper that automatically terminates a process when dropped.
///
/// # Safety Requirements
///
/// **WARNING**: This type requires a multithreaded tokio runtime to function correctly!
///
/// # Usage Guidelines
///
/// This type should only be used when:
/// - Your code is running in a multithreaded tokio runtime
/// - Automatic process cleanup on drop is absolutely necessary
///
/// # Recommended Alternatives
///
/// Instead of relying on automatic termination, prefer these safer approaches:
/// 1. Manual process termination using [`ProcessHandle::terminate`]
/// 2. Awaiting the process completion using [`ProcessHandle::wait`]
///
/// # Implementation Details
///
/// The drop implementation try to terminate the process if it was neither awaited nor
/// terminated before being dropped. If termination fails, a panic is raised.
#[derive(Debug)]
pub struct TerminateOnDrop<O: OutputStream> {
    pub(crate) process_handle: ProcessHandle<O>,
    pub(crate) interrupt_timeout: Duration,
    pub(crate) terminate_timeout: Duration,
}

impl<O: OutputStream> Deref for TerminateOnDrop<O> {
    type Target = ProcessHandle<O>;

    fn deref(&self) -> &Self::Target {
        &self.process_handle
    }
}

impl<O: OutputStream> Drop for TerminateOnDrop<O> {
    fn drop(&mut self) {
        // 1. We are in a Drop implementation which is synchronous - it can't be async.
        // But we need to execute an async operation (the `terminate` call).
        //
        // 2. `Block_on` is needed because it takes an async operation and runs it to completion
        // synchronously - it's how we can execute our async terminate call within the synchronous
        // drop.
        //
        // 3. However, block_on by itself isn't safe to call from within an async context
        // (which we are in since we're inside the Tokio runtime).
        // This is because it could lead to deadlocks - imagine if the current thread is needed to
        // process some task that our blocked async operation is waiting on.
        //
        // 4. This is where block_in_place comes in - it tells Tokio:
        // "hey, I'm about to block this thread, please make sure other threads are available to
        // still process tasks". It essentially moves the blocking work to a dedicated thread pool
        // so that the async runtime can continue functioning.
        //
        // 5. Note that `block_in_place` requires a multithreaded tokio runtime to be active!
        // So use `#[tokio::test(flavor = "multi_thread")]` in tokio-enabled tests.
        //
        // 6. Also note that `block_in_place` enforces that the given closure runs to completion,
        // even when the async executor is terminated - this might be because our program ended
        // or because we crashed due to a panic.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                if !self.process_handle.is_running().as_bool() {
                    tracing::debug!(
                        process = %self.process_handle.name,
                        "Process already terminated"
                    );
                    return;
                }

                tracing::debug!(process = %self.process_handle.name, "Terminating process");
                match self
                    .process_handle
                    .terminate(self.interrupt_timeout, self.terminate_timeout)
                    .await
                {
                    Ok(exit_status) => {
                        tracing::debug!(
                            process = %self.process_handle.name,
                            ?exit_status,
                            "Successfully terminated process"
                        )
                    }
                    Err(err) => {
                        panic!(
                            "Failed to terminate process '{}': {}",
                            self.process_handle.name, err
                        );
                    }
                };
            });
        });
    }
}
