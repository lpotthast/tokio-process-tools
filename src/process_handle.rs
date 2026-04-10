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
use tokio::process::{Child, ChildStdin};

const STDOUT_STREAM_NAME: &str = "stdout";
const STDERR_STREAM_NAME: &str = "stderr";

pub(crate) struct SingleSubscriberStreamConfig {
    pub(crate) chunk_size: NumBytes,
    pub(crate) channel_capacity: usize,
    pub(crate) backpressure_control: BackpressureControl,
}

/// Maximum time to wait for process termination after sending SIGKILL.
///
/// This is a safety timeout since SIGKILL should terminate processes immediately,
/// but there are rare cases where even SIGKILL may not work.
const SIGKILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

/// Represents the stdin stream of a child process.
///
/// stdin is always configured as piped, so it starts as `Open` with a [`ChildStdin`] handle
/// that can be used to write data to the process. It can be explicitly closed by calling
/// [`Stdin::close`], after which it transitions to the `Closed` state.
#[derive(Debug)]
pub enum Stdin {
    /// stdin is open and available for writing.
    Open(ChildStdin),
    /// stdin has been closed.
    Closed,
}

impl Stdin {
    /// Returns `true` if stdin is open and available for writing.
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
    /// will be in the `Closed` state and no further writes will be possible.
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
    std_in: Stdin,
    std_out_stream: O,
    std_err_stream: O,
    cleanup_on_drop: bool,
    panic_on_drop: Option<PanicOnDrop>,
}

