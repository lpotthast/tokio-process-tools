//! Helpers used by the staged [`crate::WaitForCompletion`] builder to drain stdout/stderr
//! alongside process exit. The builder lives in `process_handle::wait_builder`; this module
//! owns the supporting collector spawn and the timed drain orchestration.

pub(crate) mod drain;
pub(crate) mod options;

use super::ProcessHandle;
use crate::error::WaitWithOutputError;
use crate::output_stream::consumer::{Consumer, Sink, spawn_consumer_sync};
use crate::output_stream::event::Chunk;
use crate::output_stream::line::adapter::ParseLines;
use crate::output_stream::line::options::LineParsingOptions;
use crate::output_stream::visitors::collect::CollectChunks;
use crate::output_stream::{Next, Subscription};
use crate::{
    CollectedBytes, CollectedLines, LineCollectionOptions, OutputStream, RawCollectionOptions,
    Subscribable,
};
use std::borrow::Cow;
use std::process::ExitStatus;

/// Full output of a process that terminated.
///
/// `Stdout` and `Stderr` describe the collected payload type for each stream. For example,
/// line collection uses `ProcessOutput<CollectedLines>` and raw byte collection uses
/// `ProcessOutput<CollectedBytes>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput<Stdout, Stderr = Stdout> {
    /// Status the process exited with.
    pub status: ExitStatus,

    /// The process's collected output on its `stdout` stream.
    pub stdout: Stdout,

    /// The process's collected output on its `stderr` stream.
    pub stderr: Stderr,
}

/// Spawn a consumer that collects line output into a [`CollectedLines`] sink. Used by the
/// staged builder paths that take a `Subscribable` and can't call backend-specific methods.
pub(crate) fn spawn_line_collector<S>(
    stream_name: &'static str,
    subscription: S,
    parsing_options: LineParsingOptions,
    collection_options: LineCollectionOptions,
) -> Consumer<CollectedLines>
where
    S: Subscription,
{
    spawn_consumer_sync(
        stream_name,
        subscription,
        ParseLines::collect(
            parsing_options,
            CollectedLines::new(),
            move |line: Cow<'_, str>, sink: &mut CollectedLines| {
                sink.push_line(line.into_owned(), collection_options);
                Next::Continue
            },
        ),
    )
}

/// Spawn a consumer that collects raw byte output into a [`CollectedBytes`] sink.
pub(crate) fn spawn_chunk_collector<S>(
    stream_name: &'static str,
    subscription: S,
    options: RawCollectionOptions,
) -> Consumer<CollectedBytes>
where
    S: Subscription,
{
    spawn_consumer_sync(
        stream_name,
        subscription,
        CollectChunks::builder()
            .sink(CollectedBytes::new())
            .f(move |chunk: Chunk, sink: &mut CollectedBytes| {
                sink.push_chunk(chunk.as_ref(), options);
            })
            .build(),
    )
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream + Subscribable,
    Stderr: OutputStream + Subscribable,
{
    /// Subscribes to stdout and stderr, spawning a collector on each via the caller-supplied
    /// factories. If the stderr subscription fails after the stdout collector has started, the
    /// stdout collector is aborted (awaiting task join) before returning, so the stdout slot is
    /// released by the time the error reaches the caller. `Drop` alone is not sufficient here:
    /// for `SingleSubscriberOutputStream`, the consumer claim is only fully released after the
    /// task has been joined.
    pub(crate) async fn try_spawn_output_collectors<StdoutSink, StderrSink, FOut, FErr>(
        &mut self,
        spawn_stdout: FOut,
        spawn_stderr: FErr,
    ) -> Result<(Consumer<StdoutSink>, Consumer<StderrSink>), WaitWithOutputError>
    where
        StdoutSink: Sink,
        StderrSink: Sink,
        FOut: FnOnce(&'static str, Stdout::Subscription) -> Consumer<StdoutSink>,
        FErr: FnOnce(&'static str, Stderr::Subscription) -> Consumer<StderrSink>,
    {
        let out_subscription = self.stdout().try_subscribe().map_err(|source| {
            WaitWithOutputError::OutputCollectionStartFailed {
                process_name: self.name.clone(),
                source: Box::new(source),
            }
        })?;
        let out_collector = spawn_stdout(self.stdout().name(), out_subscription);

        let err_subscription = match self.stderr().try_subscribe() {
            Ok(subscription) => subscription,
            Err(source) => {
                out_collector.abort().await;
                return Err(WaitWithOutputError::OutputCollectionStartFailed {
                    process_name: self.name.clone(),
                    source: Box::new(source),
                });
            }
        };
        let err_collector = spawn_stderr(self.stderr().name(), err_subscription);

        Ok((out_collector, err_collector))
    }
}
