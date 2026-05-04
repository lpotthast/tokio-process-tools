use crate::process_handle::ProcessHandle;
use crate::process_handle::termination::GracefulTimeouts;
use crate::{OutputStream, async_drop};
use std::ops::{Deref, DerefMut};

/// A wrapper that automatically terminates a process when dropped.
///
/// # Safety Requirements
///
/// **WARNING**: This type requires a multithreaded tokio runtime to function correctly!
///
/// # Usage Guidelines
///
/// This type should only be used when:
/// - Your code is running in a multithreaded tokio runtime.
/// - Automatic process cleanup on drop is absolutely necessary.
///
/// # Recommended Alternatives
///
/// Instead of relying on automatic termination, prefer these safer approaches:
/// 1. Manual process termination using [`ProcessHandle::terminate`]
/// 2. Awaiting process completion using [`ProcessHandle::wait_for_completion`]
/// 3. Awaiting process completion or performing an explicit termination using
///    [`ProcessHandle::wait_for_completion_or_terminate`]
///
/// # Implementation Details
///
/// The drop implementation tries to terminate the process if it was neither awaited nor
/// terminated before being dropped. If checking the current process state fails, it still attempts
/// best-effort termination. If termination fails, a panic is raised.
#[derive(Debug)]
pub struct TerminateOnDrop<Stdout: OutputStream, Stderr: OutputStream = Stdout> {
    pub(crate) process_handle: ProcessHandle<Stdout, Stderr>,
    pub(crate) timeouts: GracefulTimeouts,
}

impl<Stdout, Stderr> Deref for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    type Target = ProcessHandle<Stdout, Stderr>;

    fn deref(&self) -> &Self::Target {
        &self.process_handle
    }
}

impl<Stdout, Stderr> DerefMut for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process_handle
    }
}

impl<Stdout, Stderr> Drop for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn drop(&mut self) {
        async_drop::run_future(async {
            match self.process_handle.is_running() {
                crate::RunningState::Terminated(_) => {
                    tracing::debug!(
                        process = %self.process_handle.name,
                        "Process already terminated"
                    );
                    // The child has exited; close the lifecycle so the inner ProcessHandle's
                    // drop guards do not panic when this wrapper drops out of scope.
                    self.process_handle.must_not_be_terminated();
                    return;
                }
                crate::RunningState::Running => {}
                crate::RunningState::Uncertain(err) => {
                    tracing::warn!(
                        process = %self.process_handle.name,
                        error = %err,
                        "Could not determine process state during drop; attempting best-effort termination"
                    );
                }
            }

            tracing::debug!(process = %self.process_handle.name, "Terminating process");
            match self.process_handle.terminate(self.timeouts).await {
                Ok(exit_status) => {
                    tracing::debug!(
                        process = %self.process_handle.name,
                        ?exit_status,
                        "Successfully terminated process"
                    );
                }
                Err(err) => {
                    tracing::error!(
                        process = %self.process_handle.name,
                        error = %err,
                        "Failed to terminate process during drop"
                    );
                }
            }
        });
    }
}
