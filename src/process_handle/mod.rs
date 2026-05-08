//! Spawned-process handle, stdin lifecycle, and process state queries.

mod drop_guard;
mod group;
pub(crate) mod output_collection;
mod replay;
#[cfg(any(unix, windows))]
mod signal;
mod spawn;
pub(crate) mod termination;
mod wait;
pub(crate) mod wait_builder;

use crate::output_stream::OutputStream;
use crate::process_handle::drop_guard::DropMode;
use crate::process_handle::group::ProcessGroup;
use std::borrow::Cow;
use std::io;
use std::mem::ManuallyDrop;
use std::process::ExitStatus;
use tokio::process::Child;
use tokio::process::ChildStdin;

/// Represents the `stdin` stream of a child process.
///
/// stdin is always configured as piped, so it starts as `Open` with a [`ChildStdin`] handle
/// that can be used to write data to the process. It can be explicitly closed by calling
/// [`Stdin::close`], or implicitly closed by the staged
/// [`ProcessHandle::wait_for_completion`] builder and [`ProcessHandle::kill`], after which it
/// transitions to the `Closed` state. Note that some operations (kill) close `stdin` early in
/// order to match the behavior of tokio.
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
    /// will be in the `Closed` state and no further writes will be possible. The staged
    /// [`ProcessHandle::wait_for_completion`] builder and [`ProcessHandle::kill`] also close
    /// any still-open stdin automatically; call this method when the child must observe EOF
    /// earlier.
    pub fn close(&mut self) {
        *self = Stdin::Closed;
    }
}

/// Represents the running state of a process.
#[derive(Debug)]
#[must_use = "Discarding the state defeats the purpose of probing; match on the variants \
              (or call `is_definitely_running`) to distinguish Running, Terminated, and \
              Uncertain."]
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

/// A handle to a spawned process with captured stdout/stderr streams.
///
/// This type provides methods for waiting on process completion, terminating the process,
/// and accessing its output streams. By default, processes must be explicitly waited on
/// or terminated before being dropped (see [`ProcessHandle::must_be_terminated`]).
///
/// If applicable, a process handle can be wrapped in a [`crate::TerminateOnDrop`] to be terminated
/// automatically upon being dropped. Note that this requires a multithreaded runtime!
#[derive(Debug)]
pub struct ProcessHandle<Stdout, Stderr = Stdout>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    pub(crate) name: Cow<'static, str>,

    /// Owns the spawned child as the leader of a process group. On Windows this also owns the
    /// Job Object the child has been assigned to, so the forceful-kill path can call
    /// `TerminateJobObject` and reach every descendant the OS has associated with the job
    /// instead of orphaning the tree the way `Child::start_kill` would.
    pub(super) group: ProcessGroup,

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
    /// Returns the process name configured at spawn time.
    ///
    /// The name is set by the [`Process`](crate::Process) builder via either an explicit string
    /// or an [`AutoName`](crate::AutoName) / [`AutoNameSettings`](crate::AutoNameSettings)
    /// derivation, and is used in diagnostics and error messages to identify the child.
    pub fn name(&self) -> &str {
        &self.name
    }

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
    ///             .lossy_without_backpressure()
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
    /// For [`BroadcastOutputStream`](crate::BroadcastOutputStream), this allows creating multiple
    /// concurrent consumers. For
    /// [`SingleSubscriberOutputStream`](crate::SingleSubscriberOutputStream), only one active
    /// consumer can exist at a time; concurrent attempts return
    /// [`StreamConsumerError::ActiveConsumer`](crate::StreamConsumerError::ActiveConsumer) from
    /// `consume(...)` / `consume_async(...)` / `wait_for_line(...)` rather than panicking.
    pub fn stdout(&self) -> &Stdout {
        &self.std_out_stream
    }

    /// Returns a reference to the stderr stream.
    ///
    /// For [`BroadcastOutputStream`](crate::BroadcastOutputStream), this allows creating multiple
    /// concurrent consumers. For
    /// [`SingleSubscriberOutputStream`](crate::SingleSubscriberOutputStream), only one active
    /// consumer can exist at a time; concurrent attempts return
    /// [`StreamConsumerError::ActiveConsumer`](crate::StreamConsumerError::ActiveConsumer) from
    /// `consume(...)` / `consume_async(...)` / `wait_for_line(...)` rather than panicking.
    pub fn stderr(&self) -> &Stderr {
        &self.std_err_stream
    }
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Forwards to [`ProcessGroup::send_kill`].
    ///
    /// Kept as a single named entry point so its callers in [`drop_guard`](super::drop_guard)
    /// and [`termination`](super::termination) read consistently as "kill via the handle"
    /// rather than reaching into the wrapped [`ProcessGroup`] directly.
    pub(super) fn send_kill_signal(&mut self) -> io::Result<()> {
        self.group.send_kill()
    }

    /// Returns the OS process ID if the process hasn't exited yet.
    ///
    /// Once this process has been polled to completion this will return None.
    pub fn id(&self) -> Option<u32> {
        self.group.id()
    }

    /// Reaps the child's exit status without affecting the drop-cleanup or panic guards.
    ///
    /// Callers that observe an exit status are responsible for calling
    /// [`Self::must_not_be_terminated`] when they take ownership of the lifecycle: the wait,
    /// terminate, and kill paths do this explicitly when they succeed. `is_running` deliberately
    /// does not, so a status check never silently disarms the safeguards.
    ///
    /// Exposed publicly so callers can wire this as the preflight reaper to
    /// [`Self::terminate_with_hooks`].
    #[doc(hidden)]
    pub fn try_reap_exit_status(&mut self) -> Result<Option<ExitStatus>, io::Error> {
        self.group.try_wait()
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
    /// closed it. Use [`Self::wait_for_completion`] (the staged builder), [`Self::terminate`],
    /// or [`Self::kill`] to close the lifecycle through a successful terminal call, or call
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
        // not returned from this function exactly once before returning the owned parts. The
        // `ProcessGroup` is consumed by `into_leader()` to produce the bare Tokio `Child`; on
        // Windows this drops the Job Object handle, which only closes the kernel handle (no
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set) and does not kill the child.
        unsafe {
            let group = std::ptr::read(&raw const this.group);
            let stdin = std::ptr::read(&raw const this.std_in);
            let stdout = std::ptr::read(&raw const this.std_out_stream);
            let stderr = std::ptr::read(&raw const this.std_err_stream);

            std::ptr::drop_in_place(&raw mut this.name);
            std::ptr::drop_in_place(&raw mut this.drop_mode);

            (group.into_leader(), stdin, stdout, stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

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
