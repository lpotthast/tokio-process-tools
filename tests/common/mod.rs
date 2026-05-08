//! Helpers shared by the integration test binaries under `tests/`.
//!
//! Cargo treats `tests/common/mod.rs` as a module of every integration binary, not as its own
//! binary. Each `tests/<name>.rs` file uses `mod common;` plus `use common::*;` to pull these in.

#![allow(dead_code)]

use assertr::prelude::*;
use std::io;
use std::time::Duration;
use tokio_process_tools::visitors::collect::CollectChunks;
use tokio_process_tools::{
    BroadcastOutputStream, CollectedBytes, CollectedLines, CollectionOverflowBehavior, Consumable,
    Consumer, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, GracefulShutdown,
    LineCollectionOptions, LineOutputOptions, LineOverflowBehavior, LineParsingOptions,
    LossyWithoutBackpressure, NoReplay, NumBytesExt, OutputStream, ParseLines, Process,
    ProcessHandle, ProcessName, RawCollectionOptions, RawOutputOptions, ReliableWithBackpressure,
    ReplayEnabled, SingleSubscriberOutputStream, TerminationAction, TerminationAttemptError,
};

#[track_caller]
pub fn assert_attempt_error(
    attempt_error: &TerminationAttemptError,
    expected_action: TerminationAction,
    expected_kind: io::ErrorKind,
    expected_message: &str,
) {
    assert_that!(attempt_error.action).is_equal_to(expected_action);

    let io_error = attempt_error
        .source
        .downcast_ref::<io::Error>()
        .expect("diagnostic should preserve the original io::Error");

    assert_that!(io_error.kind()).is_equal_to(expected_kind);
    assert_that!(io_error.to_string().as_str()).contains(expected_message);
}

pub fn default_graceful_shutdown() -> GracefulShutdown {
    GracefulShutdown::builder()
        .unix_sigterm(Duration::from_secs(1))
        .windows_ctrl_break(Duration::from_secs(1))
        .build()
}

pub fn short_graceful_shutdown() -> GracefulShutdown {
    GracefulShutdown::builder()
        .unix_sigterm(Duration::from_millis(50))
        .windows_ctrl_break(Duration::from_millis(50))
        .build()
}

pub fn line_parsing_options() -> LineParsingOptions {
    LineParsingOptions::builder()
        .max_line_length(16.kilobytes())
        .overflow_behavior(LineOverflowBehavior::default())
        .buffer_compaction_threshold(None)
        .build()
}

pub fn line_collection_options() -> LineCollectionOptions {
    LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::default(),
    }
}

pub fn line_output_options() -> LineOutputOptions {
    LineOutputOptions::symmetric(line_collection_options())
}

/// Spawns a [`CollectedLines`] consumer over `stream` using the test-default line parsing /
/// collection options. Mirrors the convenience the deleted `collect_lines_into_vec` factory
/// used to provide.
///
/// # Errors
///
/// Returns the stream's `Consumable::Error` if the consumer cannot be created.
pub fn collect_lines_into_vec<S>(
    stream: &S,
) -> Result<Consumer<CollectedLines>, <S as Consumable>::Error>
where
    S: Consumable + OutputStream,
{
    stream.consume(ParseLines::collect(
        line_parsing_options(),
        CollectedLines::new(),
        CollectedLines::line_collector(line_collection_options()),
    ))
}

/// Spawns a [`CollectedBytes`] consumer over `stream` with
/// [`RawCollectionOptions::TrustedUnbounded`].
///
/// # Errors
///
/// Returns the stream's `Consumable::Error` if the consumer cannot be created.
pub fn collect_chunks_into_vec<S>(
    stream: &S,
) -> Result<Consumer<CollectedBytes>, <S as Consumable>::Error>
where
    S: Consumable + OutputStream,
{
    stream.consume(CollectChunks::fold(
        CollectedBytes::new(),
        CollectedBytes::collector(RawCollectionOptions::TrustedUnbounded),
    ))
}

pub fn raw_output_options() -> RawOutputOptions {
    RawOutputOptions::symmetric(RawCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        overflow_behavior: CollectionOverflowBehavior::default(),
    })
}

pub fn spawn_broadcast_with_replay(
    name: impl Into<ProcessName>,
    cmd: tokio::process::Command,
) -> ProcessHandle<BroadcastOutputStream<ReliableWithBackpressure, ReplayEnabled>> {
    Process::new(cmd)
        .name(name)
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub fn spawn_single_subscriber_with_replay(
    name: impl Into<ProcessName>,
    cmd: tokio::process::Command,
) -> ProcessHandle<SingleSubscriberOutputStream<ReliableWithBackpressure, ReplayEnabled>> {
    Process::new(cmd)
        .name(name)
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub fn spawn_long_running_process()
-> ProcessHandle<BroadcastOutputStream<LossyWithoutBackpressure, NoReplay>> {
    Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub fn spawn_immediately_exiting_process()
-> ProcessHandle<BroadcastOutputStream<LossyWithoutBackpressure, NoReplay>> {
    Process::new(immediately_exiting_command())
        .name("immediate-exit")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub fn immediately_exiting_command() -> tokio::process::Command {
    ScriptedOutput::builder().build()
}

/// Builds a command that naturally exits after approximately `duration`.
#[cfg(not(windows))]
pub fn long_running_command(duration: Duration) -> tokio::process::Command {
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
pub fn long_running_command(duration: Duration) -> tokio::process::Command {
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

/// Builds deterministic test commands that emit scripted stdout and stderr content.
pub struct ScriptedOutput {
    _private: (),
}

impl ScriptedOutput {
    pub fn builder() -> ScriptedOutputBuilder {
        ScriptedOutputBuilder {
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

pub struct ScriptedOutputBuilder {
    stdout: Vec<ScriptedOutputAction>,
    stderr: Vec<ScriptedOutputAction>,
}

impl ScriptedOutputBuilder {
    pub fn stdout(self, text: impl Into<String>) -> Self {
        self.stdout_after(Duration::ZERO, text)
    }

    pub fn stderr(self, text: impl Into<String>) -> Self {
        self.stderr_after(Duration::ZERO, text)
    }

    pub fn stdout_after(mut self, duration: Duration, text: impl Into<String>) -> Self {
        self.stdout.push(ScriptedOutputAction {
            delay: duration,
            text: text.into(),
        });
        self
    }

    pub fn stderr_after(mut self, duration: Duration, text: impl Into<String>) -> Self {
        self.stderr.push(ScriptedOutputAction {
            delay: duration,
            text: text.into(),
        });
        self
    }

    #[cfg(not(windows))]
    pub fn build(self) -> tokio::process::Command {
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
    pub fn build(self) -> tokio::process::Command {
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
