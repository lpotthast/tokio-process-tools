use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, ReadBuf};

/// Builds deterministic test commands that emit scripted stdout and stderr content.
pub(crate) struct ScriptedOutput {
    _private: (),
}

impl ScriptedOutput {
    pub(crate) fn builder() -> ScriptedOutputBuilder {
        ScriptedOutputBuilder {
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

pub(crate) struct ScriptedOutputBuilder {
    stdout: Vec<ScriptedOutputAction>,
    stderr: Vec<ScriptedOutputAction>,
}

impl ScriptedOutputBuilder {
    pub(crate) fn stdout(self, text: impl Into<String>) -> Self {
        self.stdout_after(Duration::ZERO, text)
    }

    pub(crate) fn stderr(self, text: impl Into<String>) -> Self {
        self.stderr_after(Duration::ZERO, text)
    }

    pub(crate) fn stdout_after(mut self, duration: Duration, text: impl Into<String>) -> Self {
        self.stdout.push(ScriptedOutputAction {
            delay: duration,
            text: text.into(),
        });
        self
    }

    pub(crate) fn stderr_after(mut self, duration: Duration, text: impl Into<String>) -> Self {
        self.stderr.push(ScriptedOutputAction {
            delay: duration,
            text: text.into(),
        });
        self
    }

    #[cfg(not(windows))]
    pub(crate) fn build(self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sh");
        let mut script = String::new();

        push_unix_stream_script(
            &mut script,
            "TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT",
            &self.stdout,
            false,
        );
        push_unix_stream_script(
            &mut script,
            "TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR",
            &self.stderr,
            true,
        );
        script.push_str("wait \"$TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT_PID\"\n");
        script.push_str("wait \"$TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR_PID\"\n");

        set_scripted_output_env(&mut cmd, "TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT", self.stdout);
        set_scripted_output_env(&mut cmd, "TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR", self.stderr);
        cmd.arg("-c").arg(script);
        cmd
    }

    #[cfg(windows)]
    pub(crate) fn build(self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("powershell.exe");
        let mut script = String::new();

        push_powershell_stream_script(
            &mut script,
            "TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT",
            &self.stdout,
            "Out",
        );
        push_powershell_stream_script(
            &mut script,
            "TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR",
            &self.stderr,
            "Error",
        );
        script.push_str("$TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT_THREAD.Start()\n");
        script.push_str("$TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR_THREAD.Start()\n");
        script.push_str("$TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT_THREAD.Join()\n");
        script.push_str("$TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR_THREAD.Join()\n");

        set_scripted_output_env(&mut cmd, "TOKIO_PROCESS_TOOLS_SCRIPTED_STDOUT", self.stdout);
        set_scripted_output_env(&mut cmd, "TOKIO_PROCESS_TOOLS_SCRIPTED_STDERR", self.stderr);
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()]);
        cmd
    }
}

struct ScriptedOutputAction {
    delay: Duration,
    text: String,
}

fn set_scripted_output_env(
    cmd: &mut tokio::process::Command,
    prefix: &str,
    actions: Vec<ScriptedOutputAction>,
) {
    for (index, action) in actions.into_iter().enumerate() {
        cmd.env(format!("{prefix}_{index}"), action.text);
    }
}

#[cfg(not(windows))]
fn push_unix_stream_script(
    script: &mut String,
    prefix: &str,
    actions: &[ScriptedOutputAction],
    stderr: bool,
) {
    script.push_str("(\n");
    if actions.is_empty() {
        script.push_str(":\n");
    }
    for (index, action) in actions.iter().enumerate() {
        if !action.delay.is_zero() {
            script.push_str("sleep ");
            script.push_str(&unix_duration(action.delay));
            script.push('\n');
        }
        script.push_str("printf '%s' \"$");
        script.push_str(prefix);
        script.push('_');
        script.push_str(&index.to_string());
        script.push('"');
        if stderr {
            script.push_str(" >&2");
        }
        script.push('\n');
    }
    script.push_str(") &\n");
    script.push_str(prefix);
    script.push_str("_PID=$!\n");
}

#[cfg(not(windows))]
fn unix_duration(duration: Duration) -> String {
    format!("{}.{:09}", duration.as_secs(), duration.subsec_nanos())
}

#[cfg(windows)]
fn push_powershell_stream_script(
    script: &mut String,
    prefix: &str,
    actions: &[ScriptedOutputAction],
    console_stream: &str,
) {
    script.push('$');
    script.push_str(prefix);
    script.push_str("_THREAD = [System.Threading.Thread]::new([System.Threading.ThreadStart] {\n");
    for (index, action) in actions.iter().enumerate() {
        if !action.delay.is_zero() {
            script.push_str("Start-Sleep -Milliseconds ");
            script.push_str(&powershell_duration_millis(action.delay));
            script.push('\n');
        }
        script.push_str("[Console]::");
        script.push_str(console_stream);
        script.push_str(".Write($env:");
        script.push_str(prefix);
        script.push('_');
        script.push_str(&index.to_string());
        script.push_str(")\n");
    }
    script.push_str("})\n");
}

#[cfg(windows)]
fn powershell_duration_millis(duration: Duration) -> String {
    let millis = duration.as_millis();
    let rounded_millis = if duration.subsec_nanos() % 1_000_000 == 0 {
        millis
    } else {
        millis + 1
    };
    rounded_millis.min(i32::MAX as u128).to_string()
}

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

#[derive(Debug)]
pub(crate) struct ReadErrorAfterBytes {
    bytes: &'static [u8],
    offset: usize,
    error_kind: io::ErrorKind,
}

impl ReadErrorAfterBytes {
    pub(crate) fn new(bytes: &'static [u8], error_kind: io::ErrorKind) -> Self {
        Self {
            bytes,
            offset: 0,
            error_kind,
        }
    }
}

impl AsyncRead for ReadErrorAfterBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset < self.bytes.len() {
            let remaining = &self.bytes[self.offset..];
            let len = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..len]);
            self.offset += len;
            return Poll::Ready(Ok(()));
        }

        Poll::Ready(Err(io::Error::new(
            self.error_kind,
            "injected read failure",
        )))
    }
}
