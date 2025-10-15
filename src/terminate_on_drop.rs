use crate::process_handle::ProcessHandle;
use crate::{OutputStream, async_drop};
use std::ops::{Deref, DerefMut};
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

impl<O: OutputStream> DerefMut for TerminateOnDrop<O> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process_handle
    }
}

impl<O: OutputStream> Drop for TerminateOnDrop<O> {
    fn drop(&mut self) {
        async_drop::run_future(async {
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
    }
}
