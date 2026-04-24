use super::{
    LineOutputOptions, ProcessHandle, RawOutputOptions, WaitForCompletionOptions,
    WaitForCompletionOrTerminateOptions, WaitForCompletionOrTerminateWithOutputOptions,
    WaitForCompletionOrTerminateWithRawOutputOptions,
    WaitForCompletionOrTerminateWithTrustedLineOutputOptions, WaitForCompletionWithOutputOptions,
    WaitForCompletionWithRawOutputOptions, WaitForCompletionWithTrustedLineOutputOptions,
};
use crate::collector::{Collector, CollectorError, Sink};
use crate::error::{WaitForCompletionWithOutputError, WaitForCompletionWithOutputOrTerminateError};
use crate::output::ProcessOutput;
use crate::output_stream::OutputStream;
use crate::output_stream::collectable::CollectableOutputStream;
use crate::{CollectedBytes, CollectedLines};
use std::future::{Future, poll_fn};
use std::process::ExitStatus;
use std::task::Poll;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy)]
struct OperationDeadline {
    timeout: Duration,
    deadline: Instant,
}

impl OperationDeadline {
    fn from_started_at(started_at: Instant, timeout: Duration) -> Self {
        Self {
            timeout,
            deadline: started_at + timeout,
        }
    }

    fn from_timeout(timeout: Duration) -> Self {
        Self::from_started_at(Instant::now(), timeout)
    }
}

#[derive(Debug)]
enum CollectorDrainError {
    Timeout,
    Collection(CollectorError),
}

enum TimedDrainAction<StdoutSink, StderrSink> {
    Done(Result<(StdoutSink, StderrSink), CollectorDrainError>),
    AbortStdout(CollectorDrainError),
    AbortStderr(CollectorDrainError),
}

async fn drain_output_collectors<StdoutSink, StderrSink>(
    stdout_collector: Collector<StdoutSink>,
    stderr_collector: Collector<StderrSink>,
) -> Result<(StdoutSink, StderrSink), CollectorError>
where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    tokio::try_join!(stdout_collector.wait(), stderr_collector.wait())
}

async fn wait_for_output_collectors<StdoutSink, StderrSink>(
    stdout_collector: Collector<StdoutSink>,
    stderr_collector: Collector<StderrSink>,
    deadline: Option<Instant>,
) -> Result<(StdoutSink, StderrSink), CollectorDrainError>
where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    match deadline {
        None => drain_output_collectors(stdout_collector, stderr_collector)
            .await
            .map_err(CollectorDrainError::Collection),
        Some(deadline) => {
            let mut stdout_wait = stdout_collector.into_wait();
            let mut stderr_wait = stderr_collector.into_wait();
            // Keep the borrowed wait futures in this scope so we can abort the sibling wait handle
            // only after those borrows have been dropped.
            let action = {
                let stdout_future = stdout_wait.wait_until(deadline);
                tokio::pin!(stdout_future);
                let stderr_future = stderr_wait.wait_until(deadline);
                tokio::pin!(stderr_future);

                tokio::select! {
                    stdout = &mut stdout_future => match stdout {
                        Ok(Some(stdout)) => TimedDrainAction::Done(
                            drain_remaining_collector(stderr_future.as_mut())
                                .await
                                .map(|stderr| (stdout, stderr)),
                        ),
                        Err(err) => TimedDrainAction::AbortStderr(
                            CollectorDrainError::Collection(err),
                        ),
                        Ok(None) => TimedDrainAction::AbortStderr(
                            peek_drain_error(stderr_future.as_mut()).await,
                        ),
                    },
                    stderr = &mut stderr_future => match stderr {
                        Ok(Some(stderr)) => TimedDrainAction::Done(
                            drain_remaining_collector(stdout_future.as_mut())
                                .await
                                .map(|stdout| (stdout, stderr)),
                        ),
                        Err(err) => TimedDrainAction::AbortStdout(
                            CollectorDrainError::Collection(err),
                        ),
                        Ok(None) => TimedDrainAction::AbortStdout(
                            peek_drain_error(stdout_future.as_mut()).await,
                        ),
                    },
                }
            };

            match action {
                TimedDrainAction::Done(result) => result,
                TimedDrainAction::AbortStdout(err) => {
                    stdout_wait.abort().await;
                    Err(err)
                }
                TimedDrainAction::AbortStderr(err) => {
                    stderr_wait.abort().await;
                    Err(err)
                }
            }
        }
    }
}

