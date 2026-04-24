use std::time::Duration;

/// Builds a command that naturally exits after approximately `duration`.
#[cfg(not(windows))]
pub(crate) fn long_running_command(duration: Duration) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sleep");
    cmd.arg(format!(
        "{}.{:09}",
        duration.as_secs(),
        duration.subsec_nanos()
    ));
    cmd
}

/// Builds a command that naturally exits after approximately `duration`.
#[cfg(windows)]
pub(crate) fn long_running_command(duration: Duration) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("powershell.exe");
    let sleep_milliseconds = duration.as_millis().min(i32::MAX as u128).to_string();
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "Start-Sleep",
        "-Milliseconds",
        sleep_milliseconds.as_str(),
    ]);
    cmd
}
