use super::ProcessHandle;
use crate::error::{WaitError, WaitOrTerminateError};
use crate::output_stream::OutputStream;
use crate::process_handle::options::WaitForCompletionOrTerminateOptions;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WaitOrTerminateOutcome {
    pub(super) exit_status: ExitStatus,
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

    /// Wait for this process to run to completion. Within `timeout`, if set, or unbound otherwise.
    ///
    /// Any still-open stdin handle is closed before waiting begins, matching
    /// [`tokio::process::Child::wait`] and helping avoid deadlocks where the child is waiting for
    /// input while the parent is waiting for exit.
    ///
    /// If the timeout is reached before the process terminated, an error is returned and the
    /// process keeps running without being signalled or killed. Its stdin has still been closed
    /// before the wait started.
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
        timeout: Option<Duration>,
    ) -> Result<ExitStatus, WaitError> {
        self.wait_for_completion_inner(timeout).await
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
    /// Any still-open stdin handle is closed before the initial wait begins, matching
    /// [`tokio::process::Child::wait`].
    ///
    /// If waiting fails for any reason, including the timeout being reached before the process
    /// terminated normally, external termination of the process is forced through
    /// [`ProcessHandle::terminate`].
    /// Total wall-clock time can exceed
    /// `wait_timeout + interrupt_timeout + terminate_timeout` by one additional fixed 3-second
    /// wait when force-kill fallback is required.
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

    pub(super) async fn wait_for_completion_or_terminate_inner_detailed(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<WaitOrTerminateOutcome, WaitOrTerminateError> {
        let output_collection_timeout_budget =
            wait_or_terminate_base_budget(wait_timeout, interrupt_timeout, terminate_timeout);

        match self.wait_for_completion_inner(Some(wait_timeout)).await {
            Ok(exit_status) => Ok(WaitOrTerminateOutcome {
                exit_status,
                output_collection_timeout_budget,
            }),
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
    ) -> Result<ExitStatus, WaitOrTerminateError> {
        self.wait_for_completion_or_terminate_inner_detailed(
            wait_timeout,
            interrupt_timeout,
            terminate_timeout,
        )
        .await
        .map(|outcome| outcome.exit_status)
    }

    pub(super) async fn terminate_after_wait_error_detailed(
        &mut self,
        wait_error: WaitError,
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
            Ok(termination_outcome) => {
                if matches!(wait_error, WaitError::Timeout { .. }) {
                    Ok(WaitOrTerminateOutcome {
                        exit_status: termination_outcome.exit_status,
                        output_collection_timeout_budget: output_collection_timeout_budget
                            .saturating_add(
                                termination_outcome.output_collection_timeout_extension,
                            ),
                    })
                } else {
                    Err(WaitOrTerminateError::WaitFailed {
                        process_name,
                        wait_error: Box::new(wait_error),
                        termination_status: termination_outcome.exit_status,
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
        .map(|outcome| outcome.exit_status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::long_running_command;
    use crate::{
        CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
        LineCollectionOptions, LineOverflowBehavior, LineParsingOptions, NumBytesExt,
    };
    use assertr::prelude::*;
    use tokio::io::AsyncWriteExt;

    fn wait_or_terminate_options(wait_timeout: Duration) -> WaitForCompletionOrTerminateOptions {
        WaitForCompletionOrTerminateOptions {
            wait_timeout,
            interrupt_timeout: Duration::from_secs(1),
            terminate_timeout: Duration::from_secs(1),
        }
    }

    fn line_collection_options() -> LineCollectionOptions {
        LineCollectionOptions::Bounded {
            max_bytes: 1.megabytes(),
            max_lines: 1024,
            overflow_behavior: CollectionOverflowBehavior::default(),
        }
    }

    #[tokio::test]
    async fn test_wait_for_completion_disarms_cleanup_and_panic_guards() {
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
            .wait_for_completion(Some(Duration::from_secs(2)))
            .await
            .unwrap();

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

        let collector = process.stdout().collect_lines_into_vec(
            LineParsingOptions::builder()
                .max_line_length(16.kilobytes())
                .overflow_behavior(LineOverflowBehavior::default())
                .build(),
            line_collection_options(),
        );

        let Some(stdin) = process.stdin().as_mut() else {
            assert_that!(process.stdin().is_open()).fail("stdin should start open");
            return;
        };
        stdin.write_all(b"wait closes stdin\n").await.unwrap();
        stdin.flush().await.unwrap();

        let status = process
            .wait_for_completion(Some(Duration::from_secs(2)))
            .await
            .unwrap();

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

        let status = process
            .wait_for_completion_or_terminate(wait_or_terminate_options(Duration::from_millis(10)))
            .await
            .unwrap();

        assert_that!(status.success()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }
}
