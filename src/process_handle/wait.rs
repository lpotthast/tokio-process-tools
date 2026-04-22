use super::{ProcessHandle, WaitForCompletionOptions, WaitForCompletionOrTerminateOptions};
use crate::error::{WaitError, WaitOrTerminateError};
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
        match self.child.wait().await {
            Ok(status) => {
                self.must_not_be_terminated();
                Ok(status)
            }
            Err(err) => Err(err),
        }
    }

    /// Wait for this process to run to completion. Within `timeout`, if set, or unbound otherwise.
    ///
    /// If the timeout is reached before the process terminated, an error is returned but the
    /// process remains untouched / keeps running.
    /// Use [`ProcessHandle::wait_for_completion_or_terminate`] if you want immediate termination.
    ///
    /// This does not provide the processes output. Use [`ProcessHandle::stdout`] and
    /// [`ProcessHandle::stderr`] to inspect, watch over, or capture the process output.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError`] if waiting for the process fails or the timeout elapses.
    pub async fn wait_for_completion(
        &mut self,
        options: WaitForCompletionOptions,
    ) -> Result<ExitStatus, WaitError> {
        self.wait_for_completion_inner(options.timeout).await
    }

    pub(super) async fn wait_for_completion_inner(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<ExitStatus, WaitError> {
        match timeout {
            None => self.wait().await.map_err(|source| WaitError::IoError {
                process_name: self.name.clone(),
                source,
            }),
            Some(timeout_duration) => {
                match tokio::time::timeout(timeout_duration, self.wait()).await {
                    Ok(Ok(exit_status)) => Ok(exit_status),
                    Ok(Err(source)) => Err(WaitError::IoError {
                        process_name: self.name.clone(),
                        source,
                    }),
                    Err(_elapsed) => Err(WaitError::Timeout {
                        process_name: self.name.clone(),
                        timeout: timeout_duration,
                    }),
                }
            }
        }
    }

    /// Wait for this process to run to completion within `timeout`.
    ///
    /// If waiting fails for any reason, including the timeout being reached before the process
    /// terminated normally, external termination of the process is forced through
    /// [`ProcessHandle::terminate`].
    ///
    /// Note that this function may return `Ok` even though the timeout was reached, carrying the
    /// exit status received after sending a termination signal!
    ///
    /// # Errors
    ///
    /// Returns [`WaitOrTerminateError`] if waiting fails or if termination is required and then
    /// fails.
    pub async fn wait_for_completion_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
    ) -> Result<ExitStatus, WaitOrTerminateError> {
        self.wait_for_completion_or_terminate_inner(
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
        )
        .await
    }

    pub(super) async fn wait_for_completion_or_terminate_inner(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, WaitOrTerminateError> {
        match self.wait_for_completion_inner(Some(wait_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(wait_error) => {
                self.terminate_after_wait_error(wait_error, interrupt_timeout, terminate_timeout)
                    .await
            }
        }
    }

    pub(super) async fn terminate_after_wait_error(
        &mut self,
        wait_error: WaitError,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, WaitOrTerminateError> {
        let process_name = self.name.clone();
        match self.terminate(interrupt_timeout, terminate_timeout).await {
            Ok(termination_status) => {
                if matches!(wait_error, WaitError::Timeout { .. }) {
                    Ok(termination_status)
                } else {
                    Err(WaitOrTerminateError::WaitFailed {
                        process_name,
                        wait_error: Box::new(wait_error),
                        termination_status,
                    })
                }
            }
            Err(termination_error) => Err(WaitOrTerminateError::TerminationFailed {
                process_name,
                wait_error: Box::new(wait_error),
                termination_error,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt};
    use assertr::prelude::*;

    fn wait_options(timeout: Option<Duration>) -> WaitForCompletionOptions {
        WaitForCompletionOptions::builder().timeout(timeout).build()
    }

    fn wait_or_terminate_options(wait_timeout: Duration) -> WaitForCompletionOrTerminateOptions {
        WaitForCompletionOrTerminateOptions::builder()
            .wait_timeout(wait_timeout)
            .interrupt_timeout(Duration::from_secs(1))
            .terminate_timeout(Duration::from_secs(1))
            .build()
    }

    #[tokio::test]
    async fn test_wait_for_completion_disarms_cleanup_and_panic_guards() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("0.1");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        process
            .wait_for_completion(wait_options(Some(Duration::from_secs(2))))
            .await
            .unwrap();

        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn or_terminate_returns_wait_failure_after_cleanup() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let mut process = crate::Process::new(cmd)
            .with_name("sleep")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let wait_error = WaitError::IoError {
            process_name: "sleep".into(),
            source: io::Error::other("synthetic wait failure"),
        };

        let result = process
            .terminate_after_wait_error(wait_error, Duration::from_secs(1), Duration::from_secs(1))
            .await;

        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(process.panic_on_drop.is_none()).is_true();

        let (wait_error, termination_status) = match result {
            Err(WaitOrTerminateError::WaitFailed {
                wait_error,
                termination_status,
                ..
            }) => (wait_error, termination_status),
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected wait failure preserved after successful cleanup, got {other:?}"
                ));
                return;
            }
        };

        assert_that!(termination_status.code()).is_none();

        let source = match *wait_error {
            WaitError::IoError { source, .. } => source,
            timeout @ WaitError::Timeout { .. } => {
                assert_that!(&timeout).fail(format_args!(
                    "expected original IO wait error, got {timeout:?}"
                ));
                return;
            }
        };
        assert_that!(source.to_string().as_str()).is_equal_to("synthetic wait failure");
    }

    #[tokio::test]
    async fn wait_for_completion_or_terminate_terminates_after_timeout() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let status = process
            .wait_for_completion_or_terminate(wait_or_terminate_options(Duration::from_millis(10)))
            .await
            .unwrap();

        assert_that!(status.success()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }
}
