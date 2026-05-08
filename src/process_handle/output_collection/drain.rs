use crate::ProcessOutput;
#[cfg(any(unix, windows))]
use crate::error::WaitForCompletionOrTerminateResult;
use crate::error::{WaitForCompletionResult, WaitWithOutputError};
use crate::output_stream::OutputStream;
use crate::output_stream::consumer::{Consumer, ConsumerError, Sink};
use crate::process_handle::ProcessHandle;
#[cfg(any(unix, windows))]
use crate::process_handle::termination::GracefulShutdown;
use std::borrow::Cow;
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug)]
pub(super) enum ConsumerDrainError {
    Timeout,
    Collection(ConsumerError),
}

impl ConsumerDrainError {
    fn into_wait_with_output_error(
        self,
        process_name: Cow<'static, str>,
        eof_timeout: Duration,
    ) -> WaitWithOutputError {
        match self {
            ConsumerDrainError::Collection(source) => WaitWithOutputError::OutputCollectionFailed {
                process_name,
                source,
            },
            ConsumerDrainError::Timeout => WaitWithOutputError::OutputCollectionTimeout {
                process_name,
                timeout: eof_timeout,
            },
        }
    }
}

enum TimedDrainAction<StdoutSink, StderrSink> {
    Done(Result<(StdoutSink, StderrSink), ConsumerDrainError>),
    AbortStdout(ConsumerDrainError),
    AbortStderr(ConsumerDrainError),
}

pub(super) async fn wait_for_output_consumers<StdoutSink, StderrSink>(
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
    timeout: Duration,
) -> Result<(StdoutSink, StderrSink), ConsumerDrainError>
where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let deadline = Instant::now() + timeout;
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
                    drain_remaining_consumer(stderr_future.as_mut())
                        .await
                        .map(|stderr| (stdout, stderr)),
                ),
                Err(err) => TimedDrainAction::AbortStderr(
                    ConsumerDrainError::Collection(err),
                ),
                Ok(None) => TimedDrainAction::AbortStderr(
                    take_ready_error_or_timeout(stderr_future.as_mut()).await,
                ),
            },
            stderr = &mut stderr_future => match stderr {
                Ok(Some(stderr)) => TimedDrainAction::Done(
                    drain_remaining_consumer(stdout_future.as_mut())
                        .await
                        .map(|stdout| (stdout, stderr)),
                ),
                Err(err) => TimedDrainAction::AbortStdout(
                    ConsumerDrainError::Collection(err),
                ),
                Ok(None) => TimedDrainAction::AbortStdout(
                    take_ready_error_or_timeout(stdout_future.as_mut()).await,
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

async fn drain_remaining_consumer<S, F>(
    mut remaining_wait: std::pin::Pin<&mut F>,
) -> Result<S, ConsumerDrainError>
where
    S: Sink,
    F: Future<Output = Result<Option<S>, ConsumerError>>,
{
    match remaining_wait.as_mut().await {
        Ok(Some(sink)) => Ok(sink),
        Err(err) => Err(ConsumerDrainError::Collection(err)),
        Ok(None) => Err(ConsumerDrainError::Timeout),
    }
}

/// Polls `remaining_wait` once. Returns `Collection(err)` if the future is immediately ready
/// with an error, otherwise returns `Timeout` without advancing the future further.
fn take_ready_error_or_timeout<S, F>(
    mut remaining_wait: std::pin::Pin<&mut F>,
) -> impl Future<Output = ConsumerDrainError> + '_
where
    S: Sink,
    F: Future<Output = Result<Option<S>, ConsumerError>>,
{
    poll_fn(move |cx| match remaining_wait.as_mut().poll(cx) {
        Poll::Ready(Err(err)) => Poll::Ready(ConsumerDrainError::Collection(err)),
        _ => Poll::Ready(ConsumerDrainError::Timeout),
    })
}

pub(crate) async fn wait_for_completion_with_collectors<Stdout, Stderr, StdoutSink, StderrSink>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    proc_timeout: Duration,
    eof_timeout: Duration,
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) -> Result<WaitForCompletionResult<ProcessOutput<StdoutSink, StderrSink>>, WaitWithOutputError>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let status = match handle.wait_for_completion_inner(proc_timeout).await? {
        WaitForCompletionResult::Completed(status) => status,
        WaitForCompletionResult::Timeout { timeout } => {
            let (_stdout, _stderr) =
                tokio::join!(stdout_collector.abort(), stderr_collector.abort());
            return Ok(WaitForCompletionResult::Timeout { timeout });
        }
    };
    let (stdout, stderr) =
        wait_for_output_consumers(stdout_collector, stderr_collector, eof_timeout)
            .await
            .map_err(|err| err.into_wait_with_output_error(handle.name.clone(), eof_timeout))?;
    Ok(WaitForCompletionResult::Completed(ProcessOutput {
        status,
        stdout,
        stderr,
    }))
}

