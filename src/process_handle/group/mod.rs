//! Process-group abstraction so termination can target the entire spawned subtree.
//!
//! On Unix this is a literal POSIX process group (`setpgid` at spawn time, `killpg` at teardown).
//! On Windows it has two complementary pieces: a console process group (`CREATE_NEW_PROCESS_GROUP`)
//! that lets [`ProcessGroup::send_ctrl_break`] deliver `CTRL_BREAK_EVENT` to the whole console
//! group, and a Job Object that lets the forceful-kill path call `TerminateJobObject` to take
//! down the entire tree.
//!
//! [`prepare_command`] is called *before* spawning the child to install the platform's
//! group-leader configuration on the [`tokio::process::Command`]. After spawn, the resulting
//! [`tokio::process::Child`] is wrapped in a [`ProcessGroup`] which owns the leader plus, on
//! Windows, the Job Object that captures the leader's descendants. All named-signal delivery
//! (`SIGINT` / `SIGTERM` / `CTRL_BREAK_EVENT` / `SIGKILL`) goes through methods on
//! [`ProcessGroup`].

#[cfg(windows)]
mod job_object;

#[cfg(windows)]
use job_object::JobObject;
use std::io;
use std::process::ExitStatus;

/// Configures `command` so the spawned child becomes the leader of a fresh process group on the
/// current platform.
///
/// Without this setup the child's grandchildren (e.g. a shell wrapper that fork-execs the real
/// binary, or a build tool that fans out workers) would be reparented to init/system on the
/// leader's exit and outlive the [`crate::ProcessHandle`]. The [`ProcessGroup`] signal senders,
/// the `kill()` path, and the armed-Drop cleanup all rely on this so they can target the group
/// instead of just the leader's PID.
///
/// - **Unix:** sets the child's process group to itself (`setpgid(0, 0)` via Tokio's
///   `Command::process_group(0)`), so the child's PID is also its PGID and the group is
///   addressable as `-PGID` in `kill(2)`.
/// - **Windows:** passes `CREATE_NEW_PROCESS_GROUP`, so `GenerateConsoleCtrlEvent` with the
///   child's PID as the process-group ID delivers `CTRL_BREAK_EVENT` to every process in the
///   group. The forceful-kill path is handled separately by attaching the spawned child to a
///   Job Object inside [`ProcessGroup::from_spawned_child`].
/// - **Other platforms:** no-op. The crate's termination APIs are gated to Unix and Windows, so
///   spawning still works on other platforms but the resulting child is not set up for
///   group-targeted signal delivery.
pub(crate) fn prepare_command(
    command: &mut tokio::process::Command,
) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP)
    }
    #[cfg(unix)]
    {
        // `0` asks the child to be the leader of a new process group whose PGID equals the
        // child's PID. Termination signals can then target `-PGID` to reach the whole tree.
        command.process_group(0)
    }
    #[cfg(all(not(windows), not(unix)))]
    {
        command
    }
}

/// Owns a freshly spawned child and the platform's group-management primitive that reaches its
/// descendants on termination.
///
/// On Unix this is purely a wrapper around the Tokio [`tokio::process::Child`]; the child's PID
/// also serves as the PGID (set by [`prepare_command`] at spawn time), so the signal senders
/// route through `killpg`. On Windows it additionally owns an anonymous Job Object the child
/// has been assigned to, so `TerminateJobObject` can take down the entire tree without
/// orphaning grandchildren.
///
/// Methods that read or signal use `&self` (or the leader's PID); the few that mutate the
/// underlying Tokio child (`take_*`, `try_wait`, `wait`, `send_kill`) take `&mut self`.
pub(crate) struct ProcessGroup {
    leader: tokio::process::Child,
    #[cfg(windows)]
    job: Option<JobObject>,
}

impl std::fmt::Debug for ProcessGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ProcessGroup");
        s.field("leader", &self.leader);
        #[cfg(windows)]
        s.field("job", &self.job);
        s.finish()
    }
}

