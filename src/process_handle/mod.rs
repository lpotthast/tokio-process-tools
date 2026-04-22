mod drop_guard;
mod into_inner;
mod options;
mod output_collection;
mod replay;
mod spawn;
mod state;
mod termination;
mod wait;

use crate::output_stream::OutputStream;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::panic_on_drop::PanicOnDrop;
use std::borrow::Cow;
use std::io;
use std::process::ExitStatus;
use tokio::process::Child;
use tokio::process::ChildStdin;

pub use options::{
    LineOutputOptions, RawOutputOptions, WaitForCompletionOptions,
    WaitForCompletionOrTerminateOptions, WaitForCompletionOrTerminateWithOutputOptions,
    WaitForCompletionOrTerminateWithRawOutputOptions,
    WaitForCompletionOrTerminateWithTrustedLineOutputOptions, WaitForCompletionWithOutputOptions,
    WaitForCompletionWithRawOutputOptions, WaitForCompletionWithTrustedLineOutputOptions,
};

/// Process handle returned by broadcast spawning.
pub type BroadcastProcessHandle<StdoutD, StdoutR, StderrD = StdoutD, StderrR = StdoutR> =
    ProcessHandle<BroadcastOutputStream<StdoutD, StdoutR>, BroadcastOutputStream<StderrD, StderrR>>;

/// Represents the stdin stream of a child process.
///
/// stdin is always configured as piped, so it starts as `Open` with a [`ChildStdin`] handle
/// that can be used to write data to the process. It can be explicitly closed by calling
/// [`Stdin::close`], after which it transitions to the `Closed` state.
#[derive(Debug)]
pub enum Stdin {
    /// stdin is open and available for writing.
    Open(ChildStdin),
    /// stdin has been closed.
    Closed,
}

impl Stdin {
    /// Returns `true` if stdin is open and available for writing.
    #[must_use]
    pub fn is_open(&self) -> bool {
        matches!(self, Stdin::Open(_))
    }

    /// Returns a mutable reference to the underlying [`ChildStdin`] if open, or `None` if closed.
    pub fn as_mut(&mut self) -> Option<&mut ChildStdin> {
        match self {
            Stdin::Open(stdin) => Some(stdin),
            Stdin::Closed => None,
        }
    }

    /// Closes stdin by dropping the underlying [`ChildStdin`] handle.
    ///
    /// This sends `EOF` to the child process. After calling this method, this stdin
    /// will be in the `Closed` state and no further writes will be possible.
    pub fn close(&mut self) {
        *self = Stdin::Closed;
    }
}

/// Represents the running state of a process.
#[derive(Debug)]
pub enum RunningState {
    /// The process is still running.
    Running,

    /// The process has terminated with the given exit status.
    Terminated(ExitStatus),

    /// Failed to determine process state.
    Uncertain(io::Error),
}

impl RunningState {
    /// Returns `true` if the process is running, `false` otherwise.
    #[must_use]
    pub fn as_bool(&self) -> bool {
        matches!(self, RunningState::Running)
    }
}

impl From<RunningState> for bool {
    fn from(is_running: RunningState) -> Self {
        is_running.as_bool()
    }
}

/// A handle to a spawned process with captured stdout/stderr streams.
///
/// This type provides methods for waiting on process completion, terminating the process,
/// and accessing its output streams. By default, processes must be explicitly waited on
/// or terminated before being dropped (see [`ProcessHandle::must_be_terminated`]).
///
/// If applicable, a process handle can be wrapped in a [`crate::TerminateOnDrop`] to be terminated
/// automatically upon being dropped. Note that this requires a multi-threaded runtime!
#[derive(Debug)]
pub struct ProcessHandle<Stdout: OutputStream, Stderr: OutputStream = Stdout> {
    pub(crate) name: Cow<'static, str>,
    child: Child,
    std_in: Stdin,
    std_out_stream: Stdout,
    std_err_stream: Stderr,
    cleanup_on_drop: bool,
    panic_on_drop: Option<PanicOnDrop>,
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Returns a mutable reference to the (potentially already closed) stdin stream.
    ///
    /// Use this to write data to the child process's stdin. The stdin stream implements
    /// [`tokio::io::AsyncWrite`], allowing you to use methods like `write_all()` and `flush()`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use tokio::process::Command;
    /// # use tokio_process_tools::{
    ///     AsyncChunkCollector, AsyncLineCollector, AutoNameSettings, Chunk, Collector,
    ///     DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, DeliveryGuarantee, LineCollectionOptions,
    ///     LineOverflowBehavior, LineParsingOptions, LineWriteMode, Next, NumBytesExt, Process,
    ///     ProcessHandle, ProcessOutput, RawCollectionOptions, Sink,
    ///     StreamConfig, TerminateOnDrop, WaitForLineResult,
    /// };
    /// # use tokio::io::AsyncWriteExt;
    /// # tokio_test::block_on(async {
    /// // The stream backend does not make a difference here.
    /// let mut process = Process::new(Command::new("cat"))
    ///     .auto_name()
    ///     .stdout_and_stderr(|stream| {
    ///         stream
    ///             .broadcast()
    ///             .best_effort_delivery()
    ///             .no_replay()
    ///             .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
    ///             .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
    ///     })
    ///     .spawn()
    ///     .unwrap();
    ///
    /// // Write to stdin.
    /// if let Some(stdin) = process.stdin().as_mut() {
    ///     stdin.write_all(b"Hello, process!\n").await.unwrap();
    ///     stdin.flush().await.unwrap();
    /// }
    ///
    /// // Close stdin to signal EOF.
    /// process.stdin().close();
    /// # });
    /// ```
    pub fn stdin(&mut self) -> &mut Stdin {
        &mut self.std_in
    }