#[cfg(any(unix, windows))]
pub(crate) async fn wait_for_completion_or_terminate_with_collectors<
    Stdout,
    Stderr,
    StdoutSink,
    StderrSink,
>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    wait_timeout: Duration,
    shutdown: GracefulShutdown,
    eof_timeout: Duration,
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) -> Result<
    WaitForCompletionOrTerminateResult<ProcessOutput<StdoutSink, StderrSink>>,
    WaitWithOutputError,
>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let outcome = handle
        .wait_for_completion_or_terminate_inner(wait_timeout, shutdown)
        .await?;
    let (stdout, stderr) =
        wait_for_output_consumers(stdout_collector, stderr_collector, eof_timeout)
            .await
            .map_err(|err| err.into_wait_with_output_error(handle.name.clone(), eof_timeout))?;
    Ok(outcome.map(|status| ProcessOutput {
        status,
        stdout,
        stderr,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::backend::broadcast::BroadcastOutputStream;
    use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::config::StreamConfig;
    use crate::test_support::ReadErrorAfterBytes;
    use crate::visitors::collect::CollectChunks;
    use crate::{
        CollectedBytes, Consumable, ConsumerError, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, RawCollectionOptions,
    };
    use assertr::prelude::*;
    use std::io;
    use unwrap_infallible::UnwrapInfallible;

    fn best_effort_no_replay_options()
    -> StreamConfig<crate::LossyWithoutBackpressure, crate::NoReplay> {
        StreamConfig::builder()
            .lossy_without_backpressure()
            .no_replay()
            .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
            .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            .build()
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
            wait_for_output_consumers(
                stdout_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                stderr_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("collector error should be returned before the outer timeout");

        match result {
            Err(ConsumerDrainError::Collection(ConsumerError::StreamRead { source })) => {
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
    async fn timed_output_collection_times_out_stderr_when_stdout_finishes_first() {
        // Stdout EOFs immediately (the writer half of the duplex is dropped right away), so
        // its collector completes inside the `tokio::select!` arm. Stderr's writer is held
        // open for the lifetime of the test, so its collector keeps waiting for data or EOF
        // that never arrive. The deadline elapses inside `drain_remaining_consumer`,
        // exercising the asymmetric-abort path that aborts the slower sibling once the budget
        // is gone.
        let (stdout_read, stdout_write) = tokio::io::duplex(64);
        drop(stdout_write);
        let stdout_stream = BroadcastOutputStream::from_stream(
            stdout_read,
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
            Duration::from_secs(2),
            wait_for_output_consumers(
                stdout_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                stderr_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                Duration::from_millis(150),
            ),
        )
        .await
        .expect("drain should resolve via deadline-driven timeout, not the outer fallback");

        match result {
            Err(ConsumerDrainError::Timeout) => {}
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected drain timeout when stderr never finishes, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn timed_output_collection_times_out_stdout_when_stderr_finishes_first() {
        // Mirror image of the previous test: stderr EOFs first, stdout keeps waiting. Verifies
        // the symmetric branch of the asymmetric-abort logic in `wait_for_output_consumers`.
        let (stdout_read, _stdout_write) = tokio::io::duplex(64);
        let stdout_stream = BroadcastOutputStream::from_stream(
            stdout_read,
            "stdout",
            best_effort_no_replay_options(),
        );
        let (stderr_read, stderr_write) = tokio::io::duplex(64);
        drop(stderr_write);
        let stderr_stream = BroadcastOutputStream::from_stream(
            stderr_read,
            "stderr",
            best_effort_no_replay_options(),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_output_consumers(
                stdout_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                stderr_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                Duration::from_millis(150),
            ),
        )
        .await
        .expect("drain should resolve via deadline-driven timeout, not the outer fallback");

        match result {
            Err(ConsumerDrainError::Timeout) => {}
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected drain timeout when stdout never finishes, got {other:?}"
                ));
            }
        }
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
            wait_for_output_consumers(
                stdout_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap_infallible(),
                stderr_stream
                    .consume(CollectChunks::fold(
                        CollectedBytes::new(),
                        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
                    ))
                    .unwrap(),
                Duration::from_secs(30),
            ),
        )
        .await
        .expect("collector error should be returned before the outer timeout");

        match result {
            Err(ConsumerDrainError::Collection(ConsumerError::StreamRead { source })) => {
                assert_that!(source.stream_name()).is_equal_to("stdout");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected early stream read error, got {other:?}"
                ));
            }
        }

        let collector = stderr_stream
            .consume(CollectChunks::fold(
                CollectedBytes::new(),
                CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
            ))
            .unwrap();
        let _collected = collector
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("collector should observe cancellation");
    }
}
