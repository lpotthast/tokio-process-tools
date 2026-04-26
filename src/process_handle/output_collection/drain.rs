use crate::collector::{Collector, CollectorError, Sink};
use crate::error::{WaitForCompletionWithOutputError, WaitForCompletionWithOutputOrTerminateError};
use crate::output_stream::OutputStream;
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
pub(super) enum CollectorDrainError {
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

pub(super) async fn wait_for_output_collectors<StdoutSink, StderrSink>(
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

pub(super) async fn wait_for_completion_with_collectors<Stdout, Stderr, StdoutSink, StderrSink>(
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