impl ProcessGroup {
    /// Wraps a freshly spawned child. On Windows, attempts to assign it to a new `JobObject`. On
    /// assignment failure the child is killed before the [`io::Error`] is returned so that the
    /// caller can never leak the just-spawned process.
    ///
    /// On non-Windows targets this is infallible and simply stores the child. The signature stays
    /// `io::Result` to give callers a uniform error-handling shape across platforms.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn from_spawned_child(child: tokio::process::Child) -> Result<Self, io::Error> {
        #[cfg(windows)]
        {
            let job = match child.id() {
                Some(_id) => match JobObject::new_with_child(&child) {
                    Ok(job) => Some(job),
                    Err(source) => {
                        // Best-effort cleanup of the just-spawned child so we do not leak it. We
                        // have not yet attached a JobObject, so `start_kill` only reaches the
                        // leader, but the leader has had no/little time to spawn descendants. This
                        // is the best we can do here.
                        let mut child = child;
                        if let Err(kill_err) = child.start_kill() {
                            tracing::warn!(
                                error = %kill_err,
                                "Failed to kill spawned child after JobObject attachment failed."
                            );
                        }
                        return Err(source);
                    }
                },
                // Child has already been polled to completion.
                None => None,
            };
            Ok(Self { leader: child, job })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { leader: child })
        }
    }

    /// Takes the leader's stdin pipe, leaving `None` behind on the underlying
    /// [`tokio::process::Child`].
    pub(crate) fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.leader.stdin.take()
    }

    /// Takes the leader's stdout pipe, leaving `None` behind on the underlying
    /// [`tokio::process::Child`].
    pub(crate) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.leader.stdout.take()
    }

    /// Takes the leader's stderr pipe, leaving `None` behind on the underlying
    /// [`tokio::process::Child`].
    pub(crate) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.leader.stderr.take()
    }

    /// Returns the leader's OS process ID if the child has not yet been polled to completion.
    ///
    /// On Unix the leader's PID equals the PGID, so this also identifies the process group.
    /// On Windows the Job Object has its own opaque kernel handle; this method returns the
    /// leader process's PID specifically.
    pub(crate) fn id(&self) -> Option<u32> {
        self.leader.id()
    }

    /// Reaps the leader's exit status without blocking.
    ///
    /// Mirrors [`tokio::process::Child::try_wait`].
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.leader.try_wait()
    }

    /// Waits for the leader to exit.
    ///
    /// Mirrors [`tokio::process::Child::wait`]. Note that members of the group other than the
    /// leader may still be running after this resolves; reaching them is the responsibility of
    /// callers that expect a fully-terminated tree (use [`Self::send_kill`] or the
    /// `ProcessHandle::terminate` flow first).
    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.leader.wait().await
    }

    /// Send `SIGINT` to the leader's process group via `killpg`.
    ///
    /// `SIGINT` is the dedicated user-interrupt signal, distinct from the `SIGTERM` delivered
    /// by [`Self::send_terminate`]. The signal targets the child's process group, so any
    /// grandchildren the leader has fork-execed are signaled together with it.
    ///
    /// This is a raw signal sender. Callers that can reap process state should do so before
    /// using it.
    #[cfg(unix)]
    pub(crate) fn send_interrupt(&self) -> io::Result<()> {
        let Some(pid) = self.leader.id() else {
            // Already polled to completion; nothing to signal.
            return Ok(());
        };
        send_to_process_group(pid, nix::sys::signal::Signal::SIGINT)
    }

    /// Send `SIGTERM` to the leader's process group via `killpg`.
    ///
    /// `SIGTERM` is the conventional "asked to terminate" signal sent by service supervisors
    /// and the operating system at shutdown. The signal targets the child's process group, so
    /// any grandchildren the leader has fork-execed are signaled together with it.
    ///
    /// This is a raw signal sender. Callers that can reap process state should do so before
    /// using it.
    #[cfg(unix)]
    pub(crate) fn send_terminate(&self) -> io::Result<()> {
        let Some(pid) = self.leader.id() else {
            // Already polled to completion; nothing to signal.
            return Ok(());
        };
        send_to_process_group(pid, nix::sys::signal::Signal::SIGTERM)
    }

    /// Deliver `CTRL_BREAK_EVENT` to the leader's console process group via
    /// `GenerateConsoleCtrlEvent`.
    ///
    /// `CTRL_BREAK_EVENT` is the only console control event that can be targeted at a nonzero
    /// process group: `CTRL_C_EVENT` requires `dwProcessGroupId = 0` and would be broadcast to
    /// every process sharing the calling console (including the parent), so it is not usable
    /// to terminate a single child group. There is therefore no separate `SIGINT` vs.
    /// `SIGTERM` distinction on Windows; this single method covers the entire graceful surface.
    ///
    /// This is a raw signal sender. Callers that can reap process state should do so before
    /// using it.
    #[cfg(windows)]
    pub(crate) fn send_ctrl_break(&self) -> io::Result<()> {
        use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
        use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

        let Some(pid) = self.leader.id() else {
            // Already polled to completion; nothing to signal.
            return Ok(());
        };

        // SAFETY: `GenerateConsoleCtrlEvent` is a thread-safe Win32 API. `pid` was just
        // observed via `Child::id()`.
        let success = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Forcefully kill the entire spawned tree.
    ///
    /// On Unix this is `killpg(SIGKILL)` against the leader's PGID; on Windows it is
    /// `TerminateJobObject` against the assigned Job Object. If the Windows path has no Job
    /// Object attached (only possible if the child had already exited at spawn time), this
    /// falls back to [`tokio::process::Child::start_kill`] and only reaches the leader. On
    /// other Tokio-supported platforms there is no library-managed group setup, so this also
    /// falls back to `start_kill`.
    ///
    /// Returns `Ok(())` immediately if the child has already been polled to completion.
    pub(crate) fn send_kill(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            match self.leader.id() {
                Some(pid) => send_kill_to_process_group(pid),
                None => Ok(()),
            }
        }
        #[cfg(windows)]
        {
            match self.leader.id() {
                Some(_) => match &self.job {
                    Some(job) => job.terminate(1),
                    None => self.leader.start_kill(),
                },
                None => Ok(()),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.leader.start_kill()
        }
    }

    /// Consume this group and return the bare Tokio [`tokio::process::Child`] leader,
    /// dropping the platform group-management state.
    ///
    /// On Windows the Job Object handle is closed here; because we deliberately do **not** set
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` on the job, closing the handle does not terminate
    /// the child. This is what lets [`crate::ProcessHandle::into_inner`] hand the child back to
    /// the caller for them to manage.
    pub(crate) fn into_leader(self) -> tokio::process::Child {
        self.leader
    }
}

/// Sends `signal` to every process in the child's process group on Unix.
///
/// `pid` is the spawned child's PID. Because the child is set up as the leader of a new process
/// group at spawn time (see [`prepare_command`]), its PID also serves as the PGID. The signal is
/// sent via `killpg`, which translates to `kill(-PGID, signal)` and reaches every member of the
/// group, including any grandchildren the child has fork-execed.
#[cfg(unix)]
fn send_to_process_group(pid: u32, signal: nix::sys::signal::Signal) -> io::Result<()> {
    use nix::sys::signal;
    use nix::unistd::Pid;

    signal::killpg(Pid::from_raw(pid.cast_signed()), signal).map_err(io::Error::other)?;
    Ok(())
}

/// Sends `SIGKILL` to every process in the child's process group on Unix.
///
/// Unlike [`tokio::process::Child::start_kill`], which targets only the child's PID, this hits
/// the whole group so grandchildren are not orphaned. The caller is still responsible for reaping
/// the leader's exit status afterwards (e.g. via `wait`).
#[cfg(unix)]
fn send_kill_to_process_group(pid: u32) -> io::Result<()> {
    send_to_process_group(pid, nix::sys::signal::Signal::SIGKILL)
}
