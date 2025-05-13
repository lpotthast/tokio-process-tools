use crate::output_stream::broadcast::BroadcastOutputStream;
use crate::output_stream::single_subscriber::{FromStreamOptions, SingleSubscriberOutputStream};
use crate::output_stream::BackpressureControl;
use crate::panic_on_drop::PanicOnDrop;
use crate::terminate_on_drop::TerminateOnDrop;
use crate::{signal, CollectorError, OutputStream, StreamType};
use std::borrow::Cow;
use std::fmt::Debug;
use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use thiserror::Error;
use tokio::process::Child;

#[derive(Debug, Error)]
pub enum TerminationError {
    #[error("Failed to send signal to process: {0}")]
    SignallingFailed(#[from] io::Error),

    #[error(
        "Failed to terminate process. Graceful SIGINT termination failure: {not_terminated_after_sigint}. Graceful SIGTERM termination failure: {not_terminated_after_sigterm}. Forceful termination failure: {not_terminated_after_sigkill}"
    )]
    TerminationFailed {
        not_terminated_after_sigint: io::Error,
        not_terminated_after_sigterm: io::Error,
        not_terminated_after_sigkill: io::Error,
    },
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

/// Errors that can occur when waiting for process output.
#[derive(Debug, Error)]
pub enum WaitError {
    #[error("A general io error occurred")]
    IoError(#[from] io::Error),

    #[error("Collector failed")]
    CollectorFailed(#[from] CollectorError), // TODO: refactor?
}

#[derive(Debug)]
pub struct ProcessHandle<O: OutputStream> {
    pub(crate) name: Cow<'static, str>,
    child: Child,
    std_out_stream: O,
    std_err_stream: O,
    panic_on_drop: Option<PanicOnDrop>,
}

impl ProcessHandle<BroadcastOutputStream> {
    pub fn spawn(
        name: impl Into<Cow<'static, str>>,
        cmd: tokio::process::Command,
    ) -> io::Result<ProcessHandle<BroadcastOutputStream>> {
        Self::spawn_with_capacity(name, cmd, 128, 128)
    }

    pub fn spawn_with_capacity(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> io::Result<ProcessHandle<BroadcastOutputStream>> {
        Self::prepare_command(&mut cmd).spawn().map(|child| {
            Self::new_from_child_with_piped_io_and_capacity(
                name,
                child,
                stdout_channel_capacity,
                stderr_channel_capacity,
            )
        })
    }

    fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        mut child: Child,
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
            BroadcastOutputStream::from_stream(stdout, StreamType::StdOut, stdout_channel_capacity),
            BroadcastOutputStream::from_stream(stderr, StreamType::StdErr, stderr_channel_capacity),
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

    pub async fn wait_with_output(
        &mut self,
    ) -> Result<(ExitStatus, Vec<String>, Vec<String>), WaitError> {
        // TODO: Configurable line collection
        let out_collector = self.std_out_stream.collect_lines_into_vec();
        let err_collector = self.std_err_stream.collect_lines_into_vec();

        let status = self.wait().await?;
        let std_out = out_collector.cancel().await?;
        let std_err = err_collector.cancel().await?;

        Ok((status, std_out, std_err))
    }
}

impl ProcessHandle<SingleSubscriberOutputStream> {
    pub fn spawn(
        name: impl Into<Cow<'static, str>>,
        cmd: tokio::process::Command,
    ) -> io::Result<Self> {
        Self::spawn_with_capacity(name, cmd, 128, 128)
    }

    pub fn spawn_with_capacity(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> io::Result<Self> {
        Self::prepare_command(&mut cmd).spawn().map(|child| {
            Self::new_from_child_with_piped_io_and_capacity(
                name,
                child,
                stdout_channel_capacity,
                stderr_channel_capacity,
            )
        })
    }

    fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        mut child: Child,
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
                StreamType::StdOut,
                BackpressureControl::DropLatestIncomingIfBufferFull,
                FromStreamOptions {
                    channel_capacity: stdout_channel_capacity,
                    ..Default::default()
                },
            ),
            SingleSubscriberOutputStream::from_stream(
                stderr,
                StreamType::StdErr,
                BackpressureControl::DropLatestIncomingIfBufferFull,
                FromStreamOptions {
                    channel_capacity: stderr_channel_capacity,
                    ..Default::default()
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

    pub async fn wait_with_output(
        &mut self,
    ) -> Result<(ExitStatus, Vec<String>, Vec<String>), WaitError> {
        // TODO: Configurable line collection
        let out_collector = self.std_out_stream.collect_lines_into_vec();
        let err_collector = self.std_err_stream.collect_lines_into_vec();

        let status = self.wait().await?;
        let std_out = out_collector.cancel().await?;
        let std_err = err_collector.cancel().await?;

        Ok((status, std_out, std_err))
    }
}

impl<O: OutputStream> ProcessHandle<O> {
    /// On Windows, you can only send `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` to process groups,
    /// which works more like `killpg`. Sending to the current process ID will likely trigger
    /// undefined behavior of sending the event to every process that's attached to the console,
    /// i.e. sending the event to group ID 0. Therefore, we need to create a new process group
    /// for the child process we are about to spawn.
    ///
    /// See: https://stackoverflow.com/questions/44124338/trying-to-implement-signal-ctrl-c-event-in-python3-6
    fn prepare_platform_specifics(
        command: &mut tokio::process::Command,
    ) -> &mut tokio::process::Command {
        #[cfg(windows)]
        {
            use windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

            let flag = if self.graceful_exit {
                CREATE_NEW_PROCESS_GROUP.0
            } else {
                0
            };
            command.creation_flags(flag)
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

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> RunningState {
        match self.child.try_wait() {
            Ok(None) => RunningState::Running,
            Ok(Some(exit_status)) => RunningState::Terminated(exit_status),
            Err(err) => RunningState::Uncertain(err),
        }
    }

    pub fn stdout(&self) -> &O {
        &self.std_out_stream
    }
    pub fn stdout_mut(&mut self) -> &mut O {
        &mut self.std_out_stream
    }

    pub fn stderr(&self) -> &O {
        &self.std_err_stream
    }

    pub fn stderr_mut(&mut self) -> &mut O {
        &mut self.std_err_stream
    }

    /// NOTE: Successfully awaiting the completion of the process will unset the
    /// "must be terminated" setting, as a successfully awaited process is already terminated.
    /// Dropping this `ProcessHandle` after calling `wait` should never lead to a
    /// "must be terminated" panic being raised.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.child.wait().await {
            Ok(status) => {
                self.must_not_be_terminated();
                Ok(status)
            }
            Err(err) => Err(err),
        }
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
        self.panic_on_drop = Some(PanicOnDrop {
            resource_name: "ProcessHandle".into(),
            details: "Call `terminate()` before the type is dropped!".into(),
            armed: true,
        });
    }

    pub fn must_not_be_terminated(&mut self) {
        if let Some(mut it) = self.panic_on_drop.take() {
            it.defuse()
        }
    }

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

    /// Wait for this process to run to completion within `timeout` if set or unbound otherwise.
    async fn await_termination(&mut self, timeout: Option<Duration>) -> io::Result<ExitStatus> {
        match timeout {
            None => self.wait().await,
            Some(timeout) => match tokio::time::timeout(timeout, self.wait()).await {
                Ok(exit_status) => exit_status,
                Err(err) => Err(err.into()),
            },
        }
    }

    /// Manually sed a `SIGINT` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub fn send_interrupt_signal(&mut self) -> Result<(), io::Error> {
        signal::send_interrupt(&self.child)
    }

    /// Manually sed a `SIGTERM` on unix or equivalent on Windows to this process.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    pub fn send_terminate_signal(&mut self) -> Result<(), io::Error> {
        signal::send_terminate(&self.child)
    }

    /// Terminates this process by sending a `SIGINT`, `SIGTERM` or even a `SIGKILL` if the process
    /// doesn't run to completion after receiving any of the first two signals.
    pub async fn terminate(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        // Whether or not this function will ultimately succeed, we tried to terminate the process.
        // Dropping this handle should not create any on-drop error anymore.
        self.must_not_be_terminated();

        self.send_interrupt_signal()
            .map_err(TerminationError::SignallingFailed)?;

        match self.await_termination(Some(interrupt_timeout)).await {
            Ok(exit_status) => Ok(exit_status),
            Err(not_terminated_after_sigint) => {
                tracing::warn!(
                    process = %self.name,
                    error = %not_terminated_after_sigint,
                    "Graceful shutdown using SIGINT (or equivalent on current platform) failed. Attempting graceful shutdown using SIGTERM signal."
                );

                self.send_terminate_signal()
                    .map_err(TerminationError::SignallingFailed)?;

                match self.await_termination(Some(terminate_timeout)).await {
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
                                // and therefore wait (at max) for a fixed three seconds after any
                                // SIGKILL event.
                                match self.await_termination(Some(Duration::from_secs(3))).await {
                                    Ok(exit_status) => Ok(exit_status),
                                    Err(not_terminated_after_sigkill) => {
                                        // Unlikely. See the note above.
                                        tracing::error!(
                                            "Process, having custom name '{}', did not terminate after receiving a SIGINT, SIGTERM and SIGKILL event (or equivalent on the current platform). Something must have gone horribly wrong... Process may still be running. Manual intervention and investigation required!",
                                            self.name
                                        );
                                        Err(TerminationError::TerminationFailed {
                                            not_terminated_after_sigint,
                                            not_terminated_after_sigterm,
                                            not_terminated_after_sigkill,
                                        })
                                    }
                                }
                            }
                            Err(not_terminated_after_sigkill) => {
                                tracing::error!(
                                    process = %self.name,
                                    error = %not_terminated_after_sigkill,
                                    "Forceful shutdown using SIGKILL (or equivalent on current platform) failed. Process may still be running. Manual intervention required!"
                                );

                                Err(TerminationError::TerminationFailed {
                                    not_terminated_after_sigint,
                                    not_terminated_after_sigterm,
                                    not_terminated_after_sigkill,
                                })
                            }
                        }
                    }
                }
            }
        }
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await
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
        let mut handle = ProcessHandle::<BroadcastOutputStream>::spawn("sleep", cmd).unwrap();
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
