/// Ask the `child` to terminate gracefully.
/// This signal is typically sent to a process by its controlling terminal when a user wishes to
/// interrupt the process, initiated by pressing `Ctrl+C`.
///
/// - on `cfg(unix)`: Sends a `SIGINT` to the process.
/// - on `cfg(windows)`: Sends a `CTRL_C_EVENT` to the process.
/// - raises a panic on any other platform!
pub(crate) async fn send_interrupt(child: &tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // Returns `None` if child was already "polled to completion".
        return Ok(());
    };

    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        signal::kill(Pid::from_raw(pid as i32), Signal::SIGINT)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CTRL_C_EVENT;
        use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

        let success = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid) };
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

/// Ask the `child` to terminate gracefully. Nearly identical to `send_interrupt`.
/// This signal is typically sent to a process when the operating system requests a termination.
///
/// - on `cfg(unix)`: Sends a `SIGTERM` to the process.
/// - on `cfg(windows)`: Sends a `CTRL_BREAK_EVENT` to the process.
/// - raises a panic on any other platform!
pub(crate) async fn send_terminate(child: &tokio::process::Child) -> std::io::Result<()> {
    let Some(pid) = child.id() else {
        // Returns `None` if child was already "polled to completion".
        return Ok(());
    };

    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CTRL_BREAK_EVENT;
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
