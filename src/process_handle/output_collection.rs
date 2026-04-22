use super::{
    LineOutputOptions, ProcessHandle, RawOutputOptions, WaitForCompletionOptions,
    WaitForCompletionOrTerminateOptions, WaitForCompletionOrTerminateWithOutputOptions,
    WaitForCompletionOrTerminateWithRawOutputOptions,
    WaitForCompletionOrTerminateWithTrustedLineOutputOptions, WaitForCompletionWithOutputOptions,
    WaitForCompletionWithRawOutputOptions, WaitForCompletionWithTrustedLineOutputOptions,
};
use crate::collector::{Collector, Sink};
use crate::error::{WaitForCompletionWithOutputError, WaitForCompletionWithOutputOrTerminateError};
use crate::output::ProcessOutput;
use crate::output_stream::OutputStream;
use crate::output_stream::collectable::CollectableOutputStream;
use crate::{CollectedBytes, CollectedLines};
use std::process::ExitStatus;
use std::time::Duration;

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
    let status = handle.wait_for_completion_inner(timeout).await?;
    let stdout = stdout_collector.wait().await?;
    let stderr = stderr_collector.wait().await?;

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
    let status = handle
        .wait_for_completion_or_terminate_inner(wait_timeout, interrupt_timeout, terminate_timeout)
        .await?;
    let stdout = stdout_collector.wait().await?;
    let stderr = stderr_collector.wait().await?;

    Ok((status, stdout, stderr))
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
    use crate::{
        CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
        LineCollectionOptions, LineOutputOptions, LineOverflowBehavior, LineParsingOptions,
        NumBytesExt, RawCollectionOptions, RawOutputOptions, WaitForCompletionOptions,
        WaitForCompletionOrTerminateOptions, WaitForCompletionOrTerminateWithOutputOptions,
        WaitForCompletionOrTerminateWithRawOutputOptions,
        WaitForCompletionOrTerminateWithTrustedLineOutputOptions,
        WaitForCompletionWithOutputOptions, WaitForCompletionWithRawOutputOptions,
        WaitForCompletionWithTrustedLineOutputOptions,
    };
    use assertr::prelude::*;
    use std::time::Duration;

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