impl<O: OutputStream> Drop for ProcessHandle<O> {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            // We want users to explicitly await or terminate spawned processes.
            // If not done so, kill the process now to have some sort of last-resort cleanup.
            // A separate panic-on-drop guard may additionally raise a panic to signal the misuse.
            if let Err(err) = self.child.start_kill() {
                tracing::warn!(
                    process = %self.name,
                    error = %err,
                    "Failed to kill process while dropping an armed ProcessHandle"
                );
            }
        }
    }
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
        let std_in = match child.stdin.take() {
            Some(stdin) => Stdin::Open(stdin),
            None => Stdin::Closed,
        };
        let stdout = child
            .stdout
            .take()
            .expect("Child process stdout wasn't captured");
        let stderr = child
            .stderr
            .take()
            .expect("Child process stderr wasn't captured");

        let std_out_stream = BroadcastOutputStream::from_stream(
            stdout,
            STDOUT_STREAM_NAME,
            FromStreamOptions {
                chunk_size: stdout_chunk_size,
                channel_capacity: stdout_channel_capacity,
            },
        );
        let std_err_stream = BroadcastOutputStream::from_stream(
            stderr,
            STDERR_STREAM_NAME,
            FromStreamOptions {
                chunk_size: stderr_chunk_size,
                channel_capacity: stderr_channel_capacity,
            },
        );

        let mut this = ProcessHandle {
            name: name.into(),
            child,
            std_in,
            std_out_stream,
            std_err_stream,
            cleanup_on_drop: false,
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
        stdout_config: SingleSubscriberStreamConfig,
        stderr_config: SingleSubscriberStreamConfig,
    ) -> Result<Self, SpawnError> {
        let process_name = name.into();
        Self::prepare_command(&mut cmd)
            .spawn()
            .map(|child| {
                Self::new_from_child_with_piped_io_and_capacity(
                    process_name.clone(),
                    child,
                    stdout_config,
                    stderr_config,
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
        stdout_config: SingleSubscriberStreamConfig,
        stderr_config: SingleSubscriberStreamConfig,
    ) -> Self {
        let std_in = match child.stdin.take() {
            Some(stdin) => Stdin::Open(stdin),
            None => Stdin::Closed,
        };
        let stdout = child
            .stdout
            .take()
            .expect("Child process stdout wasn't captured");
        let stderr = child
            .stderr
            .take()
            .expect("Child process stderr wasn't captured");

        let std_out_stream = SingleSubscriberOutputStream::from_stream(
            stdout,
            STDOUT_STREAM_NAME,
            stdout_config.backpressure_control,
            FromStreamOptions {
                chunk_size: stdout_config.chunk_size,
                channel_capacity: stdout_config.channel_capacity,
            },
        );
        let std_err_stream = SingleSubscriberOutputStream::from_stream(
            stderr,
            STDERR_STREAM_NAME,
            stderr_config.backpressure_control,
            FromStreamOptions {
                chunk_size: stderr_config.chunk_size,
                channel_capacity: stderr_config.channel_capacity,
            },
        );

        let mut this = ProcessHandle {
            name: name.into(),
            child,
            std_in,
            std_out_stream,
            std_err_stream,
            cleanup_on_drop: false,
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // ProcessHandle itself performs the last-resort cleanup while its panic-on-drop guard
            // is armed. Keeping Tokio's unconditional kill-on-drop disabled ensures that
            // `must_not_be_terminated()` can really opt out.
            .kill_on_drop(false)
    }

    /// Returns the OS process ID if the process hasn't exited yet.
    ///
    /// Once this process has been polled to completion this will return None.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    fn try_reap_exit_status(&mut self) -> Result<Option<ExitStatus>, io::Error> {
        match self.child.try_wait() {
            Ok(Some(exit_status)) => {
                self.must_not_be_terminated();
                Ok(Some(exit_status))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn signalling_failed_or_reap(
        &mut self,
        signal: &'static str,
        source: io::Error,
    ) -> Result<ExitStatus, TerminationError> {
        match self.try_reap_exit_status() {
            Ok(Some(exit_status)) => Ok(exit_status),
            Ok(None) | Err(_) => Err(TerminationError::SignallingFailed {
                process_name: self.name.clone(),
                source,
                signal,
            }),
        }
    }

    /// Checks if the process is currently running.
    ///
    /// Returns [`RunningState::Running`] if the process is still running,
    /// [`RunningState::Terminated`] if it has exited, or [`RunningState::Uncertain`]
    /// if the state could not be determined.
    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> RunningState {
        match self.try_reap_exit_status() {
            Ok(None) => RunningState::Running,
            Ok(Some(exit_status)) => RunningState::Terminated(exit_status),
            Err(err) => RunningState::Uncertain(err),
        }
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
    /// # use tokio_process_tools::*;
    /// # use tokio::io::AsyncWriteExt;
    /// # tokio_test::block_on(async {
    /// // Whether we `spawn_broadcast` or `spawn_single_subscriber` does not make a difference here.
    /// let mut process = Process::new(Command::new("cat"))
    ///     .spawn_broadcast()
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
        self.cleanup_on_drop = true;
        self.panic_on_drop = Some(PanicOnDrop::new(
            "tokio_process_tools::ProcessHandle",
            "The process was not terminated.",
            "Call `wait_for_completion` or `terminate` before the type is dropped!",
        ));
    }

    /// Disables the kill/panic-on-drop safeguards for this handle.
    ///
    /// Dropping the handle after calling this method will no longer signal, kill, or panic.
    /// However, this does **not** keep the library-owned stdio pipes alive. If the child still
    /// depends on stdin, stdout, or stderr being open, dropping the handle may still affect it.
    ///
    /// Use plain [`tokio::process::Command`] directly when you need a child process that can
    /// outlive the original handle without depending on captured stdio pipes.
    pub fn must_not_be_terminated(&mut self) {
        self.cleanup_on_drop = false;
        self.defuse_drop_panic();
    }

    fn defuse_drop_panic(&mut self) {
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
        self.defuse_drop_panic();

        if let Some(exit_status) =
            self.try_reap_exit_status()
                .map_err(|source| TerminationError::SignallingFailed {
                    process_name: self.name.clone(),
                    source,
                    signal: "SIGINT",
                })?
        {
            return Ok(exit_status);
        }

        if let Err(err) = self.send_interrupt_signal() {
            return self.signalling_failed_or_reap("SIGINT", err);
        }

        match self.wait_for_completion(Some(interrupt_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(not_terminated_after_sigint) => {
                tracing::warn!(
                    process = %self.name,
                    error = %not_terminated_after_sigint,
                    "Graceful shutdown using SIGINT (or equivalent on current platform) failed. Attempting graceful shutdown using SIGTERM signal."
                );

                if let Err(err) = self.send_terminate_signal() {
                    return self.signalling_failed_or_reap("SIGTERM", err);
                }

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
                                if let Ok(Some(exit_status)) = self.try_reap_exit_status() {
                                    return Ok(exit_status);
                                }
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
    /// [`ProcessHandle::<BroadcastOutputStream>::wait_for_completion_with_output`] to see
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
    pub fn into_inner(mut self) -> (Child, O, O) {
        self.must_not_be_terminated();
        let mut this = std::mem::ManuallyDrop::new(self);

        unsafe {
            let child = std::ptr::read(&this.child);
            let stdout = std::ptr::read(&this.std_out_stream);
            let stderr = std::ptr::read(&this.std_err_stream);

            std::ptr::drop_in_place(&mut this.name);
            // `ChildStdin` is stored separately from `child`, so we still need to drop it here.
            std::ptr::drop_in_place(&mut this.std_in);
            std::ptr::drop_in_place(&mut this.panic_on_drop);

            (child, stdout, stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::prelude::*;
    use std::fs;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;

    use crate::Next;

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
        assert_that!(ran_for.as_secs_f32()).is_close_to(0.1, 0.5);

        // When terminated, we do not get an exit code (unix).
        assert_that!(exit_status.code()).is_none();
    }

    #[tokio::test]
    async fn terminate_returns_normal_exit_when_process_already_exited() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("exit 0");

        let mut handle = crate::Process::new(cmd)
            .name("sh")
            .spawn_broadcast()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let exit_status = handle
            .terminate(Duration::from_millis(50), Duration::from_millis(50))
            .await
            .unwrap();

        assert_that!(exit_status.success()).is_true();
    }

    #[tokio::test]
    async fn test_stdin_write_and_read() {
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
            .spawn_broadcast()
            .unwrap();

        // Verify stdin starts as open.
        assert_that!(process.stdin().is_open()).is_true();

        // Write to stdin.
        let test_data = b"Hello from stdin!\n";
        if let Some(stdin) = process.stdin().as_mut() {
            stdin.write_all(test_data).await.unwrap();
            stdin.flush().await.unwrap();
        }

        // Close stdin to signal EOF.
        process.stdin().close();
        assert_that!(process.stdin().is_open()).is_false();

        // Collect stdout.
        let output = process
            .wait_for_completion_with_output(
                Some(Duration::from_secs(2)),
                LineParsingOptions::default(),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(&output.stdout).has_length(1);
        assert_that!(output.stdout[0].as_str()).is_equal_to("Hello from stdin!");
    }

    #[tokio::test]
    async fn test_stdin_close_sends_eof() {
        // Use `cat` which will exit when stdin is closed.
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
            .spawn_broadcast()
            .unwrap();

        // Close stdin immediately without writing.
        process.stdin().close();
        assert_that!(process.stdin().is_open()).is_false();

        // Process should terminate since it receives EOF.
        let status = process
            .wait_for_completion(Some(Duration::from_secs(2)))
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
    }

    #[tokio::test]
    async fn test_stdin_multiple_writes() {
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
            .spawn_broadcast()
            .unwrap();

        // Write multiple lines.
        if let Some(stdin) = process.stdin().as_mut() {
            stdin.write_all(b"Line 1\n").await.unwrap();
            stdin.write_all(b"Line 2\n").await.unwrap();
            stdin.write_all(b"Line 3\n").await.unwrap();
            stdin.flush().await.unwrap();
        }

        process.stdin().close();

        let output = process
            .wait_for_completion_with_output(
                Some(Duration::from_secs(2)),
                LineParsingOptions::default(),
            )
            .await
            .unwrap();

        assert_that!(&output.stdout).has_length(3);
        assert_that!(output.stdout[0].as_str()).is_equal_to("Line 1");
        assert_that!(output.stdout[1].as_str()).is_equal_to("Line 2");
        assert_that!(output.stdout[2].as_str()).is_equal_to("Line 3");
    }

    #[tokio::test]
    async fn test_shell_command_dispatch() {
        let cmd = tokio::process::Command::new("sh");

        let mut process = crate::Process::new(cmd).spawn_broadcast().unwrap();

        // Monitor output.
        let collector = process
            .stdout()
            .collect_lines_into_vec(LineParsingOptions::default());

        // Send commands to the shell.
        if let Some(stdin) = process.stdin().as_mut() {
            stdin
                .write_all(b"printf 'Hello from shell\\n'\nexit\n")
                .await
                .unwrap();
            stdin.flush().await.unwrap();
        }

        // Wait a bit for output.
        tokio::time::sleep(Duration::from_millis(500)).await;

        process.stdin().close();
        process
            .wait_for_completion(Some(Duration::from_secs(1)))
            .await
            .unwrap();

        let collected = collector.wait().await.unwrap();
        assert_that!(&collected).has_length(1);
        assert_that!(collected[0].as_str()).is_equal_to("Hello from shell");
    }

    #[tokio::test]
    async fn test_into_inner_defuses_panic_guard() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let process = crate::Process::new(cmd)
            .name("sleep")
            .spawn_broadcast()
            .unwrap();

        let (mut child, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn test_into_inner_with_owned_name_drops_owned_string() {
        // Regression test: `into_inner` manually destructures the handle through
        // `ManuallyDrop`. If the `name` field isn't explicitly dropped, a
        // `Cow::Owned(String)` allocation leaks. Forcing the owned variant here
        // exercises the path that the static-`&str` test above doesn't.
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let process = crate::Process::new(cmd)
            .with_name(format!("sleeper-{}", 7))
            .spawn_broadcast()
            .unwrap();

        let (mut child, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn test_defusing_drop_panic_keeps_cleanup_guard_armed() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
            .spawn_broadcast()
            .unwrap();

        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.defuse_drop_panic();

        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(&process.panic_on_drop).is_none();

        process.kill().await.unwrap();
        process.wait_for_completion(None).await.unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_completion_disarms_cleanup_and_panic_guards() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("0.1");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
            .spawn_broadcast()
            .unwrap();

        process
            .wait_for_completion(Some(Duration::from_secs(2)))
            .await
            .unwrap();

        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_must_not_be_terminated_allows_process_to_survive_handle_drop() {
        use nix::errno::Errno;
        use nix::sys::signal::{self, Signal};
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;

        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
            .spawn_broadcast()
            .unwrap();
        let pid = process.id().unwrap();

        process.must_not_be_terminated();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
        drop(process);

        let pid = Pid::from_raw(pid as i32);
        assert_that!(signal::kill(pid, None).is_ok()).is_true();

        signal::kill(pid, Signal::SIGKILL).unwrap();
        match waitpid(pid, None) {
            Ok(_) | Err(Errno::ECHILD) => {}
            Err(err) => panic!("waitpid failed: {err}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_must_not_be_terminated_still_closes_stdin_on_drop() {
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;
        use tempfile::tempdir;

        let temp_dir = tempdir().unwrap();
        let output_file = temp_dir.path().join("stdin-result.txt");
        let output_file = output_file.to_str().unwrap().replace('\'', "'\"'\"'");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("cat >/dev/null; printf eof > '{output_file}'"));

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .spawn_broadcast()
            .unwrap();
        let pid = Pid::from_raw(process.id().unwrap() as i32);

        process.must_not_be_terminated();
        drop(process);

        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || waitpid(pid, None)),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        assert_that!(fs::read_to_string(temp_dir.path().join("stdin-result.txt")).unwrap())
            .is_equal_to("eof");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_must_not_be_terminated_does_not_keep_stdout_pipe_alive() {
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;

        let mut cmd = tokio::process::Command::new("yes");
        cmd.arg("tick");

        let mut process = crate::Process::new(cmd)
            .name("yes")
            .spawn_broadcast()
            .unwrap();
        let pid = Pid::from_raw(process.id().unwrap() as i32);

        process.must_not_be_terminated();
        drop(process);

        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || waitpid(pid, None)),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_completion_with_output_preserves_unterminated_final_line() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf tail");

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .spawn_broadcast()
            .unwrap();

        let output = process
            .wait_for_completion_with_output(
                Some(Duration::from_secs(2)),
                LineParsingOptions::default(),
            )
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout).contains_exactly(["tail"]);
        assert_that!(output.stderr).is_empty();
    }

    #[tokio::test]
    async fn test_inspect_lines_async_preserves_unterminated_final_line() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("printf tail");

        let mut process = crate::Process::new(cmd)
            .name("sh")
            .spawn_broadcast()
            .unwrap();

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_in_task = Arc::clone(&seen);
        let inspector = process.stdout().inspect_lines_async(
            move |line| {
                let seen = Arc::clone(&seen_in_task);
                let line = line.into_owned();
                async move {
                    seen.lock().expect("lock").push(line);
                    Next::Continue
                }
            },
            LineParsingOptions::default(),
        );

        process
            .wait_for_completion(Some(Duration::from_secs(2)))
            .await
            .unwrap();
        inspector.wait().await.unwrap();

        let seen = seen.lock().expect("lock").clone();
        assert_that!(seen).contains_exactly(["tail"]);
    }
}
