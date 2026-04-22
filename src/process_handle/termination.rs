use super::ProcessHandle;
use crate::error::TerminationError;
use crate::output_stream::OutputStream;
use crate::signal;
use std::borrow::Cow;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

/// Maximum time to wait for process termination after forceful kill.
///
/// This is a safety timeout since forceful kill should terminate processes immediately,
/// but there are rare cases where even forceful kill may not work.
const SIGKILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy)]
enum GracefulTerminationPhase {
    Interrupt,
    Terminate,
}

#[derive(Debug, Default)]
struct TerminationDiagnostics {
    interrupt: Vec<String>,
    terminate: Vec<String>,
    kill: Vec<String>,
}

impl TerminationDiagnostics {
    fn record_graceful(&mut self, phase: GracefulTerminationPhase, error: impl Into<String>) {
        match phase {
            GracefulTerminationPhase::Interrupt => self.interrupt.push(error.into()),
            GracefulTerminationPhase::Terminate => self.terminate.push(error.into()),
        }
    }

    fn record_kill(&mut self, error: impl Into<String>) {
        self.kill.push(error.into());
    }

    #[must_use]
    fn phase_summary(phase_name: &str, errors: &[String]) -> String {
        if errors.is_empty() {
            format!("{phase_name} phase did not record an error")
        } else {
            errors.join("; ")
        }
    }