async fn drain_remaining_collector<S, F>(
    mut remaining_wait: std::pin::Pin<&mut F>,
) -> Result<S, CollectorDrainError>
where
    S: Sink,
    F: Future<Output = Result<Option<S>, CollectorError>>,
{
    match remaining_wait.as_mut().await {
        Ok(Some(sink)) => Ok(sink),
        Err(err) => Err(CollectorDrainError::Collection(err)),
        Ok(None) => Err(CollectorDrainError::Timeout),
    }
}

fn peek_drain_error<S, F>(
    mut remaining_wait: std::pin::Pin<&mut F>,
) -> impl Future<Output = CollectorDrainError> + '_
where
    S: Sink,
    F: Future<Output = Result<Option<S>, CollectorError>>,
{
    poll_fn(move |cx| match remaining_wait.as_mut().poll(cx) {
        Poll::Ready(Err(err)) => Poll::Ready(CollectorDrainError::Collection(err)),
        _ => Poll::Ready(CollectorDrainError::Timeout),
    })
}

fn map_output_collector_error(
    err: CollectorDrainError,
    process_name: std::borrow::Cow<'static, str>,
    deadline: Option<OperationDeadline>,
) -> WaitForCompletionWithOutputError {
    match err {
        CollectorDrainError::Collection(source) => source.into(),
        CollectorDrainError::Timeout => {
            let deadline = deadline.expect("deadline to be present after drain timeout");
            WaitForCompletionWithOutputError::OutputCollectionTimeout {
                process_name,
                timeout: deadline.timeout,
            }
        }
    }
}

fn map_output_or_terminate_collector_error(
    err: CollectorDrainError,
    process_name: std::borrow::Cow<'static, str>,
    deadline: OperationDeadline,
) -> WaitForCompletionWithOutputOrTerminateError {
    match err {
        CollectorDrainError::Collection(source) => source.into(),
        CollectorDrainError::Timeout => {
            WaitForCompletionWithOutputOrTerminateError::OutputCollectionTimeout {
                process_name,
                timeout: deadline.timeout,
            }
        }
    }
}

async fn wait_for_completion_with_collectors<Stdout, Stderr, StdoutSink, StderrSink>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    timeout: Option<Duration>,
    stdout_collector: Collector<StdoutSink>,
    stderr_collector: Collector<StderrSink>,
) -> Result<(ExitStatus, StdoutSink, StderrSink), WaitForCompletionWithOutputError>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let deadline = timeout.map(OperationDeadline::from_timeout);
    let status = handle.wait_for_completion_inner(timeout).await?;
    let (stdout, stderr) = wait_for_output_collectors(
        stdout_collector,
        stderr_collector,
        deadline.map(|d| d.deadline),
    )
    .await
    .map_err(|err| map_output_collector_error(err, handle.name.clone(), deadline))?;

    Ok((status, stdout, stderr))
}

