use crate::error::{SpawnError, TerminationError, WaitError};
use crate::output::Output;
use crate::output_stream::broadcast::BroadcastOutputStream;
use crate::output_stream::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::{BackpressureControl, FromStreamOptions};
use crate::panic_on_drop::PanicOnDrop;
use crate::terminate_on_drop::TerminateOnDrop;
use crate::{LineParsingOptions, NumBytes, OutputStream, signal};
use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::process::Child;

const STDOUT_STREAM_NAME: &str = "stdout";
const STDERR_STREAM_NAME: &str = "stderr";

/// Maximum time to wait for process termination after sending SIGKILL.
///
/// This is a safety timeout since SIGKILL should terminate processes immediately,
/// but there are rare cases where even SIGKILL may not work.
const SIGKILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

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
    pub fn as_bool(&self) -> bool {
        match self {
            RunningState::Running => true,
            RunningState::Terminated(_) | RunningState::Uncertain(_) => false,
        }
    }
}

impl From<RunningState> for bool {
    fn from(is_running: RunningState) -> Self {
        match is_running {
            RunningState::Running => true,
            RunningState::Terminated(_) | RunningState::Uncertain(_) => false,
        }
    }
}

/// A handle to a spawned process with captured stdout/stderr streams.
///
/// This type provides methods for waiting on process completion, terminating the process,
/// and accessing its output streams. By default, processes must be explicitly waited on
/// or terminated before being dropped (see [`ProcessHandle::must_be_terminated`]).
///
/// If applicable, a process handle can be wrapped in a [`TerminateOnDrop`] to be terminated
/// automatically upon being dropped. Note that this requires a multi-threaded runtime!
#[derive(Debug)]
pub struct ProcessHandle<O: OutputStream> {
    pub(crate) name: Cow<'static, str>,
    child: Child,
    std_out_stream: O,
    std_err_stream: O,
    panic_on_drop: Option<PanicOnDrop>,
}

