use crate::output_stream::{extract_output_streams, OutputStream};
use crate::terminate_on_drop::TerminateOnDrop;
use crate::{interrupt, WaitError};
use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;
use thiserror::Error;
use tokio::process::Child;

#[derive(Debug, Error)]
pub enum TerminationError {
    #[error("Failed to terminate process: {0}")]
    IoError(#[from] io::Error),

    #[error("Failed to terminate process. Graceful termination failed with: {graceful_error}. Forceful termination failed with: {forceful_error}")]
    TerminationFailed {
        /// The error that occurred during the graceful termination attempt.
        graceful_error: io::Error,

        /// The error that occurred during the forceful termination attempt.
        forceful_error: io::Error,
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
        graceful_termination_timeout: Option<Duration>,
        forceful_termination_timeout: Option<Duration>,
    ) -> TerminateOnDrop {
        TerminateOnDrop {
            process_handle: self,
            graceful_termination_timeout,
            forceful_termination_timeout,
        }
    }

    pub async fn terminate(
        &mut self,
        graceful_shutdown_timeout: Option<Duration>,
        forceful_shutdown_timeout: Option<Duration>,
    ) -> Result<ExitStatus, TerminationError> {
        // Try a graceful shutdown first.
        interrupt::send_interrupt(&self.child).await?;

        // Wait for process termination.
        match self.await_termination(graceful_shutdown_timeout).await {
            Ok(exit_status) => Ok(exit_status),
            Err(graceful_err) => {
                // Log the graceful shutdown failure
                tracing::warn!(
                    process = %self.name,
                    error = %graceful_err,
                    "Graceful shutdown failed, attempting forceful termination"
                );

                // Try a forceful kill
                match self.child.kill().await {
                    Ok(()) => match self.await_termination(forceful_shutdown_timeout).await {
                        Ok(exit_status) => Ok(exit_status),
                        Err(err) => Err(TerminationError::from(err)),
                    },
                    Err(forceful_err) => Err(TerminationError::TerminationFailed {
                        graceful_error: graceful_err,
                        forceful_error: forceful_err,
                    }),
                }
            }
        }
    }

    async fn await_termination(&mut self, timeout: Option<Duration>) -> io::Result<ExitStatus> {
        match timeout {
            None => self.child.wait().await,
            Some(timeout) => match tokio::time::timeout(timeout, self.child.wait()).await {
                Ok(exit_status) => exit_status,
                Err(err) => Err(err.into()),
            },
        }
    }
}