async fn wait_for_completion_or_terminate_with_collectors<Stdout, Stderr, StdoutSink, StderrSink>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    wait_timeout: Duration,
    interrupt_timeout: Duration,
    terminate_timeout: Duration,
    stdout_collector: Collector<StdoutSink>,
    stderr_collector: Collector<StderrSink>,
) -> Result<(ExitStatus, StdoutSink, StderrSink), WaitForCompletionWithOutputOrTerminateError>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let started_at = Instant::now();
    let outcome = handle
        .wait_for_completion_or_terminate_inner_detailed(
            wait_timeout,
            interrupt_timeout,
            terminate_timeout,
        )
        .await?;
    let deadline =
        OperationDeadline::from_started_at(started_at, outcome.output_collection_timeout_budget);
    let (stdout, stderr) =
        wait_for_output_collectors(stdout_collector, stderr_collector, Some(deadline.deadline))
            .await
            .map_err(|err| {
                map_output_or_terminate_collector_error(err, handle.name.clone(), deadline)
            })?;

    Ok((outcome.exit_status, stdout, stderr))
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: CollectableOutputStream,
    Stderr: CollectableOutputStream,
{
    /// Waits for the process to complete while collecting bounded line output.
    ///
    /// Collectors are attached when this method is called. If the stream was configured with
    /// `.no_replay()`, output produced before attachment may be discarded; configure replay before
    /// spawning when startup output must be included.
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `options.timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_output(
        &mut self,
        options: WaitForCompletionWithOutputOptions,
    ) -> Result<ProcessOutput<CollectedLines>, WaitForCompletionWithOutputError> {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = options.line_output_options;
        let out_collector = self
            .stdout()
            .collect_lines_into_vec(line_parsing_options, stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_lines_into_vec(line_parsing_options, stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_with_collectors(
            self,
            options.timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for the process to complete while collecting all line output into memory.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its output
    /// volume are trusted.
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `options.timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_output_trusted(
        &mut self,
        options: WaitForCompletionWithTrustedLineOutputOptions,
    ) -> Result<ProcessOutput<Vec<String>>, WaitForCompletionWithOutputError> {
        let out_collector = self
            .stdout()
            .collect_all_lines_into_vec_trusted(options.line_parsing_options);
        let err_collector = self
            .stderr()
            .collect_all_lines_into_vec_trusted(options.line_parsing_options);

        let (status, stdout, stderr) = wait_for_completion_with_collectors(
            self,
            options.timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for the process to complete while collecting bounded raw byte output.
    ///
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `options.timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_raw_output(
        &mut self,
        options: WaitForCompletionWithRawOutputOptions,
    ) -> Result<ProcessOutput<CollectedBytes>, WaitForCompletionWithOutputError> {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = options.raw_output_options;
        let out_collector = self
            .stdout()
            .collect_chunks_into_vec(stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_chunks_into_vec(stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_with_collectors(
            self,
            options.timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for the process to complete while collecting all raw byte output into memory.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its output
    /// volume are trusted.
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `options.timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_raw_output_trusted(
        &mut self,
        options: WaitForCompletionOptions,
    ) -> Result<ProcessOutput<Vec<u8>>, WaitForCompletionWithOutputError> {
        let out_collector = self.stdout().collect_all_chunks_into_vec_trusted();
        let err_collector = self.stderr().collect_all_chunks_into_vec_trusted();

        let (status, stdout, stderr) = wait_for_completion_with_collectors(
            self,
            options.timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting bounded line output.
    ///
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateWithOutputOptions,
    ) -> Result<ProcessOutput<CollectedLines>, WaitForCompletionWithOutputOrTerminateError> {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = options.line_output_options;
        let out_collector = self
            .stdout()
            .collect_lines_into_vec(line_parsing_options, stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_lines_into_vec(line_parsing_options, stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting all line output into memory.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its output
    /// volume are trusted.
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_output_or_terminate_trusted(
        &mut self,
        options: WaitForCompletionOrTerminateWithTrustedLineOutputOptions,
    ) -> Result<ProcessOutput<Vec<String>>, WaitForCompletionWithOutputOrTerminateError> {
        let out_collector = self
            .stdout()
            .collect_all_lines_into_vec_trusted(options.line_parsing_options);
        let err_collector = self
            .stderr()
            .collect_all_lines_into_vec_trusted(options.line_parsing_options);

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting bounded raw byte output.
    ///
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_raw_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateWithRawOutputOptions,
    ) -> Result<ProcessOutput<CollectedBytes>, WaitForCompletionWithOutputOrTerminateError> {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = options.raw_output_options;
        let out_collector = self
            .stdout()
            .collect_chunks_into_vec(stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_chunks_into_vec(stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting all raw byte output into memory.
    ///
    /// This grows memory without a total output cap. Use it only when the child process and its output
    /// volume are trusted.
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_raw_output_or_terminate_trusted(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
    ) -> Result<ProcessOutput<Vec<u8>>, WaitForCompletionWithOutputOrTerminateError> {
        let out_collector = self.stdout().collect_all_chunks_into_vec_trusted();
        let err_collector = self.stderr().collect_all_chunks_into_vec_trusted();

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CollectorDrainError, wait_for_output_collectors};
    use crate::output_stream::StreamConfig;
    use crate::output_stream::backend::broadcast::BroadcastOutputStream;
    use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
    use crate::{
        CollectionOverflowBehavior, CollectorError, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, LineCollectionOptions, LineOutputOptions, LineOverflowBehavior,
        LineParsingOptions, NumBytesExt, RawCollectionOptions, RawOutputOptions,
        WaitForCompletionOptions, WaitForCompletionOrTerminateOptions,
        WaitForCompletionOrTerminateWithOutputOptions,
        WaitForCompletionOrTerminateWithRawOutputOptions,
        WaitForCompletionOrTerminateWithTrustedLineOutputOptions, WaitForCompletionWithOutputError,
        WaitForCompletionWithOutputOptions, WaitForCompletionWithOutputOrTerminateError,
        WaitForCompletionWithRawOutputOptions, WaitForCompletionWithTrustedLineOutputOptions,
    };
    use assertr::prelude::*;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
    use tokio::time::Instant;

    fn wait_options(timeout: Option<Duration>) -> WaitForCompletionOptions {
        WaitForCompletionOptions::builder().timeout(timeout).build()
    }

    fn line_parsing_options() -> LineParsingOptions {
        LineParsingOptions::builder()
            .max_line_length(16.kilobytes())
            .overflow_behavior(LineOverflowBehavior::default())
            .build()
    }

    fn line_collection_options() -> LineCollectionOptions {
        LineCollectionOptions::builder()
            .max_bytes(1.megabytes())
            .max_lines(1024)
            .overflow_behavior(CollectionOverflowBehavior::default())
            .build()
    }

    fn line_output_options() -> LineOutputOptions {
        let line_collection_options = line_collection_options();
        LineOutputOptions::builder()
            .line_parsing_options(line_parsing_options())
            .stdout_collection_options(line_collection_options)
            .stderr_collection_options(line_collection_options)
            .build()
    }

    fn raw_output_options() -> RawOutputOptions {
        let raw_collection_options = RawCollectionOptions::new(1.megabytes());
        RawOutputOptions::builder()
            .stdout_collection_options(raw_collection_options)
            .stderr_collection_options(raw_collection_options)
            .build()
    }

    fn wait_with_line_output_options(
        timeout: Option<Duration>,
    ) -> WaitForCompletionWithOutputOptions {
        WaitForCompletionWithOutputOptions::builder()
            .timeout(timeout)
            .line_output_options(line_output_options())
            .build()
    }

    fn wait_with_raw_output_options(
        timeout: Option<Duration>,
    ) -> WaitForCompletionWithRawOutputOptions {
        WaitForCompletionWithRawOutputOptions::builder()
            .timeout(timeout)
            .raw_output_options(raw_output_options())
            .build()
    }

    fn wait_with_trusted_line_output_options(
        timeout: Option<Duration>,
    ) -> WaitForCompletionWithTrustedLineOutputOptions {
        WaitForCompletionWithTrustedLineOutputOptions::builder()
            .timeout(timeout)
            .line_parsing_options(line_parsing_options())
            .build()
    }

    fn wait_or_terminate_options(wait_timeout: Duration) -> WaitForCompletionOrTerminateOptions {
        WaitForCompletionOrTerminateOptions::builder()
            .wait_timeout(wait_timeout)
            .interrupt_timeout(Duration::from_secs(1))
            .terminate_timeout(Duration::from_secs(1))
            .build()
    }

    fn wait_or_terminate_with_line_output_options(
        wait_timeout: Duration,
    ) -> WaitForCompletionOrTerminateWithOutputOptions {
        WaitForCompletionOrTerminateWithOutputOptions::builder()
            .wait_timeout(wait_timeout)
            .interrupt_timeout(Duration::from_secs(1))
            .terminate_timeout(Duration::from_secs(1))
            .line_output_options(line_output_options())
            .build()
    }

    fn wait_or_terminate_with_raw_output_options(
        wait_timeout: Duration,
    ) -> WaitForCompletionOrTerminateWithRawOutputOptions {
        WaitForCompletionOrTerminateWithRawOutputOptions::builder()
            .wait_timeout(wait_timeout)
            .interrupt_timeout(Duration::from_secs(1))
            .terminate_timeout(Duration::from_secs(1))
            .raw_output_options(raw_output_options())
            .build()
    }

    fn wait_or_terminate_with_trusted_line_output_options(
        wait_timeout: Duration,
    ) -> WaitForCompletionOrTerminateWithTrustedLineOutputOptions {
        WaitForCompletionOrTerminateWithTrustedLineOutputOptions::builder()
            .wait_timeout(wait_timeout)
            .interrupt_timeout(Duration::from_secs(1))
            .terminate_timeout(Duration::from_secs(1))
            .line_parsing_options(line_parsing_options())
            .build()
    }

    fn inherited_stdout_command() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf 'ready\n'; sleep 0.5 &");
        cmd
    }

    #[cfg(unix)]
    fn force_kill_with_inherited_stdout_command() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("trap '' INT TERM; printf 'ready\n'; sleep 0.2 & while :; do :; done");
        cmd
    }

    fn best_effort_no_replay_options() -> StreamConfig<crate::BestEffortDelivery, crate::NoReplay> {
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
            .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            .build()
    }

    #[derive(Debug)]
    struct ReadErrorAfterBytes {
        bytes: &'static [u8],
        offset: usize,
        error_kind: io::ErrorKind,
    }

    impl ReadErrorAfterBytes {
        fn new(bytes: &'static [u8], error_kind: io::ErrorKind) -> Self {
            Self {
                bytes,
                offset: 0,
                error_kind,
            }
        }
    }

    impl AsyncRead for ReadErrorAfterBytes {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.offset < self.bytes.len() {
                let remaining = &self.bytes[self.offset..];
                let len = remaining.len().min(buf.remaining());
                buf.put_slice(&remaining[..len]);
                self.offset += len;
                return Poll::Ready(Ok(()));
            }

            Poll::Ready(Err(io::Error::new(
                self.error_kind,
                "injected read failure",
            )))
        }
    }

    #[tokio::test]
    async fn test_wait_for_completion_with_output_preserves_unterminated_final_line() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf tail");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["tail"]);
        assert_that!(output.stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_captures_startup_output_with_replay_last_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'startup-out\n'; printf 'startup-err\n' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["startup-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["startup-err"]);
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_closes_stdin_before_waiting() {
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
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

        let Some(stdin) = process.stdin().as_mut() else {
            assert_that!(process.stdin().is_open()).fail("stdin should start open");
            return;
        };
        stdin
            .write_all(b"collector wait closes stdin\n")
            .await
            .unwrap();
        stdin.flush().await.unwrap();

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(process.stdin().is_open()).is_false();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["collector wait closes stdin"]);
        assert_that!(output.stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_times_out_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(inherited_stdout_command())
            .name("sh")
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

        let timeout = Duration::from_millis(100);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion_with_output(wait_with_line_output_options(Some(timeout))),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitForCompletionWithOutputError::OutputCollectionTimeout {
                timeout: actual, ..
            } => {
                assert_that!(actual).is_equal_to(timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn timed_output_collection_returns_early_stream_read_error_before_deadline() {
        let stdout_stream = BroadcastOutputStream::from_stream(
            ReadErrorAfterBytes::new(b"", io::ErrorKind::BrokenPipe),
            "stdout",
            best_effort_no_replay_options(),
        );
        let (stderr_read, _stderr_write) = tokio::io::duplex(64);
        let stderr_stream = BroadcastOutputStream::from_stream(
            stderr_read,
            "stderr",
            best_effort_no_replay_options(),
        );

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            wait_for_output_collectors(
                stdout_stream.collect_all_chunks_into_vec_trusted(),
                stderr_stream.collect_all_chunks_into_vec_trusted(),
                Some(Instant::now() + Duration::from_secs(30)),
            ),
        )
        .await
        .expect("collector error should be returned before the outer timeout");

        match result {
            Err(CollectorDrainError::Collection(CollectorError::StreamRead { source, .. })) => {
                assert_that!(source.stream_name()).is_equal_to("stdout");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected early stream read error, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn wait_for_completion_with_raw_output_times_out_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(inherited_stdout_command())
            .name("sh")
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

        let timeout = Duration::from_millis(100);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process
                .wait_for_completion_with_raw_output(wait_with_raw_output_options(Some(timeout))),
        )
        .await
        .expect("raw output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitForCompletionWithOutputError::OutputCollectionTimeout {
                timeout: actual, ..
            } => {
                assert_that!(actual).is_equal_to(timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn single_subscriber_output_wait_timeout_releases_collector_claim() {
        let mut process = crate::Process::new(inherited_stdout_command())
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let result = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_millis(100),
            )))
            .await;
        assert_that!(result.is_err()).is_true();

        let collector = process
            .stdout()
            .collect_lines_into_vec(line_parsing_options(), line_collection_options());
        let _collected = collector.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn timed_output_collection_cancels_hanging_single_subscriber_sibling_after_error() {
        let stdout_stream = BroadcastOutputStream::from_stream(
            ReadErrorAfterBytes::new(b"", io::ErrorKind::BrokenPipe),
            "stdout",
            best_effort_no_replay_options(),
        );
        let (stderr_read, _stderr_write) = tokio::io::duplex(64);
        let stderr_stream = SingleSubscriberOutputStream::from_stream(
            stderr_read,
            "stderr",
            best_effort_no_replay_options(),
        );

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            wait_for_output_collectors(
                stdout_stream.collect_all_chunks_into_vec_trusted(),
                stderr_stream.collect_all_chunks_into_vec_trusted(),
                Some(Instant::now() + Duration::from_secs(30)),
            ),
        )
        .await
        .expect("collector error should be returned before the outer timeout");

        match result {
            Err(CollectorDrainError::Collection(CollectorError::StreamRead { source, .. })) => {
                assert_that!(source.stream_name()).is_equal_to("stdout");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected early stream read error, got {other:?}"
                ));
            }
        }

        let collector = stderr_stream.collect_all_chunks_into_vec_trusted();
        let _collected = collector.cancel().await.unwrap();
    }

    #[tokio::test]
    async fn single_subscriber_timed_output_wait_can_be_retried_after_process_finishes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(
            "printf 'early-out\n'; printf 'early-err\n' >&2; \
             sleep 0.2; \
             printf 'late-out\n'; printf 'late-err\n' >&2",
        );

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let first = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_millis(25),
            )))
            .await;
        assert_that!(first.is_err()).is_true();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["early-out", "late-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["early-err", "late-err"]);
    }

    #[tokio::test]
    async fn single_subscriber_timed_raw_output_wait_can_be_retried_after_process_finishes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(
            "printf 'early-out'; printf 'early-err' >&2; \
             sleep 0.2; \
             printf 'late-out'; printf 'late-err' >&2",
        );

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let first = process
            .wait_for_completion_with_raw_output(wait_with_raw_output_options(Some(
                Duration::from_millis(25),
            )))
            .await;
        assert_that!(first.is_err()).is_true();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let output = process
            .wait_for_completion_with_raw_output(wait_with_raw_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"early-outlate-out".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"early-errlate-err".to_vec());
    }

    #[tokio::test]
    async fn test_broadcast_wait_for_completion_with_raw_output_preserves_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'out\nraw'; printf 'err\nraw' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_raw_output(wait_with_raw_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"out\nraw".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"err\nraw".to_vec());
    }

    #[tokio::test]
    async fn test_wait_for_completion_with_raw_output_reports_truncation() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf abcdef");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_raw_output(
                WaitForCompletionWithRawOutputOptions::builder()
                    .timeout(Some(Duration::from_secs(2)))
                    .raw_output_options(
                        RawOutputOptions::builder()
                            .stdout_collection_options(RawCollectionOptions::new(3.bytes()))
                            .stderr_collection_options(RawCollectionOptions::new(3.bytes()))
                            .build(),
                    )
                    .build(),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"abc".to_vec());
        assert_that!(output.stdout.truncated).is_true();
        assert_that!(output.stderr.truncated).is_false();
    }

    #[tokio::test]
    async fn test_single_subscriber_wait_for_completion_with_raw_output_preserves_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'out\nraw'; printf 'err\nraw' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let output = process
            .wait_for_completion_with_raw_output(wait_with_raw_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"out\nraw".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"err\nraw".to_vec());
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_trusted_collects_lines() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'out\nline\n'; printf 'err\nline\n' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_output_trusted(wait_with_trusted_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.iter().map(String::as_str)).contains_exactly(["out", "line"]);
        assert_that!(output.stderr.iter().map(String::as_str)).contains_exactly(["err", "line"]);
    }

    #[tokio::test]
    async fn wait_for_completion_with_raw_output_trusted_collects_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'trusted-out'; printf 'trusted-err' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_raw_output_trusted(wait_options(Some(Duration::from_secs(2))))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout).is_equal_to(b"trusted-out".to_vec());
        assert_that!(output.stderr).is_equal_to(b"trusted-err".to_vec());
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_or_terminate_collects_lines() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'line-out\n'; printf 'line-err\n' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_output_or_terminate(
                wait_or_terminate_with_line_output_options(Duration::from_secs(2)),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["line-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["line-err"]);
    }

    #[tokio::test]
    async fn output_or_terminate_timeout_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(inherited_stdout_command())
            .name("sh")
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

        let wait_timeout = Duration::from_millis(100);
        let interrupt_timeout = Duration::from_millis(1);
        let terminate_timeout = Duration::from_millis(1);
        let operation_timeout = wait_timeout + interrupt_timeout + terminate_timeout;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion_with_output_or_terminate(
                WaitForCompletionOrTerminateWithOutputOptions::builder()
                    .wait_timeout(wait_timeout)
                    .interrupt_timeout(interrupt_timeout)
                    .terminate_timeout(terminate_timeout)
                    .line_output_options(line_output_options())
                    .build(),
            ),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitForCompletionWithOutputOrTerminateError::OutputCollectionTimeout {
                timeout: actual,
                ..
            } => {
                assert_that!(actual).is_equal_to(operation_timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_or_terminate_force_kill_extends_output_collection_budget() {
        let mut process = crate::Process::new(force_kill_with_inherited_stdout_command())
            .name("sh")
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

        let wait_timeout = Duration::from_millis(25);
        let interrupt_timeout = Duration::from_millis(25);
        let terminate_timeout = Duration::from_millis(25);
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            process.wait_for_completion_with_output_or_terminate(
                WaitForCompletionOrTerminateWithOutputOptions::builder()
                    .wait_timeout(wait_timeout)
                    .interrupt_timeout(interrupt_timeout)
                    .terminate_timeout(terminate_timeout)
                    .line_output_options(line_output_options())
                    .build(),
            ),
        )
        .await
        .expect("output wait should return before the outer guard")
        .expect("extended force-kill budget should let output collection finish");

        assert_that!(output.status.success()).is_false();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["ready"]);
    }

    #[tokio::test]
    async fn wait_for_completion_with_output_or_terminate_trusted_collects_lines() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'trusted-line-out\n'; printf 'trusted-line-err\n' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_output_or_terminate_trusted(
                wait_or_terminate_with_trusted_line_output_options(Duration::from_secs(2)),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.iter().map(String::as_str))
            .contains_exactly(["trusted-line-out"]);
        assert_that!(output.stderr.iter().map(String::as_str))
            .contains_exactly(["trusted-line-err"]);
    }

    #[tokio::test]
    async fn wait_for_completion_with_raw_output_or_terminate_collects_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf 'raw-out'; printf 'raw-err' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_raw_output_or_terminate(
                wait_or_terminate_with_raw_output_options(Duration::from_secs(2)),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"raw-out".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"raw-err".to_vec());
    }

    #[tokio::test]
    async fn wait_for_completion_with_raw_output_or_terminate_trusted_collects_bytes() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'trusted-raw-out'; printf 'trusted-raw-err' >&2");

        let mut process = crate::Process::new(cmd)
            .name("sh")
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

        let output = process
            .wait_for_completion_with_raw_output_or_terminate_trusted(wait_or_terminate_options(
                Duration::from_secs(2),
            ))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout).is_equal_to(b"trusted-raw-out".to_vec());
        assert_that!(output.stderr).is_equal_to(b"trusted-raw-err".to_vec());
    }
}
