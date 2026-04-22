//!
#![doc = include_str!("../README.md")]
//!

mod async_drop;
mod collector;
mod error;
mod inspector;
mod output;
mod output_stream;
mod panic_on_drop;
mod process;
mod process_handle;
mod signal;
mod terminate_on_drop;

pub use collector::{AsyncChunkCollector, AsyncLineCollector, Collector, CollectorError, Sink};
pub use error::{
    SpawnError, StreamReadError, TerminationError, WaitError, WaitForCompletionWithOutputError,
    WaitForCompletionWithOutputOrTerminateError, WaitForLineResult, WaitOrTerminateError,
};
pub use inspector::{Inspector, InspectorError};
pub use output::ProcessOutput;
pub use output_stream::backend::{broadcast, single_subscriber};
pub use output_stream::collection::{
    CollectedBytes, CollectedLines, CollectionOverflowBehavior, FailOnSinkWriteError,
    LineCollectionOptions, LogAndContinueSinkWriteErrors, RawCollectionOptions, SinkWriteError,
    SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation, WriteCollectionOptions,
};
pub use output_stream::config::{
    StreamConfig, StreamConfigBuilder, StreamConfigMaxBufferedChunksBuilder,
    StreamConfigReadChunkSizeBuilder, StreamConfigReadyBuilder, StreamConfigReplayBuilder,
};
pub use output_stream::line::{LineOverflowBehavior, LineParsingOptions, LineWriteMode};
pub use output_stream::options::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytes, NumBytesExt,
};
pub use output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, ReliableDelivery, Replay,
    ReplayEnabled, ReplayRetention,
};
pub use output_stream::{Chunk, Next, OutputStream};
pub use process::{
    AutoName, AutoNameSettings, ConfiguredProcessBuilder, NamedProcess, Process,
    ProcessBuilderWithStderr, ProcessBuilderWithStdout, ProcessName, ProcessStreamBuilder,
    ProcessStreamConfig,
};
pub use process_handle::{
    BroadcastProcessHandle, LineOutputOptions, ProcessHandle, RawOutputOptions, RunningState,
    Stdin, WaitForCompletionOptions, WaitForCompletionOrTerminateOptions,
    WaitForCompletionOrTerminateWithOutputOptions,
    WaitForCompletionOrTerminateWithRawOutputOptions,
    WaitForCompletionOrTerminateWithTrustedLineOutputOptions, WaitForCompletionWithOutputOptions,
    WaitForCompletionWithRawOutputOptions, WaitForCompletionWithTrustedLineOutputOptions,
};
pub use terminate_on_drop::TerminateOnDrop;

#[allow(dead_code)]
trait SendSync: Send + Sync {}
impl<D, R> SendSync for broadcast::BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
}
impl SendSync for single_subscriber::SingleSubscriberOutputStream {}
impl<Stdout, Stderr> SendSync for ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream + SendSync,
    Stderr: OutputStream + SendSync,
{
}

#[cfg(test)]
mod test {
    use crate::output::ProcessOutput;
    use crate::{
        AutoName, AutoNameSettings, CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, LineCollectionOptions, LineOutputOptions, LineOverflowBehavior,
        LineParsingOptions, Next, NumBytesExt, Process, RunningState, WaitForCompletionOptions,
        WaitForCompletionWithOutputOptions,
    };
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::process::Command;

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

    fn wait_with_line_output_options(
        timeout: Option<Duration>,
    ) -> WaitForCompletionWithOutputOptions {
        WaitForCompletionWithOutputOptions::builder()
            .timeout(timeout)
            .line_output_options(line_output_options())
            .build()
    }

    #[tokio::test]
    async fn wait_with_output() {
        let mut process = Process::new(Command::new("ls"))
            .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn `ls` command");
        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(wait_with_line_output_options(None))
            .await
            .unwrap();
        assert_that!(status.success()).is_true();
        for expected in [
            "Cargo.lock",
            "Cargo.toml",
            "LICENSE-APACHE",
            "LICENSE-MIT",
            "README.md",
            "src",
            "target",
        ] {
            assert_that!(stdout.lines().iter().any(|entry| entry == expected))
                .with_detail_message(format!(
                    "expected ls output to contain {expected:?}, got {stdout:?}"
                ))
                .is_true();
        }
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn single_subscriber_panics_on_multiple_consumers() {
        let mut process = Process::new(Command::new("ls"))
            .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn `ls` command");

        let _inspector = process
            .stdout()
            .inspect_lines(|_line| Next::Continue, LineParsingOptions::default());

        assert_that_panic_by(|| {
            let _inspector = process
                .stdout()
                .inspect_lines(|_line| Next::Continue, LineParsingOptions::default());
        })
        .has_type::<String>()
        .is_equal_to("Cannot create multiple active consumers on SingleSubscriberOutputStream (stream: 'stdout'). Only one active inspector, collector, or line waiter can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.");

        process
            .wait_for_completion(wait_options(None))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn is_running() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1");
        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn `sleep` command");

        match process.is_running() {
            RunningState::Running => {}
            RunningState::Terminated(exit_status) => {
                assert_that!(exit_status).fail("Process should be running");
            }
            RunningState::Uncertain(_) => {
                assert_that!(&process).fail("Process state should not be uncertain");
            }
        }

        let _exit_status = process
            .wait_for_completion(wait_options(None))
            .await
            .unwrap();

        match process.is_running() {
            RunningState::Running => {
                assert_that!(process).fail("Process should not be running anymore");
            }
            RunningState::Terminated(exit_status) => {
                assert_that!(exit_status.code()).is_some().is_equal_to(0);
                assert_that!(exit_status.success()).is_true();
            }
            RunningState::Uncertain(_) => {
                assert_that!(process).fail("Process state should not be uncertain");
            }
        }
    }

    #[tokio::test]
    async fn terminate() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1000");
        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn `sleep` command");
        process
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();
        match process.is_running() {
            RunningState::Running => {
                assert_that!(process).fail("Process should not be running anymore");
            }
            RunningState::Terminated(exit_status) => {
                // Terminating a process with a signal results in no code being emitted (on linux).
                assert_that!(exit_status.code()).is_none();
                assert_that!(exit_status.success()).is_false();
            }
            RunningState::Uncertain(_) => {
                assert_that!(process).fail("Process state should not be uncertain");
            }
        }
    }
}
