#[cfg(unix)]
pub(crate) async fn send_interrupt(child: &tokio::process::Child) -> std::io::Result<()> {
    use nix::sys::signal::{self, Signal};
    use nix::unistd::Pid;

    let pid = Pid::from_raw(child.id().expect("Failed to get child PID") as i32);
    signal::kill(pid, Signal::SIGINT)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}

#[cfg(windows)]
pub(crate) async fn send_interrupt(child: &tokio::process::Child) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CTRL_C_EVENT;
    use windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent;

    let pid = child.id().expect("Failed to get child PID") as u32;
    let success = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, pid) };
    if success == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