impl ProcessHandle<BroadcastOutputStream> {
    /// Spawns a new process with broadcast output streams and custom channel capacities.
    ///
    /// This method is intended for internal use by the `Process` builder.
    /// Users should use `Process::new(cmd).spawn_broadcast()` instead.
    pub(crate) fn spawn_with_capacity(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
        stdout_chunk_size: NumBytes,
        stderr_chunk_size: NumBytes,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> Result<ProcessHandle<BroadcastOutputStream>, SpawnError> {
        let process_name = name.into();
        Self::prepare_command(&mut cmd)
            .spawn()
            .map(|child| {
                Self::new_from_child_with_piped_io_and_capacity(
                    process_name.clone(),
                    child,
                    stdout_chunk_size,
                    stderr_chunk_size,
                    stdout_channel_capacity,
                    stderr_channel_capacity,
                )
            })
            .map_err(|source| SpawnError::SpawnFailed {
                process_name,
                source,
            })
    }

    fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        mut child: Child,
        stdout_chunk_size: NumBytes,
        stderr_chunk_size: NumBytes,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> ProcessHandle<BroadcastOutputStream> {
        let stdout = child
            .stdout
            .take()
            .expect("Child process stdout wasn't captured");
        let stderr = child
            .stderr
            .take()
            .expect("Child process stderr wasn't captured");

        let (child, std_out_stream, std_err_stream) = (
            child,
            BroadcastOutputStream::from_stream(
                stdout,
                "stdout",
                FromStreamOptions {
                    chunk_size: stdout_chunk_size,
                    channel_capacity: stdout_channel_capacity,
                },
            ),
            BroadcastOutputStream::from_stream(
                stderr,
                STDERR_STREAM_NAME,
                FromStreamOptions {
                    chunk_size: stderr_chunk_size,
                    channel_capacity: stderr_channel_capacity,
                },
            ),
        );

        let mut this = ProcessHandle {
            name: name.into(),
            child,
            std_out_stream,
            std_err_stream,
            panic_on_drop: None,
        };
        this.must_be_terminated();
        this
    }

    /// Convenience function, waiting for the process to complete using
    /// [ProcessHandle::wait_for_completion] while collecting both `stdout` and `stderr`
    /// into individual `Vec<String>` collections using the provided [LineParsingOptions].
    ///
    /// You may want to destructure this using:
    /// ```no_run
    /// # use tokio::process::Command;
    /// # use tokio_process_tools::*;
    /// # tokio_test::block_on(async {
    /// # let mut proc = Process::new(Command::new("ls")).spawn_broadcast().unwrap();
    /// let Output {
    ///     status,
    ///     stdout,
    ///     stderr
    /// } = proc.wait_for_completion_with_output(None, LineParsingOptions::default()).await.unwrap();
    /// # });
    /// ```
    pub async fn wait_for_completion_with_output(
        &mut self,
        timeout: Option<Duration>,
        options: LineParsingOptions,
    ) -> Result<Output, WaitError> {
        let out_collector = self.stdout().collect_lines_into_vec(options);
        let err_collector = self.stderr().collect_lines_into_vec(options);

        let status = self.wait_for_completion(timeout).await?;

        let stdout = out_collector.wait().await?;
        let stderr = err_collector.wait().await?;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Convenience function, waiting for the process to complete using
    /// [ProcessHandle::wait_for_completion_or_terminate] while collecting both `stdout` and `stderr`
    /// into individual `Vec<String>` collections using the provided [LineParsingOptions].
    pub async fn wait_for_completion_with_output_or_terminate(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        options: LineParsingOptions,
    ) -> Result<Output, WaitError> {
        let out_collector = self.stdout().collect_lines_into_vec(options);
        let err_collector = self.stderr().collect_lines_into_vec(options);

        let status = self
            .wait_for_completion_or_terminate(wait_timeout, interrupt_timeout, terminate_timeout)
            .await?;

        let stdout = out_collector.wait().await?;
        let stderr = err_collector.wait().await?;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl ProcessHandle<SingleSubscriberOutputStream> {
    /// Spawns a new process with single subscriber output streams and custom channel capacities.
    ///
    /// This method is intended for internal use by the `Process` builder.
    /// Users should use `Process::new(cmd).spawn_single_subscriber()` instead.
    pub(crate) fn spawn_with_capacity(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
        stdout_chunk_size: NumBytes,
        stderr_chunk_size: NumBytes,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> Result<Self, SpawnError> {
        let process_name = name.into();
        Self::prepare_command(&mut cmd)
            .spawn()
            .map(|child| {
                Self::new_from_child_with_piped_io_and_capacity(
                    process_name.clone(),
                    child,
                    stdout_chunk_size,
                    stderr_chunk_size,
                    stdout_channel_capacity,
                    stderr_channel_capacity,
                )
            })
            .map_err(|source| SpawnError::SpawnFailed {
                process_name,
                source,
            })
    }

    fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        mut child: Child,
        stdout_chunk_size: NumBytes,
        stderr_chunk_size: NumBytes,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> Self {
        let stdout = child
            .stdout
            .take()
            .expect("Child process stdout wasn't captured");
        let stderr = child
            .stderr
            .take()
            .expect("Child process stderr wasn't captured");

        let (child, std_out_stream, std_err_stream) = (
            child,
            SingleSubscriberOutputStream::from_stream(
                stdout,
                STDOUT_STREAM_NAME,
                BackpressureControl::DropLatestIncomingIfBufferFull,
                FromStreamOptions {
                    chunk_size: stdout_chunk_size,
                    channel_capacity: stdout_channel_capacity,
                },
            ),
            SingleSubscriberOutputStream::from_stream(
                stderr,
                STDERR_STREAM_NAME,
                BackpressureControl::DropLatestIncomingIfBufferFull,
                FromStreamOptions {
                    chunk_size: stderr_chunk_size,
                    channel_capacity: stderr_channel_capacity,
                },
            ),
        );

        let mut this = ProcessHandle {
            name: name.into(),
            child,
            std_out_stream,
            std_err_stream,
            panic_on_drop: None,
        };
        this.must_be_terminated();
        this
    }

    /// Convenience function, waiting for the process to complete using
    /// [ProcessHandle::wait_for_completion] while collecting both `stdout` and `stderr`
    /// into individual `Vec<String>` collections using the provided [LineParsingOptions].
    ///
    /// You may want to destructure this using:
    /// ```no_run
    /// # use tokio_process_tools::*;
    /// # tokio_test::block_on(async {
    /// # let mut proc = Process::new(tokio::process::Command::new("ls")).spawn_broadcast().unwrap();
    /// let Output {
    ///     status,
    ///     stdout,
    ///     stderr
    /// } = proc.wait_for_completion_with_output(None, LineParsingOptions::default()).await.unwrap();
    /// # });
    /// ```
    pub async fn wait_for_completion_with_output(
        &mut self,
        timeout: Option<Duration>,
        options: LineParsingOptions,
    ) -> Result<Output, WaitError> {
        let out_collector = self.stdout().collect_lines_into_vec(options);
        let err_collector = self.stderr().collect_lines_into_vec(options);

        let status = self.wait_for_completion(timeout).await?;

        let stdout = out_collector.wait().await?;
        let stderr = err_collector.wait().await?;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// Convenience function, waiting for the process to complete using
    /// [ProcessHandle::wait_for_completion_or_terminate] while collecting both `stdout` and `stderr`
    /// into individual `Vec<String>` collections using the provided [LineParsingOptions].
    pub async fn wait_for_completion_with_output_or_terminate(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        options: LineParsingOptions,
    ) -> Result<Output, WaitError> {
        let out_collector = self.stdout().collect_lines_into_vec(options);
        let err_collector = self.stderr().collect_lines_into_vec(options);

        let status = self
            .wait_for_completion_or_terminate(wait_timeout, interrupt_timeout, terminate_timeout)
            .await?;

        let stdout = out_collector.wait().await?;
        let stderr = err_collector.wait().await?;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl<O: OutputStream> ProcessHandle<O> {
    /// On Windows, you can only send `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` to process groups,
    /// which works more like `killpg`. Sending to the current process ID will likely trigger
    /// undefined behavior of sending the event to every process that's attached to the console,
    /// i.e. sending the event to group ID 0. Therefore, we create a new process group
    /// for the child process we are about to spawn.
    ///
    /// See: https://stackoverflow.com/questions/44124338/trying-to-implement-signal-ctrl-c-event-in-python3-6
    fn prepare_platform_specifics(
        command: &mut tokio::process::Command,
    ) -> &mut tokio::process::Command {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP)
        }
        #[cfg(not(windows))]
        {
            command
        }
    }

    fn prepare_command(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
        Self::prepare_platform_specifics(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // It is much too easy to leave dangling resources here and there.
            // This library tries to make it clear and encourage users to terminate spawned
            // processes appropriately. If not done so anyway, this acts as a "last resort"
            // type of solution, less graceful as the `terminate_on_drop` effect but at least
            // capable of cleaning up.
            .kill_on_drop(true)
    }

    /// Returns the OS process ID if the process hasn't exited yet.
    ///
    /// Once this process has been polled to completion this will return None.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Checks if the process is currently running.
    ///
    /// Returns [`RunningState::Running`] if the process is still running,
    /// [`RunningState::Terminated`] if it has exited, or [`RunningState::Uncertain`]
    /// if the state could not be determined.
    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> RunningState {
        match self.child.try_wait() {
            Ok(None) => RunningState::Running,
            Ok(Some(exit_status)) => {
                self.must_not_be_terminated();
                RunningState::Terminated(exit_status)
            }
            Err(err) => RunningState::Uncertain(err),
        }
    }

    /// Returns a reference to the stdout stream.
    ///
    /// For `BroadcastOutputStream`, this allows creating multiple concurrent consumers.
    /// For `SingleSubscriberOutputStream`, only one consumer can be created (subsequent
    /// attempts will panic with a helpful error message).
    pub fn stdout(&self) -> &O {
        &self.std_out_stream
    }

    /// Returns a reference to the stderr stream.
    ///
    /// For `BroadcastOutputStream`, this allows creating multiple concurrent consumers.
    /// For `SingleSubscriberOutputStream`, only one consumer can be created (subsequent
    /// attempts will panic with a helpful error message).
    pub fn stderr(&self) -> &O {
        &self.std_err_stream
    }

    /// Sets a panic-on-drop mechanism for this `ProcessHandle`.
    ///
    /// This method enables a safeguard that ensures that the process represented by this
    /// `ProcessHandle` is properly terminated or awaited before being dropped.
    /// If `must_be_terminated` is set and the `ProcessHandle` is
    /// dropped without invoking `terminate()` or `wait()`, an intentional panic will occur to
    /// prevent silent failure-states, ensuring that system resources are handled correctly.
    ///
    /// You typically do not need to call this, as every ProcessHandle is marked by default.
    /// Call `must_not_be_terminated` to clear this safeguard to explicitly allow dropping the
    /// process without terminating it.
    ///
    /// # Panic
    ///
    /// If the `ProcessHandle` is dropped without being awaited or terminated
    /// after calling this method, a panic will occur with a descriptive message
    /// to inform about the incorrect usage.
    pub fn must_be_terminated(&mut self) {
        self.panic_on_drop = Some(PanicOnDrop::new(
            "tokio_process_tools::ProcessHandle",
            "The process was not terminated.",
            "Call `wait_for_completion` or `terminate` before the type is dropped!",
        ));
    }

    /// Disables the panic-on-drop safeguard, allowing the spawned process to be kept running
    /// uncontrolled in the background, while this handle can safely be dropped.
    pub fn must_not_be_terminated(&mut self) {
        if let Some(mut it) = self.panic_on_drop.take() {
            it.defuse()
        }
    }

    /// Wrap this process handle in a `TerminateOnDrop` instance, terminating the controlled process
    /// automatically when this handle is dropped.
    ///
    /// **SAFETY: This only works when your code is running in a multithreaded tokio runtime!**
    ///
    /// Prefer manual termination of the process or awaiting it and relying on the (automatically
    /// configured) `must_be_terminated` logic, raising a panic when a process was neither awaited
    /// nor terminated before being dropped.
    pub fn terminate_on_drop(
        self,
        graceful_termination_timeout: Duration,
        forceful_termination_timeout: Duration,
    ) -> TerminateOnDrop<O> {
        TerminateOnDrop {
            process_handle: self,
            interrupt_timeout: graceful_termination_timeout,
            terminate_timeout: forceful_termination_timeout,
        }
    }

    /// Manually send a `SIGINT` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub fn send_interrupt_signal(&mut self) -> Result<(), io::Error> {
        signal::send_interrupt(&self.child)
    }

    /// Manually send a `SIGTERM` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub fn send_terminate_signal(&mut self) -> Result<(), io::Error> {
        signal::send_terminate(&self.child)
    }

    /// Terminates this process by sending a `SIGINT`, `SIGTERM` or even a `SIGKILL` if the process
    /// doesn't run to completion after receiving any of the first two signals.
    ///
    /// This handle can be dropped safely after this call returned, no matter the outcome.
    /// We accept that in extremely rare cases, failed `SIGKILL`, a rogue process may be left over.
    pub async fn terminate(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        // Whether or not this function will ultimately succeed, we tried our best to terminate
        // this process.
        // Dropping this handle should not create any on-drop panic anymore.
        // We accept that in extremely rare cases, failed `kill`, a rogue process may be left over.
        self.must_not_be_terminated();

        self.send_interrupt_signal()
            .map_err(|err| TerminationError::SignallingFailed {
                process_name: self.name.clone(),
                source: err,
                signal: "SIGINT",
            })?;

        match self.wait_for_completion(Some(interrupt_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(not_terminated_after_sigint) => {
                tracing::warn!(
                    process = %self.name,
                    error = %not_terminated_after_sigint,
                    "Graceful shutdown using SIGINT (or equivalent on current platform) failed. Attempting graceful shutdown using SIGTERM signal."
                );

                self.send_terminate_signal()
                    .map_err(|err| TerminationError::SignallingFailed {
                        process_name: self.name.clone(),
                        source: err,
                        signal: "SIGTERM",
                    })?;

                match self.wait_for_completion(Some(terminate_timeout)).await {
                    Ok(exit_status) => Ok(exit_status),
                    Err(not_terminated_after_sigterm) => {
                        tracing::warn!(
                            process = %self.name,
                            error = %not_terminated_after_sigterm,
                            "Graceful shutdown using SIGTERM (or equivalent on current platform) failed. Attempting forceful shutdown using SIGKILL signal."
                        );

                        match self.kill().await {
                            Ok(()) => {
                                // Note: A SIGKILL should typically (somewhat) immediately lead to
                                // termination of the process. But there are cases in which even
                                // a SIGKILL does not / cannot / will not kill a process.
                                // Something must have gone horribly wrong then...
                                // But: We do not want to wait indefinitely in case this happens
                                // and therefore wait (at max) for a fixed duration after any
                                // SIGKILL event.
                                match self.wait_for_completion(Some(SIGKILL_WAIT_TIMEOUT)).await {
                                    Ok(exit_status) => Ok(exit_status),
                                    Err(not_terminated_after_sigkill) => {
                                        // Unlikely. See the note above.
                                        tracing::error!(
                                            "Process, having custom name '{}', did not terminate after receiving a SIGINT, SIGTERM and SIGKILL event (or equivalent on the current platform). Something must have gone horribly wrong... Process may still be running. Manual intervention and investigation required!",
                                            self.name
                                        );
                                        Err(TerminationError::TerminationFailed {
                                            process_name: self.name.clone(),
                                            sigint_error: not_terminated_after_sigint.to_string(),
                                            sigterm_error: not_terminated_after_sigterm.to_string(),
                                            sigkill_error: io::Error::new(
                                                io::ErrorKind::TimedOut,
                                                not_terminated_after_sigkill.to_string(),
                                            ),
                                        })
                                    }
                                }
                            }
                            Err(kill_error) => {
                                tracing::error!(
                                    process = %self.name,
                                    error = %kill_error,
                                    "Forceful shutdown using SIGKILL (or equivalent on current platform) failed. Process may still be running. Manual intervention required!"
                                );

                                Err(TerminationError::TerminationFailed {
                                    process_name: self.name.clone(),
                                    sigint_error: not_terminated_after_sigint.to_string(),
                                    sigterm_error: not_terminated_after_sigterm.to_string(),
                                    sigkill_error: kill_error,
                                })
                            }
                        }
                    }
                }
            }
        }
    }

    /// Forces the process to exit. Most users should call [ProcessHandle::terminate] instead.
    ///
    /// This is equivalent to sending a SIGKILL on unix platforms followed by wait.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
    }

    /// Successfully awaiting the completion of the process will unset the
    /// "must be terminated" setting, as a successfully awaited process is already terminated.
    /// Dropping this `ProcessHandle` after successfully calling `wait` should never lead to a
    /// "must be terminated" panic being raised.
    async fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.child.wait().await {
            Ok(status) => {
                self.must_not_be_terminated();
                Ok(status)
            }
            Err(err) => Err(err),
        }
    }

    /// Wait for this process to run to completion. Within `timeout`, if set, or unbound otherwise.
    ///
    /// If the timeout is reached before the process terminated, an error is returned but the
    /// process remains untouched / keeps running.
    /// Use [ProcessHandle::wait_for_completion_or_terminate] if you want immediate termination.
    ///
    /// This does not provide the processes output. You can take a look at the convenience function
    /// [ProcessHandle::<BroadcastOutputStream>::wait_for_completion_with_output] to see
    /// how the [ProcessHandle::stdout] and [ProcessHandle::stderr] streams (also available in
    /// *_mut variants) can be used to inspect / watch over / capture the processes output.
    pub async fn wait_for_completion(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<ExitStatus, WaitError> {
        match timeout {
            None => self.wait().await.map_err(|source| WaitError::IoError {
                process_name: self.name.clone(),
                source,
            }),
            Some(timeout_duration) => {
                match tokio::time::timeout(timeout_duration, self.wait()).await {
                    Ok(Ok(exit_status)) => Ok(exit_status),
                    Ok(Err(source)) => Err(WaitError::IoError {
                        process_name: self.name.clone(),
                        source,
                    }),
                    Err(_elapsed) => Err(WaitError::Timeout {
                        process_name: self.name.clone(),
                        timeout: timeout_duration,
                    }),
                }
            }
        }
    }

    /// Wait for this process to run to completion within `timeout`.
    ///
    /// If the timeout is reached before the process terminated normally, external termination of
    /// the process is forced through [ProcessHandle::terminate].
    ///
    /// Note that this function may return `Ok` even though the timeout was reached, carrying the
    /// exit status received after sending a termination signal!
    pub async fn wait_for_completion_or_terminate(
        &mut self,
        wait_timeout: Duration,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        match self.wait_for_completion(Some(wait_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(_err) => self.terminate(interrupt_timeout, terminate_timeout).await,
        }
    }

    /// Consumes this handle to provide the wrapped `tokio::process::Child` instance as well as the
    /// stdout and stderr output streams.
    pub fn into_inner(self) -> (Child, O, O) {
        (self.child, self.std_out_stream, self.std_err_stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;

    #[tokio::test]
    async fn test_termination() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let started_at = jiff::Zoned::now();
        let mut handle = crate::Process::new(cmd)
            .name("sleep")
            .spawn_broadcast()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let exit_status = handle
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();
        let terminated_at = jiff::Zoned::now();

        // We terminate after roughly 100 ms of waiting.
        // Let's use a 50 ms grace period on the assertion taken up by performing the termination.
        // We can increase this if the test should turn out to be flaky.
        let ran_for = started_at.duration_until(&terminated_at);
        assert_that(ran_for.as_secs_f32()).is_close_to(0.1, 0.5);

        // When terminated, we do not get an exit code (unix).
        assert_that(exit_status.code()).is_none();
    }
}