    /// Returns a reference to the stdout stream.
    ///
    /// For `BroadcastOutputStream`, this allows creating multiple concurrent consumers.
    /// For `SingleSubscriberOutputStream`, only one active consumer can exist at a time
    /// (concurrent attempts will panic with a helpful error message).
    pub fn stdout(&self) -> &Stdout {
        &self.std_out_stream
    }

    /// Returns a reference to the stderr stream.
    ///
    /// For `BroadcastOutputStream`, this allows creating multiple concurrent consumers.
    /// For `SingleSubscriberOutputStream`, only one active consumer can exist at a time
    /// (concurrent attempts will panic with a helpful error message).
    pub fn stderr(&self) -> &Stderr {
        &self.std_err_stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
        LineCollectionOptions, LineOverflowBehavior, LineParsingOptions, NumBytesExt,
    };
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

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
    async fn test_stdin_write_and_read() {
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

        assert_that!(process.stdin().is_open()).is_true();

        let test_data = b"Hello from stdin!\n";
        if let Some(stdin) = process.stdin().as_mut() {
            stdin.write_all(test_data).await.unwrap();
            stdin.flush().await.unwrap();
        }

        process.stdin().close();
        assert_that!(process.stdin().is_open()).is_false();

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().len()).is_equal_to(1);
        assert_that!(output.stdout[0].as_str()).is_equal_to("Hello from stdin!");
    }

    #[tokio::test]
    async fn test_stdin_close_sends_eof() {
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

        process.stdin().close();
        assert_that!(process.stdin().is_open()).is_false();

        let status = process
            .wait_for_completion(wait_options(Some(Duration::from_secs(2))))
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
    }

    #[tokio::test]
    async fn test_stdin_multiple_writes() {
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

        if let Some(stdin) = process.stdin().as_mut() {
            stdin.write_all(b"Line 1\n").await.unwrap();
            stdin.write_all(b"Line 2\n").await.unwrap();
            stdin.write_all(b"Line 3\n").await.unwrap();
            stdin.flush().await.unwrap();
        }

        process.stdin().close();

        let output = process
            .wait_for_completion_with_output(wait_with_line_output_options(Some(
                Duration::from_secs(2),
            )))
            .await
            .unwrap();

        assert_that!(output.stdout.lines().len()).is_equal_to(3);
        assert_that!(output.stdout[0].as_str()).is_equal_to("Line 1");
        assert_that!(output.stdout[1].as_str()).is_equal_to("Line 2");
        assert_that!(output.stdout[2].as_str()).is_equal_to("Line 3");
    }

    #[tokio::test]
    async fn test_shell_command_dispatch() {
        let cmd = tokio::process::Command::new("sh");

        let mut process = crate::Process::new(cmd)
            .auto_name()
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

        let collector = process
            .stdout()
            .collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

        if let Some(stdin) = process.stdin().as_mut() {
            stdin
                .write_all(b"printf 'Hello from shell\\n'\nexit\n")
                .await
                .unwrap();
            stdin.flush().await.unwrap();
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        process.stdin().close();
        process
            .wait_for_completion(wait_options(Some(Duration::from_secs(1))))
            .await
            .unwrap();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().len()).is_equal_to(1);
        assert_that!(collected[0].as_str()).is_equal_to("Hello from shell");
    }
}
