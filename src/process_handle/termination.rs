use super::ProcessHandle;
use crate::error::{
    TerminationAttemptError, TerminationAttemptOperation, TerminationAttemptPhase, TerminationError,
};
use crate::output_stream::OutputStream;
use crate::signal;
use std::borrow::Cow;
use std::error::Error;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

/// Maximum time to wait for process termination after forceful kill.
///
/// This is a safety timeout since forceful kill should terminate processes immediately,
/// but there are rare cases where even forceful kill may not work.
const FORCE_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminationOutcome {
    pub(super) exit_status: ExitStatus,
    pub(super) output_collection_timeout_extension: Duration,
}

impl TerminationOutcome {
    fn graceful_success(exit_status: ExitStatus) -> Self {
        Self {
            exit_status,
            output_collection_timeout_extension: Duration::ZERO,
        }
    }

    fn force_kill_success(exit_status: ExitStatus) -> Self {
        Self {
            exit_status,
            output_collection_timeout_extension: FORCE_KILL_WAIT_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum GracefulTerminationPhase {
    Interrupt,
    Terminate,
}

impl GracefulTerminationPhase {
    fn attempt_phase(self) -> TerminationAttemptPhase {
        match self {
            Self::Interrupt => TerminationAttemptPhase::Interrupt,
            Self::Terminate => TerminationAttemptPhase::Terminate,
        }
    }
}

#[derive(Debug, Default)]
struct TerminationDiagnostics {
    attempt_errors: Vec<TerminationAttemptError>,
}

impl TerminationDiagnostics {
    fn record_preflight_status_error(&mut self, error: impl Error + 'static) {
        self.record(
            TerminationAttemptPhase::Preflight,
            TerminationAttemptOperation::CheckStatus,
            None,
            error,
        );
    }

    fn record_graceful_signal_error(
        &mut self,
        phase: GracefulTerminationPhase,
        signal_name: &'static str,
        error: impl Error + 'static,
    ) {
        self.record(
            phase.attempt_phase(),
            TerminationAttemptOperation::SendSignal,
            Some(signal_name),
            error,
        );
    }

    fn record_graceful_wait_error(
        &mut self,
        phase: GracefulTerminationPhase,
        signal_name: &'static str,
        error: impl Error + 'static,
    ) {
        self.record(
            phase.attempt_phase(),
            TerminationAttemptOperation::WaitForExit,
            Some(signal_name),
            error,
        );
    }

    fn record_graceful_status_error(
        &mut self,
        phase: GracefulTerminationPhase,
        signal_name: &'static str,
        error: impl Error + 'static,
    ) {
        self.record(
            phase.attempt_phase(),
            TerminationAttemptOperation::CheckStatus,
            Some(signal_name),
            error,
        );
    }

    fn record_kill_signal_error(&mut self, error: impl Error + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::SendSignal,
            Some(signal::KILL_SIGNAL_NAME),
            error,
        );
    }

    fn record_kill_wait_error(&mut self, error: impl Error + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::WaitForExit,
            Some(signal::KILL_SIGNAL_NAME),
            error,
        );
    }

    fn record_kill_status_error(&mut self, error: impl Error + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::CheckStatus,
            Some(signal::KILL_SIGNAL_NAME),
            error,
        );
    }

    fn record(
        &mut self,
        phase: TerminationAttemptPhase,
        operation: TerminationAttemptOperation,
        signal_name: Option<&'static str>,
        error: impl Error + 'static,
    ) {
        self.attempt_errors.push(TerminationAttemptError {
            phase,
            operation,
            signal_name,
            source: Box::new(error),
        });
    }

