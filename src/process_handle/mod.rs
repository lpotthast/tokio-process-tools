//! Spawned-process handle, stdin lifecycle, and process state queries.

mod drop_guard;
pub(crate) mod output_collection;
mod replay;
mod spawn;
mod termination;
mod wait;

use crate::output_stream::OutputStream;
use crate::panic_on_drop::PanicOnDrop;
use std::borrow::Cow;
use std::io;
use std::mem::ManuallyDrop;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Child;
use tokio::process::ChildStdin;

/// Options for waiting until a process exits, terminating it if waiting fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitForCompletionOrTerminateOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,
}

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
    ///     AsyncChunkCollector, AsyncLineCollector, AutoName, Chunk, Consumer,
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

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Returns the OS process ID if the process hasn't exited yet.
    ///
    /// Once this process has been polled to completion this will return None.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(super) fn try_reap_exit_status(&mut self) -> Result<Option<ExitStatus>, io::Error> {
        match self.child.try_wait() {
            Ok(Some(exit_status)) => {
                self.must_not_be_terminated();
                Ok(Some(exit_status))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Checks if the process is currently running.
    ///
    /// Returns [`RunningState::Running`] if the process is still running,
    /// [`RunningState::Terminated`] if it has exited, or [`RunningState::Uncertain`]
    /// if the state could not be determined.
    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> RunningState {
        match self.try_reap_exit_status() {
            Ok(None) => RunningState::Running,
            Ok(Some(exit_status)) => RunningState::Terminated(exit_status),
            Err(err) => RunningState::Uncertain(err),
        }
    }
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Consumes this handle to provide the wrapped `tokio::process::Child` instance, stdin handle,
    /// and stdout/stderr output streams.
    ///
    /// The returned [`Child`] no longer owns its `stdin` field because this crate separates piped
    /// stdin into [`Stdin`]. Keep the returned [`Stdin`] alive to keep the child's stdin pipe open.
    /// Dropping [`Stdin::Open`] closes the pipe, so the child may observe EOF and exit or otherwise
    /// change behavior.
    pub fn into_inner(mut self) -> (Child, Stdin, Stdout, Stderr) {
        self.must_not_be_terminated();
        let mut this = ManuallyDrop::new(self);

        // SAFETY: `this` is wrapped in `ManuallyDrop`, so moving out fields with `ptr::read` will
        // not cause the original `ProcessHandle` destructor to run. We explicitly drop the fields
        // not returned from this function exactly once before returning the owned parts.
        unsafe {
            let child = std::ptr::read(&raw const this.child);
            let stdin = std::ptr::read(&raw const this.std_in);
            let stdout = std::ptr::read(&raw const this.std_out_stream);
            let stderr = std::ptr::read(&raw const this.std_err_stream);

            std::ptr::drop_in_place(&raw mut this.name);
            std::ptr::drop_in_place(&raw mut this.panic_on_drop);

            (child, stdin, stdout, stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process, RunningState,
        test_support::long_running_command,
    };
    use assertr::prelude::*;
    use std::process::Stdio;
    use std::time::Duration;

    mod stdin {
        use super::*;

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

    mod is_running {
        use super::*;

        #[tokio::test]
        async fn reports_running_before_wait_and_terminated_after_wait() {
            let mut process = Process::new(long_running_command(Duration::from_secs(1)))
                .name(AutoName::program_only())
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

            match process.is_running() {
                RunningState::Running => {}
                RunningState::Terminated(exit_status) => {
                    assert_that!(exit_status).fail("process should still be running");
                }
                RunningState::Uncertain(_) => {
                    assert_that!(&process).fail("process state should not be uncertain");
                }
            }

            process
                .wait_for_completion(Duration::from_secs(2))
                .await
                .unwrap();

            match process.is_running() {
                RunningState::Running => {
                    assert_that!(process).fail("process should not be running anymore");
                }
                RunningState::Terminated(exit_status) => {
                    assert_that!(exit_status.code()).is_some().is_equal_to(0);
                    assert_that!(exit_status.success()).is_true();
                }
                RunningState::Uncertain(_) => {
                    assert_that!(process).fail("process state should not be uncertain");
                }
            }
        }
    }

    #[cfg(test)]
    mod into_inner {
        use super::*;
        use crate::LineParsingOptions;
        use crate::test_support::line_collection_options;
        use tokio::io::AsyncWriteExt;

        #[tokio::test]
        async fn returns_stdin_with_pipe_still_open() {
            let cmd = tokio::process::Command::new("cat");
            let process = Process::new(cmd)
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

            let (mut child, mut stdin, stdout, _stderr) = process.into_inner();
            assert_that!(child.stdin.is_none()).is_true();

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_that!(child.try_wait().unwrap().is_none()).is_true();

            let collector = stdout
                .collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

            let Some(stdin_handle) = stdin.as_mut() else {
                assert_that!(stdin.is_open()).fail("stdin should be returned open");
                return;
            };
            stdin_handle
                .write_all(b"stdin stayed open\n")
                .await
                .unwrap();
            stdin_handle.flush().await.unwrap();

            stdin.close();

            let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert_that!(status.success()).is_true();

            let collected = collector.wait().await.unwrap();
            assert_that!(collected.lines().len()).is_equal_to(1);
            assert_that!(collected[0].as_str()).is_equal_to("stdin stayed open");
        }

        #[tokio::test]
        async fn defuses_panic_guard() {
            let process = Process::new(long_running_command(Duration::from_secs(5)))
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

            let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
            child.kill().await.unwrap();
            let _status = child.wait().await.unwrap();
        }

        #[tokio::test]
        async fn supports_handles_built_with_owned_name() {
            let process = Process::new(long_running_command(Duration::from_secs(5)))
                .name(format!("sleeper-{}", 7))
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

            let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
            child.kill().await.unwrap();
            let _status = child.wait().await.unwrap();
        }
    }
}
