/// Prepare a command so platform-specific signal delivery targets the spawned child *and* any
/// processes it goes on to fork.
///
/// The child is set up as the leader of a new process group (Unix) or its own console process
/// group (Windows). Termination signals are then sent to that group, not the child's PID, so any
/// grandchildren the child fork-execs (a shell wrapper that launches the real binary, a build
/// tool that fans out workers) are signaled together with the leader instead of being orphaned
/// when the leader exits.
///
/// - **Unix:** sets the child's process group to itself (`setpgid(0, 0)` via Tokio's
///   `Command::process_group(0)`), so the child's PID is also its PGID and the group is
///   addressable as `-PGID` in `kill(2)`.
/// - **Windows:** passes `CREATE_NEW_PROCESS_GROUP`, so `GenerateConsoleCtrlEvent` with the
///   child's PID as the process-group ID delivers `CTRL_BREAK_EVENT` to every process in the
///   group.
pub(crate) fn prepare_command_for_signalling(
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

#[cfg(unix)]
pub(crate) const INTERRUPT_SIGNAL_NAME: &str = "SIGINT";
#[cfg(windows)]
pub(crate) const INTERRUPT_SIGNAL_NAME: &str = "CTRL_BREAK_EVENT";
#[cfg(all(not(windows), not(unix)))]
pub(crate) const INTERRUPT_SIGNAL_NAME: &str = "interrupt";

#[cfg(unix)]
pub(crate) const TERMINATE_SIGNAL_NAME: &str = "SIGTERM";
#[cfg(windows)]
pub(crate) const TERMINATE_SIGNAL_NAME: &str = "CTRL_BREAK_EVENT";
#[cfg(all(not(windows), not(unix)))]
pub(crate) const TERMINATE_SIGNAL_NAME: &str = "terminate";

#[cfg(unix)]
pub(crate) const KILL_SIGNAL_NAME: &str = "SIGKILL";
#[cfg(windows)]
pub(crate) const KILL_SIGNAL_NAME: &str = "TerminateProcess";
#[cfg(all(not(windows), not(unix)))]
pub(crate) const KILL_SIGNAL_NAME: &str = "kill";

/// Ask the `child` (and every process in its process group) to terminate gracefully.
/// This signal is typically sent to a process by its controlling terminal when a user wishes to
/// interrupt the process, initiated by pressing `Ctrl+C`.
///
/// - on `cfg(unix)`: Sends a `SIGINT` to the child's process group via `killpg`. Spawns set up
///   the child as the leader of a new process group (see [`prepare_command_for_signalling`]),
///   so the signal reaches any grandchildren the child has forked.
/// - on `cfg(windows)`: Sends a `CTRL_BREAK_EVENT` to the child's console process group.
/// - raises a panic on any other platform!
///
/// Windows cannot target `CTRL_C_EVENT` at a nonzero process group. Since this crate creates a
/// process group for each child on Windows, `CTRL_BREAK_EVENT` is the targetable graceful
/// interrupt event.
///
/// This is a raw signal sender over `tokio::process::Child`. Callers that can reap process state
/// should do so before using it.
pub(crate) fn send_interrupt(child: &tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // Returns `None` if child was already "polled to completion".
        return Ok(());
    };

    #[cfg(unix)]
    {
        send_to_process_group(pid, nix::sys::signal::Signal::SIGINT)
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
        use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

        let success = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if success == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(all(not(windows), not(unix)))]
    {
        panic!("Cannot send interrupt signal to process. Platform is unsupported.")
    }
}

/// Ask the `child` (and every process in its process group) to terminate. Nearly identical to
/// `send_interrupt`. This signal is typically sent to a process when the operating system
/// requests a termination.
///
/// - on `cfg(unix)`: Sends a `SIGTERM` to the child's process group via `killpg`.
/// - on `cfg(windows)`: Sends a `CTRL_BREAK_EVENT` to the child's console process group.
/// - raises a panic on any other platform!
///
/// This is a raw signal sender over `tokio::process::Child`. Callers that can reap process state
/// should do so before using it.
pub(crate) fn send_terminate(child: &tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // Returns `None` if child was already "polled to completion".
        return Ok(());
    };

    #[cfg(unix)]
    {
        send_to_process_group(pid, nix::sys::signal::Signal::SIGTERM)
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::CTRL_BREAK_EVENT;
        use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

        let success = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        if success == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(all(not(windows), not(unix)))]
    {
        panic!("Cannot send termination signal to process. Platform is unsupported.")
    }
}

/// Sends `signal` to every process in the child's process group on Unix.
///
/// `pid` is the spawned child's PID. Because the child is set up as the leader of a new process
/// group at spawn time, its PID also serves as the PGID. The signal is sent via `killpg`, which
/// translates to `kill(-PGID, signal)` and reaches every member of the group, including any
/// grandchildren the child has fork-execed.
#[cfg(unix)]
fn send_to_process_group(pid: u32, signal: nix::sys::signal::Signal) -> std::io::Result<()> {
    use nix::sys::signal;
    use nix::unistd::Pid;

    signal::killpg(Pid::from_raw(pid.cast_signed()), signal).map_err(std::io::Error::other)?;
    Ok(())
}

/// Sends `SIGKILL` to every process in the child's process group on Unix.
///
/// Unlike [`tokio::process::Child::start_kill`], which targets only the child's PID, this hits
/// the whole group so grandchildren are not orphaned. The caller is still responsible for reaping
/// the leader's exit status afterwards (e.g. via `wait`).
#[cfg(unix)]
pub(crate) fn send_kill_to_process_group(pid: u32) -> std::io::Result<()> {
    send_to_process_group(pid, nix::sys::signal::Signal::SIGKILL)
}