    #[must_use]
    fn into_termination_failed(self, process_name: Cow<'static, str>) -> TerminationError {
        assert!(
            !self.attempt_errors.is_empty(),
            "into_termination_failed must not be used when no error was recorded!",
        );

        TerminationError::TerminationFailed {
            process_name,
            attempt_errors: self.attempt_errors,
        }
    }
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Manually send an interrupt signal to this process.
    ///
    /// This is `SIGINT` on Unix and the targetable graceful Windows equivalent
    /// (`CTRL_BREAK_EVENT`) on Windows.
    ///
    /// If the process has already exited, this reaps it and returns `Ok(())` instead of
    /// attempting to signal a stale PID or process group. If the signal send fails because the
    /// child exited after the preflight check, this also reaps it and returns `Ok(())`.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// # Errors
    ///
    /// Returns an error if the process status could not be checked or if the platform signal could
    /// not be sent.
    pub fn send_interrupt_signal(&mut self) -> Result<(), io::Error> {
        self.send_signal_with_preflight_reap(signal::send_interrupt)
    }

    /// Manually send a termination signal to this process.
    ///
    /// This is `SIGTERM` on Unix and `CTRL_BREAK_EVENT` on Windows.
    ///
    /// If the process has already exited, this reaps it and returns `Ok(())` instead of
    /// attempting to signal a stale PID or process group. If the signal send fails because the
    /// child exited after the preflight check, this also reaps it and returns `Ok(())`.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// # Errors
    ///
    /// Returns an error if the process status could not be checked or if the platform signal could
    /// not be sent.
    pub fn send_terminate_signal(&mut self) -> Result<(), io::Error> {
        self.send_signal_with_preflight_reap(signal::send_terminate)
    }

    /// Terminates this process by sending platform graceful shutdown signals first, then killing
    /// the process if it does not complete after receiving them.
    ///
    /// On Unix this means `SIGINT`, then `SIGTERM`, then `SIGKILL`. On Windows, targeted
    /// `CTRL_C_EVENT` delivery is not supported for child process groups, so both graceful phases
    /// use `CTRL_BREAK_EVENT` before falling back to `TerminateProcess`.
    /// After the graceful phases time out, termination performs one additional fixed 3-second wait
    /// for the force-kill result.
    ///
    /// When this method returns `Ok`, the process has reached a terminal state and this handle's
    /// drop cleanup and panic guards are disarmed, so the handle can be dropped safely afterward.
    ///
    /// If this method returns `Err`, or if the returned future is canceled before completion, the
    /// guards remain armed. Dropping the handle will still attempt best-effort cleanup and panic
    /// unless the process is later successfully awaited, terminated, killed, or explicitly detached
    /// with [`ProcessHandle::must_not_be_terminated`].
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if signalling or waiting for process termination fails.
    pub async fn terminate(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        self.terminate_detailed(interrupt_timeout, terminate_timeout)
            .await
            .map(|outcome| outcome.exit_status)
    }

    pub(super) async fn terminate_detailed(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<TerminationOutcome, TerminationError> {
        self.terminate_inner_with_preflight_reaper(
            interrupt_timeout,
            terminate_timeout,
            Self::try_reap_exit_status,
            Self::send_interrupt_signal_raw,
            Self::send_terminate_signal_raw,
        )
        .await
    }

    #[cfg(test)]
    async fn terminate_inner<InterruptSignalSender, TerminateSignalSender>(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        send_interrupt_signal: InterruptSignalSender,
        send_terminate_signal: TerminateSignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        InterruptSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
        TerminateSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.terminate_inner_detailed(
            interrupt_timeout,
            terminate_timeout,
            send_interrupt_signal,
            send_terminate_signal,
        )
        .await
        .map(|outcome| outcome.exit_status)
    }

    #[cfg(test)]
    async fn terminate_inner_detailed<InterruptSignalSender, TerminateSignalSender>(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        send_interrupt_signal: InterruptSignalSender,
        send_terminate_signal: TerminateSignalSender,
    ) -> Result<TerminationOutcome, TerminationError>
    where
        InterruptSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
        TerminateSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.terminate_inner_with_preflight_reaper(
            interrupt_timeout,
            terminate_timeout,
            Self::try_reap_exit_status,
            send_interrupt_signal,
            send_terminate_signal,
        )
        .await
    }

