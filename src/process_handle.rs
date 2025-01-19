use crate::output_stream::{extract_output_streams, OutputStream};
use crate::terminate_on_drop::TerminateOnDrop;
use crate::{signal, WaitError};
use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Child;

#[derive(Debug, Error)]
pub enum TerminationError {
    #[error("Failed to send signal to process: {0}")]
    SignallingFailed(#[from] io::Error),

    #[error("Failed to terminate process. Graceful SIGINT termination failure: {not_terminated_after_sigint}. Graceful SIGTERM termination failure: {not_terminated_after_sigterm}. Forceful termination failure: {not_terminated_after_sigkill}")]
    TerminationFailed {
        not_terminated_after_sigint: io::Error,
        not_terminated_after_sigterm: io::Error,
        not_terminated_after_sigkill: io::Error,
    },
}

/// Represents the running state of a process.
#[derive(Debug)]
pub enum IsRunning {
    /// Process is still running.
    Running,

    /// Process has terminated with the given exit status.
    NotRunning(ExitStatus),

    /// Failed to determine process state.
    Uncertain(io::Error),
}

impl IsRunning {
    pub fn as_bool(&self) -> bool {
        match self {
            IsRunning::Running => true,
            IsRunning::NotRunning(_) | IsRunning::Uncertain(_) => false,
        }
    }
}

impl From<IsRunning> for bool {
    fn from(is_running: IsRunning) -> Self {
        match is_running {
            IsRunning::Running => true,
            IsRunning::NotRunning(_) | IsRunning::Uncertain(_) => false,
        }
    }
}

#[derive(Debug)]
pub struct ProcessHandle {
    pub(crate) name: Cow<'static, str>,
    child: Child,
    std_out_stream: OutputStream,
    std_err_stream: OutputStream,
}

impl ProcessHandle {
    pub fn spawn(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
    ) -> io::Result<Self> {
        let child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // It is much too easy to leave dangling resources here and there.
            // This library tries to make it clear and encourage users to terminate spawned
            // processes appropriately. If not done so anyway, this acts as a "last resort"
            // type of solution, less graceful as the `terminate_on_drop` effect but at least
            // capable of cleaning up.
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self::new_from_child_with_piped_io(name, child))
    }

    pub fn new_from_child_with_piped_io(name: impl Into<Cow<'static, str>>, child: Child) -> Self {
        Self::new_from_child_with_piped_io_and_capacity(name, child, 128, 128)
    }

