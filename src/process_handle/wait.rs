use super::ProcessHandle;
use super::WaitForCompletionOrTerminateOptions;
use crate::error::{
    WaitError, WaitForCompletionOrTerminateResult, WaitForCompletionResult, WaitOrTerminateError,
};
use crate::output_stream::OutputStream;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WaitOrTerminateOutcome {
    pub(super) result: WaitForCompletionOrTerminateResult<ExitStatus>,
    pub(super) output_collection_timeout_budget: Duration,
}

fn wait_or_terminate_base_budget(
    wait_timeout: Duration,
    interrupt_timeout: Duration,
    terminate_timeout: Duration,
) -> Duration {
    wait_timeout
        .saturating_add(interrupt_timeout)
        .saturating_add(terminate_timeout)
}

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
        match self.child.wait().await {
            Ok(status) => {
                self.must_not_be_terminated();
                Ok(status)
            }
            Err(err) => Err(err),
        }
    }

    /// Wait for this process to run to completion within `timeout`.
    ///
    /// Any still-open stdin handle is closed before waiting begins, matching
    /// [`tokio::process::Child::wait`] and helping avoid deadlocks where the child is waiting for
    /// input while the parent is waiting for exit.
    ///
    /// If the timeout is reached before the process terminated, a timeout outcome is returned and the
    /// process keeps running without being signalled or killed. Its stdin has still been closed
    /// before the wait started.
    /// Use [`ProcessHandle::wait_for_completion_or_terminate`] if you want immediate termination.
    ///
    /// This does not provide the processes output. Use [`ProcessHandle::stdout`] and
    /// [`ProcessHandle::stderr`] to inspect, watch over, or capture the process output.
    ///
    /// # Errors
    ///
    /// Returns [`WaitError`] if waiting for the process fails.
    pub async fn wait_for_completion(
        &mut self,
        timeout: Duration,
    ) -> Result<WaitForCompletionResult, WaitError> {
        self.wait_for_completion_inner(timeout).await
    }

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

    pub(super) async fn wait_for_completion_unbounded_inner(
        &mut self,
    ) -> Result<ExitStatus, WaitError> {
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

    fn wait_timeout_error(timeout: Duration) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("process did not complete within {timeout:?}"),
        )
    }

    pub(super) fn wait_timeout_diagnostic(timeout: Duration) -> io::Error {
        Self::wait_timeout_error(timeout)
    }

    fn terminated_after_timeout_result(
        exit_status: ExitStatus,
        timeout: Duration,
        output_collection_timeout_budget: Duration,
    ) -> WaitOrTerminateOutcome {
        WaitOrTerminateOutcome {
            result: WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
                result: exit_status,
                timeout,
            },
            output_collection_timeout_budget,
        }
    }

    pub(super) fn exit_status_from_wait_or_terminate_result(
        result: WaitForCompletionOrTerminateResult<ExitStatus>,
    ) -> ExitStatus {
        match result {
            WaitForCompletionOrTerminateResult::Completed(exit_status)
            | WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
                result: exit_status,
                ..
            } => exit_status,
        }
    }

    /// Wait for this process to run to completion within `timeout`.
    ///
    /// Any still-open stdin handle is closed before the initial wait begins, matching
    /// [`tokio::process::Child::wait`].
    ///
    /// If the timeout is reached before the process terminated normally, external termination is
    /// forced through [`ProcessHandle::terminate`] and a
    /// [`WaitForCompletionOrTerminateResult::TerminatedAfterTimeout`] outcome is returned when
    /// cleanup succeeds. If waiting fails for a non-timeout reason, cleanup termination is still
    /// attempted and the original wait failure is preserved in the returned error.
    /// Total wall-clock time can exceed
    /// `wait_timeout + interrupt_timeout + terminate_timeout` by one additional fixed 3-second
    /// wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitOrTerminateError`] if waiting fails or if timeout-triggered termination is
    /// required and then fails.
    pub async fn wait_for_completion_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
    ) -> Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError> {
        self.wait_for_completion_or_terminate_inner(
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
        )
        .await
    }

    pub(super) async fn wait_for_completion_or_terminate_inner_detailed(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<WaitOrTerminateOutcome, WaitOrTerminateError> {
        let output_collection_timeout_budget =
            wait_or_terminate_base_budget(wait_timeout, interrupt_timeout, terminate_timeout);

        match self.wait_for_completion_inner(wait_timeout).await {
            Ok(WaitForCompletionResult::Completed(exit_status)) => Ok(WaitOrTerminateOutcome {
                result: WaitForCompletionOrTerminateResult::Completed(exit_status),
                output_collection_timeout_budget,
            }),
            Ok(WaitForCompletionResult::Timeout { timeout }) => {
                self.terminate_after_wait_timeout_detailed(
                    timeout,
                    interrupt_timeout,
                    terminate_timeout,
                )
                .await
            }
            Err(wait_error) => {
                self.terminate_after_wait_error_detailed(
                    wait_error,
                    wait_timeout,
                    interrupt_timeout,
                    terminate_timeout,
                )
                .await
            }
        }
    }

    pub(super) async fn wait_for_completion_or_terminate_inner(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError> {
        self.wait_for_completion_or_terminate_inner_detailed(
            wait_timeout,
            interrupt_timeout,
            terminate_timeout,
        )
        .await
        .map(|outcome| outcome.result)
    }

    pub(super) async fn terminate_after_wait_timeout_detailed(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<WaitOrTerminateOutcome, WaitOrTerminateError> {
        let process_name = self.name.clone();
        let output_collection_timeout_budget =
            wait_or_terminate_base_budget(wait_timeout, interrupt_timeout, terminate_timeout);
        match self
            .terminate_detailed(interrupt_timeout, terminate_timeout)
            .await
        {
            Ok(termination_outcome) => Ok(Self::terminated_after_timeout_result(
                termination_outcome.exit_status,
                wait_timeout,
                output_collection_timeout_budget
                    .saturating_add(termination_outcome.output_collection_timeout_extension),
            )),
            Err(termination_error) => Err(WaitOrTerminateError::TerminationAfterTimeoutFailed {
                process_name,
                timeout: wait_timeout,
                termination_error,
            }),
        }
    }

    pub(super) async fn terminate_after_wait_error_detailed(
        &mut self,
        wait_error: WaitError,
        _wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<WaitOrTerminateOutcome, WaitOrTerminateError> {
        let process_name = self.name.clone();

        match self
            .terminate_detailed(interrupt_timeout, terminate_timeout)
            .await
        {
            Ok(termination_outcome) => Err(WaitOrTerminateError::WaitFailed {
                process_name,
                wait_error: Box::new(wait_error),
                termination_status: termination_outcome.exit_status,
            }),
            Err(termination_error) => Err(WaitOrTerminateError::TerminationFailed {
                process_name,
                wait_error: Box::new(wait_error),
                termination_error,
            }),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) async fn terminate_after_wait_error(
        &mut self,
        wait_error: WaitError,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, WaitOrTerminateError> {
        self.terminate_after_wait_error_detailed(
            wait_error,
            Duration::ZERO,
            interrupt_timeout,
            terminate_timeout,
        )
        .await
        .map(|outcome| Self::exit_status_from_wait_or_terminate_result(outcome.result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        line_collection_options, line_parsing_options, long_running_command,
    };
    use crate::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt};
    use assertr::prelude::*;
    use tokio::io::AsyncWriteExt;

    fn wait_or_terminate_options(wait_timeout: Duration) -> WaitForCompletionOrTerminateOptions {
        WaitForCompletionOrTerminateOptions {
            wait_timeout,
            interrupt_timeout: Duration::from_secs(1),
            terminate_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn wait_for_completion_disarms_cleanup_and_panic_guards() {
        let mut process = crate::Process::new(long_running_command(Duration::from_millis(100)))
            .name("long-running")
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
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn wait_for_completion_closes_stdin_before_waiting() {
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
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

        let collector = process
            .stdout()
            .collect_lines_into_vec(line_parsing_options(), line_collection_options());

        let Some(stdin) = process.stdin().as_mut() else {
            assert_that!(process.stdin().is_open()).fail("stdin should start open");
            return;
        };
        stdin.write_all(b"wait closes stdin\n").await.unwrap();
        stdin.flush().await.unwrap();

        let status = process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(status.success()).is_true();
        assert_that!(process.stdin().is_open()).is_false();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().len()).is_equal_to(1);
        assert_that!(collected[0].as_str()).is_equal_to("wait closes stdin");
    }

    #[tokio::test]
    async fn or_terminate_returns_wait_failure_after_cleanup() {
        let mut process = crate::Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
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
            process_name: "long-running".into(),
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

        let WaitError::IoError { source, .. } = *wait_error;
        assert_that!(source.to_string().as_str()).is_equal_to("synthetic wait failure");
    }

    #[tokio::test]
    async fn wait_for_completion_or_terminate_terminates_after_timeout() {
        let mut process = crate::Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
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

        let result = process
            .wait_for_completion_or_terminate(wait_or_terminate_options(Duration::from_millis(10)))
            .await
            .unwrap();
        let status = match result {
            WaitForCompletionOrTerminateResult::TerminatedAfterTimeout { result, timeout } => {
                assert_that!(timeout).is_equal_to(Duration::from_millis(10));
                result
            }
            other @ WaitForCompletionOrTerminateResult::Completed(_) => {
                assert_that!(&other).fail(format_args!(
                    "expected timeout-driven termination, got {other:?}"
                ));
                return;
            }
        };

        assert_that!(status.success()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }
}