    async fn terminate_inner_with_preflight_reaper<
        PreflightReaper,
        InterruptSignalSender,
        TerminateSignalSender,
    >(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        mut try_reap_exit_status: PreflightReaper,
        mut send_interrupt_signal: InterruptSignalSender,
        mut send_terminate_signal: TerminateSignalSender,
    ) -> Result<TerminationOutcome, TerminationError>
    where
        PreflightReaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
        InterruptSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
        TerminateSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        let result = 'termination: {
            let mut diagnostics = TerminationDiagnostics::default();

            match try_reap_exit_status(self) {
                Ok(Some(exit_status)) => {
                    break 'termination Ok(TerminationOutcome::graceful_success(exit_status));
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        process = %self.name,
                        signal = signal::INTERRUPT_SIGNAL_NAME,
                        error = %err,
                        "Could not determine process state before termination. Attempting interrupt signal."
                    );
                    diagnostics.record_preflight_status_error(err);
                }
            }
            if let Some(exit_status) = self
                .attempt_graceful_phase(
                    signal::INTERRUPT_SIGNAL_NAME,
                    signal::TERMINATE_SIGNAL_NAME,
                    interrupt_timeout,
                    GracefulTerminationPhase::Interrupt,
                    &mut diagnostics,
                    &mut send_interrupt_signal,
                )
                .await
            {
                break 'termination Ok(exit_status);
            }

            if let Some(exit_status) = self
                .attempt_graceful_phase(
                    signal::TERMINATE_SIGNAL_NAME,
                    signal::KILL_SIGNAL_NAME,
                    terminate_timeout,
                    GracefulTerminationPhase::Terminate,
                    &mut diagnostics,
                    &mut send_terminate_signal,
                )
                .await
            {
                break 'termination Ok(exit_status);
            }

            self.attempt_forceful_kill(diagnostics).await
        };

        self.disarm_after_successful_termination(result)
    }

    fn send_signal_with_preflight_reap<SignalSender>(
        &mut self,
        send_signal: SignalSender,
    ) -> Result<(), io::Error>
    where
        SignalSender: FnOnce(&tokio::process::Child) -> Result<(), io::Error>,
    {
        self.send_signal_with_reaper(send_signal, Self::try_reap_exit_status)
    }

    fn send_signal_with_reaper<SignalSender, Reaper>(
        &mut self,
        send_signal: SignalSender,
        mut try_reap_exit_status: Reaper,
    ) -> Result<(), io::Error>
    where
        SignalSender: FnOnce(&tokio::process::Child) -> Result<(), io::Error>,
        Reaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
    {
        match try_reap_exit_status(self)? {
            Some(_) => Ok(()),
            None => match send_signal(&self.child) {
                Ok(()) => Ok(()),
                Err(signal_error) => match try_reap_exit_status(self) {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) | Err(_) => Err(signal_error),
                },
            },
        }
    }

    fn send_interrupt_signal_raw(&mut self) -> Result<(), io::Error> {
        signal::send_interrupt(&self.child)
    }

    fn send_terminate_signal_raw(&mut self) -> Result<(), io::Error> {
        signal::send_terminate(&self.child)
    }

    fn disarm_after_successful_termination<T>(
        &mut self,
        result: Result<T, TerminationError>,
    ) -> Result<T, TerminationError> {
        if result.is_ok() {
            self.must_not_be_terminated();
        }

        result
    }

    async fn attempt_graceful_phase<SignalSender>(
        &mut self,
        signal_name: &'static str,
        next_signal_name: &'static str,
        timeout: Duration,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
        send_signal: &mut SignalSender,
    ) -> Option<TerminationOutcome>
    where
        SignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        match send_signal(self) {
            Ok(()) => {
                self.wait_after_graceful_signal(
                    signal_name,
                    next_signal_name,
                    timeout,
                    phase,
                    diagnostics,
                )
                .await
            }
            Err(err) => {
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %err,
                    "Graceful shutdown signal could not be sent. Attempting next shutdown phase."
                );
                diagnostics.record_graceful_signal_error(phase, signal_name, err);
                self.try_reap_after_failed_signal(signal_name, phase, diagnostics)
            }
        }
    }

    async fn wait_after_graceful_signal(
        &mut self,
        signal_name: &'static str,
        next_signal_name: &'static str,
        timeout: Duration,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
    ) -> Option<TerminationOutcome> {
        match self.wait_for_completion_inner(Some(timeout)).await {
            Ok(exit_status) => Some(TerminationOutcome::graceful_success(exit_status)),
            Err(not_terminated) => {
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %not_terminated,
                    "Graceful shutdown signal timed out. Attempting next shutdown phase."
                );
                diagnostics.record_graceful_wait_error(phase, signal_name, not_terminated);
                None
            }
        }
    }

    fn try_reap_after_failed_signal(
        &mut self,
        signal_name: &'static str,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
    ) -> Option<TerminationOutcome> {
        match self.try_reap_exit_status() {
            Ok(Some(exit_status)) => Some(TerminationOutcome::graceful_success(exit_status)),
            Ok(None) => None,
            Err(reap_error) => {
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    error = %reap_error,
                    "Could not determine process state after graceful signal send failed."
                );
                diagnostics.record_graceful_status_error(phase, signal_name, reap_error);
                None
            }
        }
    }

    async fn attempt_forceful_kill(
        &mut self,
        mut diagnostics: TerminationDiagnostics,
    ) -> Result<TerminationOutcome, TerminationError> {
        match self.child.start_kill() {
            Ok(()) => {
                // Note: A forceful kill should typically (somewhat) immediately lead to
                // termination of the process. But there are cases in which even a forceful kill
                // does not / cannot / will not kill a process. We do not want to wait indefinitely
                // in case this happens and therefore wait (at max) for a fixed duration after any
                // kill.
                match self
                    .wait_for_completion_inner(Some(FORCE_KILL_WAIT_TIMEOUT))
                    .await
                {
                    Ok(exit_status) => Ok(TerminationOutcome::force_kill_success(exit_status)),
                    Err(not_terminated_after_kill) => {
                        // Unlikely. See the note above.
                        tracing::error!(
                            process = %self.name,
                            interrupt_signal = signal::INTERRUPT_SIGNAL_NAME,
                            terminate_signal = signal::TERMINATE_SIGNAL_NAME,
                            kill_signal = signal::KILL_SIGNAL_NAME,
                            "Process did not terminate after all termination attempts. Process may still be running. Manual intervention and investigation required!"
                        );
                        diagnostics.record_kill_wait_error(not_terminated_after_kill);
                        Err(diagnostics.into_termination_failed(self.name.clone()))
                    }
                }
            }
            Err(kill_error) => {
                tracing::error!(
                    process = %self.name,
                    error = %kill_error,
                    signal = signal::KILL_SIGNAL_NAME,
                    "Forceful shutdown failed. Process may still be running. Manual intervention required!"
                );
                diagnostics.record_kill_signal_error(kill_error);

                match self.try_reap_exit_status() {
                    Ok(Some(exit_status)) => {
                        return Ok(TerminationOutcome::graceful_success(exit_status));
                    }
                    Ok(None) => {}
                    Err(reap_error) => {
                        tracing::warn!(
                            process = %self.name,
                            signal = signal::KILL_SIGNAL_NAME,
                            error = %reap_error,
                            "Could not determine process state after forceful shutdown failed."
                        );
                        diagnostics.record_kill_status_error(reap_error);
                    }
                }

                Err(diagnostics.into_termination_failed(self.name.clone()))
            }
        }
    }

    /// Forces the process to exit. Most users should call [`ProcessHandle::terminate`] instead.
    ///
    /// This is equivalent to sending `SIGKILL` on Unix or calling `TerminateProcess` on Windows,
    /// followed by wait.
    /// Any still-open stdin handle is closed before Tokio performs that kill-and-wait sequence,
    /// matching [`tokio::process::Child::kill`] semantics.
    /// A successful call waits for the child to exit and disarms the drop cleanup and panic guards,
    /// so the handle can be dropped safely afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if Tokio cannot kill or wait for the child process.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.stdin().close();
        self.child.kill().await?;
        self.must_not_be_terminated();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::BroadcastOutputStream;
    use crate::panic_on_drop::PanicOnDrop;
    use crate::test_support::{ScriptedOutput, long_running_command};
    use crate::{
        BestEffortDelivery, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NoReplay,
        NumBytesExt,
    };
    use assertr::prelude::*;

    #[cfg(windows)]
    const CTRL_BREAK_HELPER_EXIT_CODE: i32 = 77;

    #[cfg(windows)]
    const CTRL_BREAK_HELPER_SOURCE: &str = r#"
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