    #[must_use]
    fn into_termination_failed(
        self,
        process_name: Cow<'static, str>,
        kill_error_kind: io::ErrorKind,
    ) -> TerminationError {
        let sigkill_error =
            io::Error::new(kill_error_kind, Self::phase_summary("kill", &self.kill));

        TerminationError::TerminationFailed {
            process_name,
            sigint_error: Self::phase_summary("interrupt", &self.interrupt),
            sigterm_error: Self::phase_summary("terminate", &self.terminate),
            sigkill_error,
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
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform signal could not be sent.
    pub fn send_interrupt_signal(&mut self) -> Result<(), io::Error> {
        signal::send_interrupt(&self.child)
    }

    /// Manually send a termination signal to this process.
    ///
    /// This is `SIGTERM` on Unix and `CTRL_BREAK_EVENT` on Windows.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform signal could not be sent.
    pub fn send_terminate_signal(&mut self) -> Result<(), io::Error> {
        signal::send_terminate(&self.child)
    }

    /// Terminates this process by sending platform graceful shutdown signals first, then killing
    /// the process if it does not complete after receiving them.
    ///
    /// On Unix this means `SIGINT`, then `SIGTERM`, then `SIGKILL`. On Windows, targeted
    /// `CTRL_C_EVENT` delivery is not supported for child process groups, so both graceful phases
    /// use `CTRL_BREAK_EVENT` before falling back to `TerminateProcess`.
    ///
    /// This handle can be dropped safely after this call returned, no matter the outcome.
    /// We accept that in extremely rare cases, failed forceful kill may leave a rogue process
    /// behind.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if signalling or waiting for process termination fails.
    pub async fn terminate(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
    ) -> Result<ExitStatus, TerminationError> {
        self.terminate_inner(
            interrupt_timeout,
            terminate_timeout,
            Self::send_interrupt_signal,
            Self::send_terminate_signal,
        )
        .await
    }

    async fn terminate_inner<InterruptSignalSender, TerminateSignalSender>(
        &mut self,
        interrupt_timeout: Duration,
        terminate_timeout: Duration,
        mut send_interrupt_signal: InterruptSignalSender,
        mut send_terminate_signal: TerminateSignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        InterruptSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
        TerminateSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.defuse_drop_panic();

        let mut diagnostics = TerminationDiagnostics::default();

        if let Some(exit_status) = self.try_reap_before_termination(&mut diagnostics) {
            return Ok(exit_status);
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
            return Ok(exit_status);
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
            return Ok(exit_status);
        }

        self.attempt_forceful_kill(diagnostics).await
    }

    fn try_reap_before_termination(
        &mut self,
        diagnostics: &mut TerminationDiagnostics,
    ) -> Option<ExitStatus> {
        match self.try_reap_exit_status() {
            Ok(Some(exit_status)) => Some(exit_status),
            Ok(None) => None,
            Err(err) => {
                diagnostics.record_graceful(
                    GracefulTerminationPhase::Interrupt,
                    format!(
                        "Preflight process status check failed before sending {}: {err}",
                        signal::INTERRUPT_SIGNAL_NAME
                    ),
                );
                tracing::warn!(
                    process = %self.name,
                    signal = signal::INTERRUPT_SIGNAL_NAME,
                    error = %err,
                    "Could not determine process state before termination. Attempting interrupt signal."
                );
                None
            }
        }
    }

    async fn attempt_graceful_phase<SignalSender>(
        &mut self,
        signal_name: &'static str,
        next_signal_name: &'static str,
        timeout: Duration,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
        send_signal: &mut SignalSender,
    ) -> Option<ExitStatus>
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
                diagnostics.record_graceful(phase, format!("failed to send {signal_name}: {err}"));
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %err,
                    "Graceful shutdown signal could not be sent. Attempting next shutdown phase."
                );
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
    ) -> Option<ExitStatus> {
        match self.wait_for_completion_inner(Some(timeout)).await {
            Ok(exit_status) => Some(exit_status),
            Err(not_terminated) => {
                diagnostics.record_graceful(
                    phase,
                    format!("process did not terminate after {signal_name}: {not_terminated}"),
                );
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %not_terminated,
                    "Graceful shutdown signal timed out. Attempting next shutdown phase."
                );
                None
            }
        }
    }

    fn try_reap_after_failed_signal(
        &mut self,
        signal_name: &'static str,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
    ) -> Option<ExitStatus> {
        match self.try_reap_exit_status() {
            Ok(Some(exit_status)) => Some(exit_status),
            Ok(None) => None,
            Err(reap_error) => {
                diagnostics.record_graceful(
                    phase,
                    format!("status check after failed {signal_name} send failed: {reap_error}"),
                );
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    error = %reap_error,
                    "Could not determine process state after graceful signal send failed."
                );
                None
            }
        }
    }

    async fn attempt_forceful_kill(
        &mut self,
        mut diagnostics: TerminationDiagnostics,
    ) -> Result<ExitStatus, TerminationError> {
        match self.kill().await {
            Ok(()) => {
                // Note: A forceful kill should typically (somewhat) immediately lead to
                // termination of the process. But there are cases in which even a forceful kill
                // does not / cannot / will not kill a process. We do not want to wait indefinitely
                // in case this happens and therefore wait (at max) for a fixed duration after any
                // kill.
                match self
                    .wait_for_completion_inner(Some(SIGKILL_WAIT_TIMEOUT))
                    .await
                {
                    Ok(exit_status) => Ok(exit_status),
                    Err(not_terminated_after_kill) => {
                        diagnostics.record_kill(format!(
                            "process did not terminate after {}: {not_terminated_after_kill}",
                            signal::KILL_SIGNAL_NAME
                        ));
                        // Unlikely. See the note above.
                        tracing::error!(
                            process = %self.name,
                            interrupt_signal = signal::INTERRUPT_SIGNAL_NAME,
                            terminate_signal = signal::TERMINATE_SIGNAL_NAME,
                            kill_signal = signal::KILL_SIGNAL_NAME,
                            "Process did not terminate after all termination attempts. Process may still be running. Manual intervention and investigation required!"
                        );
                        Err(diagnostics
                            .into_termination_failed(self.name.clone(), io::ErrorKind::TimedOut))
                    }
                }
            }
            Err(kill_error) => {
                let kill_error_kind = kill_error.kind();
                diagnostics.record_kill(format!(
                    "failed to send {}: {kill_error}",
                    signal::KILL_SIGNAL_NAME
                ));

                match self.try_reap_exit_status() {
                    Ok(Some(exit_status)) => return Ok(exit_status),
                    Ok(None) => {}
                    Err(reap_error) => {
                        diagnostics.record_kill(format!(
                            "status check after failed {} send failed: {reap_error}",
                            signal::KILL_SIGNAL_NAME
                        ));
                        tracing::warn!(
                            process = %self.name,
                            signal = signal::KILL_SIGNAL_NAME,
                            error = %reap_error,
                            "Could not determine process state after forceful shutdown failed."
                        );
                    }
                }

                tracing::error!(
                    process = %self.name,
                    error = %kill_error,
                    signal = signal::KILL_SIGNAL_NAME,
                    "Forceful shutdown failed. Process may still be running. Manual intervention required!"
                );

                Err(diagnostics.into_termination_failed(self.name.clone(), kill_error_kind))
            }
        }
    }

    /// Forces the process to exit. Most users should call [`ProcessHandle::terminate`] instead.
    ///
    /// This is equivalent to sending `SIGKILL` on Unix or calling `TerminateProcess` on Windows,
    /// followed by wait.
    /// A successful call waits for the child to exit and disarms the drop cleanup and panic guards,
    /// so the handle can be dropped safely afterward.
    ///
    /// # Errors
    ///
    /// Returns an error if Tokio cannot kill or wait for the child process.
    pub async fn kill(&mut self) -> io::Result<()> {
        self.child.kill().await?;
        self.must_not_be_terminated();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panic_on_drop::PanicOnDrop;
    use crate::{
        DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt, SealedReplayBehavior,
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
    fn wait_options(timeout: Option<Duration>) -> crate::WaitForCompletionOptions {
        crate::WaitForCompletionOptions::builder()
            .timeout(timeout)
            .build()
    }

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

    #[cfg(not(windows))]
    fn long_running_command() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");
        cmd
    }

    #[cfg(windows)]
    fn long_running_command() -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("ping");
        cmd.args(["-n", "6", "127.0.0.1"]);
        cmd
    }

    fn spawn_long_running_process()
    -> crate::BroadcastProcessHandle<crate::BestEffortDelivery, crate::NoReplay> {
        crate::Process::new(long_running_command())
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

    #[tokio::test]
    async fn terminate_falls_back_to_kill_when_graceful_signal_sends_fail() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut process = spawn_long_running_process();
        let terminate_attempted = Arc::new(AtomicBool::new(false));
        let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

        let exit_status = process
            .terminate_inner(
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
        assert_that!(exit_status.success()).is_false();
        assert_that!(process.cleanup_on_drop).is_false();
    }

    #[test]
    fn termination_failed_summarizes_all_recorded_phase_errors() {
        let mut diagnostics = TerminationDiagnostics::default();
        diagnostics.record_graceful(
            GracefulTerminationPhase::Interrupt,
            "failed to send SIGINT: injected interrupt failure",
        );
        diagnostics.record_graceful(
            GracefulTerminationPhase::Interrupt,
            "status check after failed SIGINT send failed: injected reap failure",
        );
        diagnostics.record_graceful(
            GracefulTerminationPhase::Terminate,
            "failed to send SIGTERM: injected terminate failure",
        );
        diagnostics.record_kill("failed to send SIGKILL: injected kill failure");
        diagnostics
            .record_kill("status check after failed SIGKILL send failed: final reap failure");

        let error = diagnostics
            .into_termination_failed(Cow::Borrowed("diagnostic-test"), io::ErrorKind::Other);

        let TerminationError::TerminationFailed {
            sigint_error,
            sigterm_error,
            sigkill_error,
            ..
        } = error
        else {
            panic!("expected TerminationFailed");
        };

        assert_that!(sigint_error.as_str()).contains("injected interrupt failure");
        assert_that!(sigint_error.as_str()).contains("injected reap failure");
        assert_that!(sigterm_error.as_str()).contains("injected terminate failure");
        assert_that!(sigkill_error.to_string().as_str()).contains("injected kill failure");
        assert_that!(sigkill_error.to_string().as_str()).contains("final reap failure");
    }

    #[tokio::test]
    async fn test_termination() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let started_at = jiff::Zoned::now();
        let mut handle = crate::Process::new(cmd)
            .name("sleep")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
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
    }

    #[tokio::test]
    async fn terminate_returns_normal_exit_when_process_already_exited() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("exit 0");

        let mut handle = crate::Process::new(cmd)
            .name("sh")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let exit_status = handle
            .terminate(Duration::from_millis(50), Duration::from_millis(50))
            .await
            .unwrap();

        assert_that!(exit_status.success()).is_true();
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
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
            .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

        let ready = process
            .stdout()
            .wait_for_line_from_start_with_timeout(
                |line| line == "ready",
                crate::LineParsingOptions::default(),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_that!(ready).is_equal_to(crate::WaitForLineResult::Matched);

        process.send_interrupt_signal().unwrap();

        let exit_status = process
            .wait_for_completion(wait_options(Some(Duration::from_secs(5))))
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
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap()
            .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

        let ready = process
            .stdout()
            .wait_for_line_from_start_with_timeout(
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
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("5");

        let mut process = crate::Process::new(cmd)
            .name("sleep")
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

        assert_that!(process.cleanup_on_drop).is_true();
        assert_that!(
            process
                .panic_on_drop
                .as_ref()
                .is_some_and(PanicOnDrop::is_armed)
        )
        .is_true();

        process.kill().await.unwrap();

        assert_that!(process.cleanup_on_drop).is_false();
        assert_that!(&process.panic_on_drop).is_none();

        drop(process);
    }
}