    pub fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        child: Child,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> Self {
        let (child, std_out_stream, std_err_stream) =
            extract_output_streams(child, stdout_channel_capacity, stderr_channel_capacity);
        Self {
            name: name.into(),
            child,
            std_out_stream,
            std_err_stream,
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> IsRunning {
        match self.child.try_wait() {
            Ok(None) => IsRunning::Running,
            Ok(Some(exit_status)) => IsRunning::NotRunning(exit_status),
            Err(err) => IsRunning::Uncertain(err),
        }
    }

    pub fn stdout(&self) -> &OutputStream {
        &self.std_out_stream
    }

    pub fn stderr(&self) -> &OutputStream {
        &self.std_err_stream
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub async fn wait_with_output(
        &mut self,
    ) -> Result<(ExitStatus, Vec<String>, Vec<String>), WaitError> {
        let out_collector = self.std_out_stream.collect_into_vec();
        let err_collector = self.std_err_stream.collect_into_vec();

        let status = self.child.wait().await?;
        let std_out = out_collector.abort().await?;
        let std_err = err_collector.abort().await?;

        Ok((status, std_out, std_err))
    }

    pub fn terminate_on_drop(
        self,
        graceful_termination_timeout: Duration,
        forceful_termination_timeout: Duration,
    ) -> TerminateOnDrop {
        TerminateOnDrop {
            process_handle: self,
            interrupt_timeout: graceful_termination_timeout,
            terminate_timeout: forceful_termination_timeout,
        }
    }

    /// Wait for this process to run to completion within `timeout` if set or unbound otherwise.
    async fn await_termination(&mut self, timeout: Option<Duration>) -> io::Result<ExitStatus> {
        match timeout {
            None => self.child.wait().await,
            Some(timeout) => match tokio::time::timeout(timeout, self.child.wait()).await {
                Ok(exit_status) => exit_status,
                Err(err) => Err(err.into()),
            },
        }
    }

    /// Manually sed a `SIGINT` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub async fn send_interrupt_signal(&mut self) -> Result<(), io::Error> {
        signal::send_interrupt(&self.child).await
    }

    /// Manually sed a `SIGTERM` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub async fn send_terminate_signal(&mut self) -> Result<(), io::Error> {
        signal::send_terminate(&self.child).await
    }

    /// Terminates this process by sending a `SIGINT`, `SIGTERM` or even a `SIGKILL` if the process
    /// doesn't run to completion after receiving any of the first two signals.
    ///
    /// Recommendation: Use timeout!
    pub async fn terminate(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        self.send_interrupt_signal()
            .await
            .map_err(TerminationError::SignallingFailed)?;

        match self.await_termination(Some(interrupt_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(not_terminated_after_sigint) => {
                tracing::warn!(
                    process = %self.name,
                    error = %not_terminated_after_sigint,
                    "Graceful shutdown using SIGINT (or equivalent on current platform) failed. Attempting graceful shutdown using SIGTERM signal."
                );

                self.send_terminate_signal()
                    .await
                    .map_err(TerminationError::SignallingFailed)?;

                match self.await_termination(Some(terminate_timeout)).await {
                    Ok(exit_status) => Ok(exit_status),
                    Err(not_terminated_after_sigterm) => {
                        tracing::warn!(
                            process = %self.name,
                            error = %not_terminated_after_sigterm,
                            "Graceful shutdown using SIGTERM (or equivalent on current platform) failed. Attempting forceful shutdown using SIGKILL signal."
                        );

                        match self.child.kill().await {
                            Ok(()) => {
                                // Note: A SIGKILL should typically (somewhat) immediately lead to
                                // termination of the process. But there are cases in which even
                                // a SIGKILL does not / can not / will not kill a process.
                                // Something must have been horribly gone wrong then...
                                // But: We do not want to wait indefinitely in case this happens
                                // and therefore wait for a fixed second after any SIGKILL event.
                                match self.await_termination(Some(Duration::from_secs(1))).await {
                                    Ok(exit_status) => Ok(exit_status),
                                    Err(not_terminated_after_sigkill) => {
                                        // Unlikely. See note above.
                                        Err(TerminationError::TerminationFailed {
                                            not_terminated_after_sigint,
                                            not_terminated_after_sigterm,
                                            not_terminated_after_sigkill,
                                        })
                                    }
                                }
                            }
                            Err(not_terminated_after_sigkill) => {
                                tracing::error!(
                                    process = %self.name,
                                    error = %not_terminated_after_sigkill,
                                    "Forceful shutdown using SIGKILL (or equivalent on current platform) failed. Process may still be running. Manual intervention required! Did the process register a shutdown handler?"
                                );

                                Err(TerminationError::TerminationFailed {
                                    not_terminated_after_sigint,
                                    not_terminated_after_sigterm,
                                    not_terminated_after_sigkill,
                                })
                            }
                        }
                    }
                }
            }
        }
    }

    /// Consumes this handle to provide the wrapped `tokio::process::Child` instance as well as the
    /// stdout and stderr output streams.
    pub fn into_inner(self) -> (Child, OutputStream, OutputStream) {
        (self.child, self.std_out_stream, self.std_err_stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_termination() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");
        let mut handle = ProcessHandle::spawn("sleep", cmd).unwrap();
        handle
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();
    }
}
