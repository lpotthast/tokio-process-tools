mod drop_guard;
mod into_inner;
pub(crate) mod options;
pub(crate) mod output_collection;
mod replay;
mod spawn;
mod state;
mod termination;
mod wait;

use crate::output_stream::OutputStream;
use crate::panic_on_drop::PanicOnDrop;
use std::borrow::Cow;
use std::io;
use std::process::ExitStatus;
use tokio::process::Child;
use tokio::process::ChildStdin;

/// Represents the `stdin` stream of a child process.
///
/// stdin is always configured as piped, so it starts as `Open` with a [`ChildStdin`] handle
/// that can be used to write data to the process. It can be explicitly closed by calling
/// [`Stdin::close`], or implicitly closed by terminal wait helpers such as
/// [`ProcessHandle::wait_for_completion`] and [`ProcessHandle::kill`], after which it transitions
/// to the `Closed` state. Note that some operations (kill) close `stdin` early in order to match
/// the behavior of tokio.
#[derive(Debug)]
pub enum Stdin {
    /// `stdin` is open and available for writing.
    Open(ChildStdin),

    /// `stdin` has been closed.
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
    /// will be in the `Closed` state and no further writes will be possible. Terminal wait helpers
    /// such as [`ProcessHandle::wait_for_completion`] and [`ProcessHandle::kill`] also close any
    /// still-open stdin automatically; call this method when the child must observe EOF earlier.
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
pub struct ProcessHandle<Stdout, Stderr = Stdout>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
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
    ///     AsyncChunkCollector, AsyncLineCollector, AutoName, Chunk, Collector,
    ///     DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, DeliveryGuarantee, LineCollectionOptions,
    ///     LineOverflowBehavior, LineParsingOptions, LineWriteMode, Next, NumBytesExt, Process,
    ///     ProcessHandle, ProcessOutput, RawCollectionOptions, Sink,
    ///     StreamConfig, TerminateOnDrop, WaitForLineResult,
    /// };
    /// # use tokio::io::AsyncWriteExt;
    /// # tokio_test::block_on(async {
    /// // The stream backend does not make a difference here.
    /// let mut process = Process::new(Command::new("cat"))
    ///     .name(AutoName::program_only())
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
    use crate::test_support::long_running_command;
    use assertr::prelude::*;
    use std::process::Stdio;
    use std::time::Duration;

    #[tokio::test]
    async fn open_stdin_reports_open_until_closed() {
        let mut child = long_running_command(Duration::from_secs(5))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let child_stdin = child.stdin.take().unwrap();
        let mut stdin = Stdin::Open(child_stdin);

        assert_that!(stdin.is_open()).is_true();
        assert_that!(stdin.as_mut().is_some()).is_true();

        stdin.close();

        assert_that!(stdin.is_open()).is_false();
        assert_that!(stdin.as_mut()).is_none();

        child.kill().await.unwrap();
    }

    #[test]
    fn closed_stdin_reports_closed() {
        let mut stdin = Stdin::Closed;

        assert_that!(stdin.is_open()).is_false();
        assert_that!(stdin.as_mut()).is_none();

        stdin.close();

        assert_that!(stdin.is_open()).is_false();
        assert_that!(stdin.as_mut()).is_none();
    }
}