const CTRL_BREAK_EVENT: u32 = 1;
const WAITING_EVENT: u32 = u32::MAX;
const TRUE: i32 = 1;
const FALSE: i32 = 0;

static RECEIVED_EVENT: AtomicU32 = AtomicU32::new(WAITING_EVENT);

type HandlerRoutine = Option<unsafe extern "system" fn(u32) -> i32>;

#[link(name = "Kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler_routine: HandlerRoutine, add: i32) -> i32;
}

unsafe extern "system" fn handle_control_event(ctrl_type: u32) -> i32 {
    if ctrl_type == CTRL_BREAK_EVENT {
        RECEIVED_EVENT.store(ctrl_type, Ordering::SeqCst);
        TRUE
    } else {
        FALSE
    }
}

fn main() {
    let registered = unsafe { SetConsoleCtrlHandler(Some(handle_control_event), TRUE) };
    if registered == FALSE {
        eprintln!("handler-registration-failed");
        std::process::exit(2);
    }

    println!("ready");
    std::io::stdout().flush().unwrap();

    loop {
        if RECEIVED_EVENT.load(Ordering::SeqCst) == CTRL_BREAK_EVENT {
            println!("ctrl-break");
            std::io::stdout().flush().unwrap();
            std::process::exit(77);
        }

        thread::sleep(Duration::from_millis(10));
    }
}
"#;

    #[cfg(windows)]
    #[cfg(windows)]
    fn compile_ctrl_break_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let source_path = dir.join("ctrl_break_helper.rs");
        let exe_path = dir.join("ctrl_break_helper.exe");
        std::fs::write(&source_path, CTRL_BREAK_HELPER_SOURCE).unwrap();

        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = std::process::Command::new(rustc)
            .arg("--edition=2024")
            .arg(&source_path)
            .arg("-o")
            .arg(&exe_path)
            .output()
            .unwrap();

        assert_that!(output.status.success())
            .with_detail_message(format!(
                "failed to compile ctrl-break helper\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ))
            .is_true();

        exe_path
    }

    fn immediately_exiting_command() -> tokio::process::Command {
        ScriptedOutput::builder().build()
    }

    fn spawn_long_running_process()
    -> ProcessHandle<BroadcastOutputStream<BestEffortDelivery, NoReplay>> {
        crate::Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
    }

    fn spawn_immediately_exiting_process()
    -> ProcessHandle<BroadcastOutputStream<BestEffortDelivery, NoReplay>> {
        crate::Process::new(immediately_exiting_command())
            .name("immediate-exit")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
    }

    fn successful_exit_status() -> ExitStatus {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            ExitStatus::from_raw(0)
        }

        #[cfg(all(not(windows), not(unix)))]
        {
            unimplemented!("test exit status construction is only implemented on Unix and Windows")
        }
    }

    #[tokio::test]
    async fn terminate_falls_back_to_kill_when_graceful_signal_sends_fail() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut process = spawn_long_running_process();
        let terminate_attempted = Arc::new(AtomicBool::new(false));
        let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

        let outcome = process
            .terminate_inner_detailed(
                Duration::from_millis(10),
                Duration::from_millis(10),
                |_| Err(io::Error::other("injected interrupt signal failure")),
                move |_| {
                    terminate_attempted_in_sender.store(true, Ordering::SeqCst);
                    Err(io::Error::other("injected terminate signal failure"))
                },
            )
            .await
            .unwrap();

        assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
        assert_that!(outcome.exit_status.success()).is_false();
        assert_that!(outcome.output_collection_timeout_extension)
            .is_equal_to(FORCE_KILL_WAIT_TIMEOUT);
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn terminate_detailed_reports_no_output_extension_when_graceful_phase_succeeds() {
        let mut process = spawn_long_running_process();

        let outcome = process
            .terminate_detailed(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();

        assert_that!(outcome.exit_status.success()).is_false();
        assert_that!(outcome.output_collection_timeout_extension).is_equal_to(Duration::ZERO);
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn preflight_status_error_does_not_stop_termination_escalation() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut process = spawn_long_running_process();
        let interrupt_attempted = Arc::new(AtomicBool::new(false));
        let interrupt_attempted_in_sender = Arc::clone(&interrupt_attempted);
        let terminate_attempted = Arc::new(AtomicBool::new(false));
        let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

        let outcome = process
            .terminate_inner_with_preflight_reaper(
                Duration::from_millis(10),
                Duration::from_millis(10),
                |_| Err(io::Error::other("injected preflight status failure")),
                move |_| {
                    interrupt_attempted_in_sender.store(true, Ordering::SeqCst);
                    Err(io::Error::other("injected interrupt signal failure"))
                },
                move |_| {
                    terminate_attempted_in_sender.store(true, Ordering::SeqCst);
                    Err(io::Error::other("injected terminate signal failure"))
                },
            )
            .await
            .unwrap();

        assert_that!(interrupt_attempted.load(Ordering::SeqCst)).is_true();
        assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
        assert_that!(outcome.exit_status.success()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn canceled_terminate_keeps_drop_guards_armed() {
        let mut process = spawn_long_running_process();

        let result = tokio::time::timeout(
            Duration::from_millis(50),
            process.terminate_inner(
                Duration::from_secs(5),
                Duration::from_secs(5),
                |_| Ok(()),
                |_| Ok(()),
            ),
        )
        .await;

        assert_that!(result.is_err()).is_true();
        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.kill().await.unwrap();
    }

    #[tokio::test]
    async fn failed_terminate_result_keeps_drop_guards_armed() {
        let mut process = spawn_long_running_process();

        let result = process.disarm_after_successful_termination::<ExitStatus>(Err(
            TerminationError::TerminationFailed {
                process_name: Cow::Borrowed("long-running"),
                attempt_errors: vec![TerminationAttemptError {
                    phase: TerminationAttemptPhase::Kill,
                    operation: TerminationAttemptOperation::SendSignal,
                    signal_name: Some(signal::KILL_SIGNAL_NAME),
                    source: Box::new(io::Error::other("synthetic termination failure")),
                }],
            },
        ));

        assert_that!(result.is_err()).is_true();
        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.kill().await.unwrap();
    }

    #[test]
    fn termination_failed_preserves_recorded_attempt_errors_in_order() {
        let mut diagnostics = TerminationDiagnostics::default();
        diagnostics
            .record_preflight_status_error(io::Error::other("injected preflight status failure"));
        diagnostics.record_graceful_signal_error(
            GracefulTerminationPhase::Interrupt,
            signal::INTERRUPT_SIGNAL_NAME,
            io::Error::new(io::ErrorKind::Interrupted, "injected interrupt failure"),
        );
        diagnostics.record_graceful_status_error(
            GracefulTerminationPhase::Interrupt,
            signal::INTERRUPT_SIGNAL_NAME,
            io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected interrupt status failure",
            ),
        );
        diagnostics.record_graceful_wait_error(
            GracefulTerminationPhase::Terminate,
            signal::TERMINATE_SIGNAL_NAME,
            io::Error::new(io::ErrorKind::TimedOut, "injected terminate wait failure"),
        );
        diagnostics.record_kill_signal_error(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected kill failure",
        ));
        diagnostics.record_kill_wait_error(io::Error::new(
            io::ErrorKind::TimedOut,
            "final wait failure",
        ));

        let error = diagnostics.into_termination_failed(Cow::Borrowed("diagnostic-test"));

        let TerminationError::TerminationFailed { attempt_errors, .. } = error;

        assert_that!(attempt_errors.len()).is_equal_to(6);
        assert_attempt_error(
            &attempt_errors[0],
            TerminationAttemptPhase::Preflight,
            TerminationAttemptOperation::CheckStatus,
            None,
            io::ErrorKind::Other,
            "injected preflight status failure",
        );
        assert_attempt_error(
            &attempt_errors[1],
            TerminationAttemptPhase::Interrupt,
            TerminationAttemptOperation::SendSignal,
            Some(signal::INTERRUPT_SIGNAL_NAME),
            io::ErrorKind::Interrupted,
            "injected interrupt failure",
        );
        assert_attempt_error(
            &attempt_errors[2],
            TerminationAttemptPhase::Interrupt,
            TerminationAttemptOperation::CheckStatus,
            Some(signal::INTERRUPT_SIGNAL_NAME),
            io::ErrorKind::ConnectionReset,
            "injected interrupt status failure",
        );
        assert_attempt_error(
            &attempt_errors[3],
            TerminationAttemptPhase::Terminate,
            TerminationAttemptOperation::WaitForExit,
            Some(signal::TERMINATE_SIGNAL_NAME),
            io::ErrorKind::TimedOut,
            "injected terminate wait failure",
        );
        assert_attempt_error(
            &attempt_errors[4],
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::SendSignal,
            Some(signal::KILL_SIGNAL_NAME),
            io::ErrorKind::PermissionDenied,
            "injected kill failure",
        );
        assert_attempt_error(
            &attempt_errors[5],
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::WaitForExit,
            Some(signal::KILL_SIGNAL_NAME),
            io::ErrorKind::TimedOut,
            "final wait failure",
        );
    }

    fn assert_attempt_error(
        attempt_error: &TerminationAttemptError,
        expected_phase: TerminationAttemptPhase,
        expected_operation: TerminationAttemptOperation,
        expected_signal_name: Option<&'static str>,
        expected_kind: io::ErrorKind,
        expected_message: &str,
    ) {
        assert_that!(attempt_error.phase).is_equal_to(expected_phase);
        assert_that!(attempt_error.operation).is_equal_to(expected_operation);
        assert_that!(attempt_error.signal_name).is_equal_to(expected_signal_name);

        let io_error = attempt_error
            .source
            .downcast_ref::<io::Error>()
            .expect("diagnostic should preserve the original io::Error");

        assert_that!(io_error.kind()).is_equal_to(expected_kind);
        assert_that!(io_error.to_string().as_str()).contains(expected_message);
    }

    #[tokio::test]
    async fn terminate_stops_process() {
        let started_at = jiff::Zoned::now();
        let mut handle = crate::Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let exit_status = handle
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();
        let terminated_at = jiff::Zoned::now();

        let ran_for = started_at.duration_until(&terminated_at);
        assert_that!(ran_for.as_secs_f32()).is_close_to(0.1, 0.5);

        assert_that!(exit_status.code()).is_none();
        assert_that!(exit_status.success()).is_false();
        assert_that!(handle.cleanup_on_drop).is_false();
        assert_that!(&handle.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn terminate_returns_normal_exit_when_process_already_exited() {
        let mut handle = spawn_immediately_exiting_process();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let exit_status = handle
            .terminate(Duration::from_millis(50), Duration::from_millis(50))
            .await
            .unwrap();

        assert_that!(exit_status.success()).is_true();
    }

    #[tokio::test]
    async fn send_interrupt_signal_reaps_already_exited_child_before_signalling() {
        let mut process = spawn_immediately_exiting_process();

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_that!(process.id()).is_some();

        process.send_interrupt_signal().unwrap();

        assert_that!(process.id()).is_none();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn send_terminate_signal_reaps_already_exited_child_before_signalling() {
        let mut process = spawn_immediately_exiting_process();

        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_that!(process.id()).is_some();

        process.send_terminate_signal().unwrap();

        assert_that!(process.id()).is_none();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();
    }

    #[tokio::test]
    async fn send_signal_reaps_exit_observed_after_signal_failure() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        let result = process.send_signal_with_reaper(
            |_| {
                signal_attempts += 1;
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "injected signal failure",
                ))
            },
            |process| {
                reap_attempts += 1;
                match reap_attempts {
                    1 => Ok(None),
                    2 => {
                        process.must_not_be_terminated();
                        Ok(Some(successful_exit_status()))
                    }
                    _ => panic!("unexpected reap attempt"),
                }
            },
        );

        assert_that!(result).is_ok();
        assert_that!(signal_attempts).is_equal_to(1);
        assert_that!(reap_attempts).is_equal_to(2);
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();

        process.kill().await.unwrap();
    }

    #[tokio::test]
    async fn send_signal_returns_original_error_when_child_is_still_running_after_signal_failure() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        let error = process
            .send_signal_with_reaper(
                |_| {
                    signal_attempts += 1;
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected signal failure",
                    ))
                },
                |_| {
                    reap_attempts += 1;
                    Ok(None)
                },
            )
            .unwrap_err();

        assert_that!(error.kind()).is_equal_to(io::ErrorKind::PermissionDenied);
        assert_that!(error.to_string().as_str()).contains("injected signal failure");
        assert_that!(signal_attempts).is_equal_to(1);
        assert_that!(reap_attempts).is_equal_to(2);
        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.kill().await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn windows_interrupt_signal_delivers_targeted_ctrl_break() {
        let temp_dir = tempfile::tempdir().unwrap();
        let helper = compile_ctrl_break_helper(temp_dir.path());
        let cmd = tokio::process::Command::new(helper);
        let mut process = crate::Process::new(cmd)
            .name("ctrl-break-helper")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
            .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

        let ready = process
            .stdout()
            .wait_for_line_with_timeout(
                |line| line == "ready",
                crate::LineParsingOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_that!(ready).is_equal_to(crate::WaitForLineResult::Matched);

        process.send_interrupt_signal().unwrap();

        let exit_status = process
            .wait_for_completion(Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_that!(exit_status.code())
            .is_some()
            .is_equal_to(CTRL_BREAK_HELPER_EXIT_CODE);
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn windows_terminate_exits_during_first_graceful_phase() {
        let temp_dir = tempfile::tempdir().unwrap();
        let helper = compile_ctrl_break_helper(temp_dir.path());
        let cmd = tokio::process::Command::new(helper);
        let mut process = crate::Process::new(cmd)
            .name("ctrl-break-helper")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
            .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

        let ready = process
            .stdout()
            .wait_for_line_with_timeout(
                |line| line == "ready",
                crate::LineParsingOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_that!(ready).is_equal_to(crate::WaitForLineResult::Matched);

        let started_at = std::time::Instant::now();
        let exit_status = process
            .terminate(Duration::from_secs(2), Duration::from_secs(5))
            .await
            .unwrap();

        assert_that!(started_at.elapsed() < Duration::from_secs(2)).is_true();
        assert_that!(exit_status.code())
            .is_some()
            .is_equal_to(CTRL_BREAK_HELPER_EXIT_CODE);
    }

    #[tokio::test]
    async fn test_kill_disarms_cleanup_and_panic_guards() {
        let mut process = crate::Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        assert_that!(process.stdin().is_open()).is_true();
        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.kill().await.unwrap();

        assert_that!(process.stdin().is_open()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();

        drop(process);
    }
}
