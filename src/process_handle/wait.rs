use super::ProcessHandle;
#[cfg(any(unix, windows))]
use super::termination::GracefulShutdown;
use crate::error::{WaitError, WaitForCompletionResult};
#[cfg(any(unix, windows))]
use crate::error::{WaitForCompletionOrTerminateResult, WaitOrTerminateError};
use crate::output_stream::OutputStream;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Successfully awaiting the completion of the process will unset the
    /// "must be terminated" setting, as a successfully awaited process is already terminated.
    /// Dropping this `ProcessHandle` after successfully calling `wait` should never lead to a
    /// "must be terminated" panic being raised.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.stdin().close();
        match self.group.wait().await {
            Ok(status) => {
                self.must_not_be_terminated();
                Ok(status)
            }
            Err(err) => Err(err),
        }
    }

    /// Wait for the process to run to completion within `timeout`. Used internally by the staged
    /// [`crate::WaitForCompletion`] builder and by other in-crate orchestration paths.
    ///
    /// Stdin is closed before the wait begins, matching [`tokio::process::Child::wait`]. If the
    /// timeout elapses before exit, the process keeps running and a `Timeout` outcome is returned.
    pub(super) async fn wait_for_completion_inner(
        &mut self,
        timeout: Duration,
    ) -> Result<WaitForCompletionResult, WaitError> {
        match tokio::time::timeout(timeout, self.wait()).await {
            Ok(Ok(exit_status)) => Ok(WaitForCompletionResult::Completed(exit_status)),
            Ok(Err(source)) => Err(WaitError::IoError {
                process_name: self.name.clone(),
                source,
            }),
            Err(_elapsed) => Ok(WaitForCompletionResult::Timeout { timeout }),
        }
    }

    pub(super) async fn wait_for_completion_unbounded(&mut self) -> Result<ExitStatus, WaitError> {
        self.wait().await.map_err(|source| WaitError::IoError {
            process_name: self.name.clone(),
            source,
        })
    }

    pub(super) async fn wait_for_exit_after_signal(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ExitStatus>, WaitError> {
        match self.wait_for_completion_inner(timeout).await? {
            WaitForCompletionResult::Completed(exit_status) => Ok(Some(exit_status)),
            WaitForCompletionResult::Timeout { .. } => Ok(None),
        }
    }
}

#[cfg(any(unix, windows))]
impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    pub(super) async fn wait_for_completion_or_terminate_inner(
        &mut self,
        wait_timeout: Duration,
        shutdown: GracefulShutdown,
    ) -> Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError> {
        match self.wait_for_completion_inner(wait_timeout).await {
            Ok(WaitForCompletionResult::Completed(exit_status)) => {
                Ok(WaitForCompletionOrTerminateResult::Completed(exit_status))
            }
            Ok(WaitForCompletionResult::Timeout { timeout }) => {
                self.terminate_after_wait_timeout(timeout, shutdown).await
            }
            Err(wait_error) => self.terminate_after_wait_error(wait_error, shutdown).await,
        }
    }

    pub(super) async fn terminate_after_wait_timeout(
        &mut self,
        wait_timeout: Duration,
        shutdown: GracefulShutdown,
    ) -> Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError> {
        let process_name = self.name.clone();
        match self.terminate(shutdown).await {
            Ok(exit_status) => Ok(WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
                result: exit_status,
                timeout: wait_timeout,
            }),
            Err(termination_error) => Err(WaitOrTerminateError::TerminationAfterTimeoutFailed {
                process_name,
                timeout: wait_timeout,
                termination_error,
            }),
        }
    }

    /// Test-only fault-injection seam. Drives the post-wait-error termination cleanup with a
    /// caller-supplied `WaitError`, exposing the same `WaitOrTerminateError` shape that the
    /// staged [`crate::WaitForCompletion`] builder would produce on a real wait failure.
    /// Production code should use the builder via [`ProcessHandle::wait_for_completion`] instead.
    #[doc(hidden)]
    pub async fn terminate_after_wait_error(
        &mut self,
        wait_error: WaitError,
        shutdown: GracefulShutdown,
    ) -> Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError> {
        let process_name = self.name.clone();

        match self.terminate(shutdown).await {
            Ok(termination_status) => Err(WaitOrTerminateError::WaitFailed {
                process_name,
                wait_error: Box::new(wait_error),
                termination_status,
            }),
            Err(termination_error) => Err(WaitOrTerminateError::TerminationFailed {
                process_name,
                wait_error: Box::new(wait_error),
                termination_error,
            }),
        }
    }
}
