//! Spawned-process handle, stdin lifecycle, and process state queries.

mod drop_guard;
pub(crate) mod output_collection;
mod replay;
mod spawn;
pub(crate) mod termination;
mod wait;

use crate::output_stream::OutputStream;
use crate::panic_on_drop::PanicOnDrop;
use crate::process_handle::termination::GracefulTimeouts;
use std::borrow::Cow;
use std::io;
use std::mem::ManuallyDrop;
use std::process::ExitStatus;
use std::time::Duration;
use tokio::process::Child;
use tokio::process::ChildStdin;

/// Options for waiting until a process exits, terminating it if waiting fails.
///
/// `graceful_timeouts` is a [`GracefulTimeouts`] value, whose shape itself differs per platform
/// (two timeouts on Unix, one on Windows). Cross-platform callers construct it under cfg gates;
/// the wait-or-terminate signature stays the same on every supported OS.
///
/// This type is only available on Unix and Windows because the underlying `terminate(...)`
/// machinery is gated to those platforms.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitForCompletionOrTerminateOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Per-platform graceful-shutdown timeout budget. See [`GracefulTimeouts`].
    pub graceful_timeouts: GracefulTimeouts,
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

    /// Reaping the child failed, so the actual state could not be observed.
    ///
    /// The conservative reading is "still running": until termination has been confirmed, the
    /// drop-cleanup and panic guards on the handle are still required, and the process may yet
    /// produce output. The variant carries the underlying [`io::Error`] so callers can log or
    /// retry at their discretion; treating it as "not running" risks dropping a still-live child.
    /// [`RunningState::is_definitely_running`] returns `false` here on purpose so callers cannot
    /// mistake "we don't know" for "still running" via a single boolean check, but matching on
    /// the enum is the only way to distinguish [`RunningState::Terminated`] from this case.
    Uncertain(io::Error),
}

impl RunningState {
    /// Returns `true` only when the state is [`RunningState::Running`].
    ///
    /// Both [`RunningState::Terminated`] and [`RunningState::Uncertain`] return `false`. The
    /// asymmetry is intentional: callers who need to distinguish "not running" from "we don't
    /// know" should match the enum directly. The error carried by
    /// [`RunningState::Uncertain`] is real and worth surfacing, not silently collapsing.
    ///
    /// Even when this returns `false` for an [`RunningState::Uncertain`] state, the safe
    /// interpretation is still "the process may be running"; do not use this predicate to decide
    /// whether it is safe to drop the handle. See [`RunningState::Uncertain`].
    #[must_use]
    pub fn is_definitely_running(&self) -> bool {
        matches!(self, RunningState::Running)
    }
}

/// Drop-time behavior selected by the lifecycle methods on [`ProcessHandle`].
///
/// The state machine has two reachable states because every public lifecycle entry point either
/// keeps both safeguards on (`Armed`) or turns both off (`Disarmed`). There is no "panic only,
/// no cleanup" or "cleanup only, no panic" combination: the panic guard makes sense only when
/// paired with the kill that signals the misuse.
#[derive(Debug)]
pub(super) enum DropMode {
    /// Cleanup is attempted on drop and the panic guard fires when it does.
    Armed { panic: PanicOnDrop },
    /// Both cleanup and the panic guard are off. Drop is a no-op for this handle's lifecycle.
    Disarmed,
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
    pub(super) drop_mode: DropMode,
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
    ///     AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process,
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

    /// Reaps the child's exit status without affecting the drop-cleanup or panic guards.
    ///
    /// Callers that observe an exit status are responsible for calling
    /// [`Self::must_not_be_terminated`] when they take ownership of the lifecycle: the wait,
    /// terminate, and kill paths do this explicitly when they succeed. `is_running` deliberately
    /// does not, so a status check never silently disarms the safeguards.
    pub(super) fn try_reap_exit_status(&mut self) -> Result<Option<ExitStatus>, io::Error> {
        self.child.try_wait()
    }

    /// Checks if the process is currently running.
    ///
    /// Returns [`RunningState::Running`] if the process is still running,
    /// [`RunningState::Terminated`] if it has exited, or [`RunningState::Uncertain`]
    /// if the state could not be determined. Each call re-runs the underlying
    /// `try_wait`, so a transient probing failure observed once does not become permanent.
    ///
    /// This is a pure status query: it does not disarm the drop-cleanup or panic guards, even
    /// when it observes that the process has exited. A handle whose status reads
    /// [`RunningState::Terminated`] still panics on drop until one of the lifecycle methods has
    /// closed it. Use [`Self::wait_for_completion`], [`Self::terminate`], or [`Self::kill`] to
    /// close the lifecycle through a successful terminal call, or call
    /// [`Self::must_not_be_terminated`] explicitly to detach the handle without termination.
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
            std::ptr::drop_in_place(&raw mut this.drop_mode);

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
        async fn does_not_disarm_drop_guards_when_process_has_exited() {
            let mut process = Process::new(long_running_command(Duration::from_millis(50)))
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

            tokio::time::sleep(Duration::from_millis(200)).await;

            // Observe the exit through the query API. Even with the child reaped, the drop
            // guards must remain armed because is_running is documented as a pure status query.
            let _state = process.is_running();

            assert_that!(process.is_drop_armed()).is_true();

            // Detach explicitly so the test does not panic when `process` drops.
            process.must_not_be_terminated();
        }

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
