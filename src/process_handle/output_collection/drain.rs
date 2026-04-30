use crate::error::{
    WaitForCompletionOrTerminateResult, WaitForCompletionResult, WaitWithOutputError,
};
use crate::output_stream::OutputStream;
use crate::output_stream::consumer::{Consumer, ConsumerError, Sink};
use crate::process_handle::ProcessHandle;
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
pub(super) enum ConsumerDrainError {
    Timeout,
    Collection(ConsumerError),
}

enum TimedDrainAction<StdoutSink, StderrSink> {
    Done(Result<(StdoutSink, StderrSink), ConsumerDrainError>),
    AbortStdout(ConsumerDrainError),
    AbortStderr(ConsumerDrainError),
}

async fn drain_output_consumers<StdoutSink, StderrSink>(
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) -> Result<(StdoutSink, StderrSink), ConsumerError>
where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    tokio::try_join!(stdout_collector.wait(), stderr_collector.wait())
}

async fn abort_output_consumers<StdoutSink, StderrSink>(
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let (_stdout, _stderr) = tokio::join!(stdout_collector.abort(), stderr_collector.abort());
}

pub(super) async fn wait_for_output_consumers<StdoutSink, StderrSink>(
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
    deadline: Option<Instant>,
) -> Result<(StdoutSink, StderrSink), ConsumerDrainError>
where
    StdoutSink: Sink,
    StderrSink: Sink,
{
    match deadline {
        None => drain_output_consumers(stdout_collector, stderr_collector)
            .await
            .map_err(ConsumerDrainError::Collection),
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
                            drain_remaining_consumer(stderr_future.as_mut())
                                .await
                                .map(|stderr| (stdout, stderr)),
                        ),
                        Err(err) => TimedDrainAction::AbortStderr(
                            ConsumerDrainError::Collection(err),
                        ),
                        Ok(None) => TimedDrainAction::AbortStderr(
                            peek_drain_error(stderr_future.as_mut()).await,
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

fn peek_drain_error<S, F>(
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

fn map_output_collector_error(
    err: ConsumerDrainError,
    process_name: std::borrow::Cow<'static, str>,
    deadline: OperationDeadline,
) -> WaitWithOutputError {
    match err {
        ConsumerDrainError::Collection(source) => WaitWithOutputError::OutputCollectionFailed {
            process_name,
            source,
        },
        ConsumerDrainError::Timeout => WaitWithOutputError::OutputCollectionTimeout {
            process_name,
            timeout: deadline.timeout,
        },
    }
}

pub(super) async fn wait_for_completion_with_collectors<Stdout, Stderr, StdoutSink, StderrSink>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    timeout: Duration,
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) -> Result<WaitForCompletionResult<(ExitStatus, StdoutSink, StderrSink)>, WaitWithOutputError>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
    StdoutSink: Sink,
    StderrSink: Sink,
{
    let deadline = OperationDeadline::from_timeout(timeout);
    let status = match handle.wait_for_completion_inner(timeout).await? {
        WaitForCompletionResult::Completed(status) => status,
        WaitForCompletionResult::Timeout { timeout } => {
            abort_output_consumers(stdout_collector, stderr_collector).await;
            return Ok(WaitForCompletionResult::Timeout { timeout });
        }
    };
    let (stdout, stderr) =
        wait_for_output_consumers(stdout_collector, stderr_collector, Some(deadline.deadline))
            .await
            .map_err(|err| map_output_collector_error(err, handle.name.clone(), deadline))?;

    Ok(WaitForCompletionResult::Completed((status, stdout, stderr)))
}

pub(super) async fn wait_for_completion_or_terminate_with_collectors<
    Stdout,
    Stderr,
    StdoutSink,
    StderrSink,
>(
    handle: &mut ProcessHandle<Stdout, Stderr>,
    wait_timeout: Duration,
    interrupt_timeout: Duration,
    terminate_timeout: Duration,
    stdout_collector: Consumer<StdoutSink>,
    stderr_collector: Consumer<StderrSink>,
) -> Result<
    WaitForCompletionOrTerminateResult<(ExitStatus, StdoutSink, StderrSink)>,
    WaitWithOutputError,
>
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
        wait_for_output_consumers(stdout_collector, stderr_collector, Some(deadline.deadline))
            .await
            .map_err(|err| map_output_collector_error(err, handle.name.clone(), deadline))?;

    Ok(outcome.result.map(|status| (status, stdout, stderr)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::backend::broadcast::BroadcastOutputStream;
    use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::config::StreamConfig;
    use crate::test_support::ReadErrorAfterBytes;
    use crate::{
        ConsumerError, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, RawCollectionOptions,
    };
    use assertr::prelude::*;
    use std::io;

    fn best_effort_no_replay_options() -> StreamConfig<crate::BestEffortDelivery, crate::NoReplay> {
        StreamConfig::builder()
            .best_effort_delivery()
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
                stdout_stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded),
                stderr_stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded),
                Some(Instant::now() + Duration::from_secs(30)),
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
                stdout_stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded),
                stderr_stream
                    .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
                    .unwrap(),
                Some(Instant::now() + Duration::from_secs(30)),
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
            .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
            .unwrap();
        let _collected = collector
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("collector should observe cancellation");
    }
}
