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

/// Grace window granted to Tokio's SIGCHLD reaper after a signal-send failure so a freshly-exited
/// child is observed as exited rather than as still running. Covers the brief race where the OS
/// rejects signals to a not-yet-reaped process group (`EPERM` on macOS, `ESRCH` on Linux).
const REAP_AFTER_SIGNAL_FAILURE_GRACE: Duration = Duration::from_millis(100);

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
    fn record_preflight_status_error(&mut self, error: impl Error + Send + Sync + 'static) {
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
        error: impl Error + Send + Sync + 'static,
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
        error: impl Error + Send + Sync + 'static,
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
        error: impl Error + Send + Sync + 'static,
    ) {
        self.record(
            phase.attempt_phase(),
            TerminationAttemptOperation::CheckStatus,
            Some(signal_name),
            error,
        );
    }

    fn record_kill_signal_error(&mut self, error: impl Error + Send + Sync + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::SendSignal,
            Some(signal::KILL_SIGNAL_NAME),
            error,
        );
    }

    fn record_kill_wait_error(&mut self, error: impl Error + Send + Sync + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::WaitForExit,
            Some(signal::KILL_SIGNAL_NAME),
            error,
        );
    }

    fn record_kill_status_error(&mut self, error: impl Error + Send + Sync + 'static) {
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
        error: impl Error + Send + Sync + 'static,
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

    #[must_use]
    fn into_signal_failed(self, process_name: Cow<'static, str>) -> TerminationError {
        assert!(
            !self.attempt_errors.is_empty(),
            "into_signal_failed must not be used when no error was recorded!",
        );

        TerminationError::SignalFailed {
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
    /// Returns [`TerminationError`] if the process status could not be checked or if the platform
    /// signal could not be sent.
    pub fn send_interrupt_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_preflight_reap(
            GracefulTerminationPhase::Interrupt,
            signal::INTERRUPT_SIGNAL_NAME,
            signal::send_interrupt,
        )
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
    /// Returns [`TerminationError`] if the process status could not be checked or if the platform
    /// signal could not be sent.
    pub fn send_terminate_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_preflight_reap(
            GracefulTerminationPhase::Terminate,
            signal::TERMINATE_SIGNAL_NAME,
            signal::send_terminate,
        )
    }

    /// Terminates this process by sending platform graceful shutdown signals first, then killing
    /// the process if it does not complete after receiving them.
    ///
    /// On Unix this means `SIGINT`, then `SIGTERM`, then `SIGKILL`. On Windows, both graceful
    /// phases send `CTRL_BREAK_EVENT` before falling back to `TerminateProcess`. The force-kill
    /// fallback adds one fixed 3-second wait on top of the graceful timeouts.
    ///
    /// # Timeouts
    ///
    /// `interrupt_timeout` and `terminate_timeout` bound the post-signal wait of their phase:
    ///
    /// - Signal send succeeds: wait up to the user-supplied timeout and escalate if the process is
    ///   not terminated.
    /// - Signal send fails: replace the user timeout with a fixed 100 ms grace so Tokio's SIGCHLD
    ///   reaper can catch up to a child that just exited (the OS rejects signals to a not-yet-
    ///   reaped process group with `EPERM` on macOS or `ESRCH` on Linux). Real permission denials
    ///   still surface as an error after the grace elapses.
    ///
    /// `Duration::from_secs(0)` disables the post-signal wait entirely and effectively forces the
    /// call into `SIGKILL`. Prefer small but non-zero values (e.g. 100 ms to a few seconds).
    ///
    /// # Drop guards on `Ok` vs `Err`
    ///
    /// On `Ok`, the drop cleanup and panic guards are disarmed and the handle can be dropped
    /// safely. On `Err` (or if the future is canceled), the guards stay armed: the library cannot
    /// verify cleanup from the outside, so dropping would leak a process. Recover by retrying
    /// `terminate`, escalating to [`kill`](Self::kill), calling
    /// [`must_not_be_terminated`](Self::must_not_be_terminated) to accept the failure, or
    /// propagating the error and letting the panic-on-drop surface the leak.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if signaling or waiting for process termination fails.
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
        phase: GracefulTerminationPhase,
        signal_name: &'static str,
        send_signal: SignalSender,
    ) -> Result<(), TerminationError>
    where
        SignalSender: FnOnce(&tokio::process::Child) -> Result<(), io::Error>,
    {
        self.send_signal_with_reaper(phase, signal_name, send_signal, Self::try_reap_exit_status)
    }

    fn send_signal_with_reaper<SignalSender, Reaper>(
        &mut self,
        phase: GracefulTerminationPhase,
        signal_name: &'static str,
        send_signal: SignalSender,
        mut try_reap_exit_status: Reaper,
    ) -> Result<(), TerminationError>
    where
        SignalSender: FnOnce(&tokio::process::Child) -> Result<(), io::Error>,
        Reaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
    {
        let mut diagnostics = TerminationDiagnostics::default();

        // We don't have to send the singal at all when the process already terminated.
        match try_reap_exit_status(self) {
            Ok(Some(_exit_status)) => {
                self.must_not_be_terminated();
                Ok(())
            }
            Ok(None) => match send_signal(&self.child) {
                Ok(()) => Ok(()),
                // Sync probe only. The SIGCHLD-grace bounded wait lives on the `terminate()` path.
                // Keeping this sync avoids making the public `send_*_signal` APIs async.
                Err(signal_error) => match try_reap_exit_status(self) {
                    Ok(Some(_exit_status)) => {
                        self.must_not_be_terminated();
                        Ok(())
                    }
                    Ok(None) => {
                        diagnostics.record_graceful_signal_error(phase, signal_name, signal_error);
                        Err(diagnostics.into_signal_failed(self.name.clone()))
                    }
                    Err(reap_error) => {
                        diagnostics.record_graceful_signal_error(phase, signal_name, signal_error);
                        diagnostics.record_graceful_status_error(phase, signal_name, reap_error);
                        Err(diagnostics.into_signal_failed(self.name.clone()))
                    }
                },
            },
            Err(status_error) => {
                diagnostics.record_graceful_status_error(phase, signal_name, status_error);
                Err(diagnostics.into_signal_failed(self.name.clone()))
            }
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
                self.observe_exit_after_failed_signal(signal_name, phase, diagnostics)
                    .await
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
        match self.wait_for_exit_after_signal(timeout).await {
            Ok(Some(exit_status)) => Some(TerminationOutcome::graceful_success(exit_status)),
            Ok(None) => {
                let not_terminated = Self::wait_timeout_diagnostic(timeout);
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
            Err(wait_error) => {
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %wait_error,
                    "Graceful shutdown signal timed out. Attempting next shutdown phase."
                );
                diagnostics.record_graceful_wait_error(phase, signal_name, wait_error);
                None
            }
        }
    }

    /// Recovery probe after a graceful signal send failed: waits briefly so a freshly-exited
    /// child is observed as exited rather than as still running. See
    /// [`REAP_AFTER_SIGNAL_FAILURE_GRACE`].
    async fn observe_exit_after_failed_signal(
        &mut self,
        signal_name: &'static str,
        phase: GracefulTerminationPhase,
        diagnostics: &mut TerminationDiagnostics,
    ) -> Option<TerminationOutcome> {
        match self
            .wait_for_exit_after_signal(REAP_AFTER_SIGNAL_FAILURE_GRACE)
            .await
        {
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
        match Self::start_kill_process_group(&mut self.child) {
            Ok(()) => {
                // Note: A forceful kill should typically (somewhat) immediately lead to
                // termination of the process. But there are cases in which even a forceful kill
                // does not / cannot / will not kill a process. We do not want to wait indefinitely
                // in case this happens and therefore wait (at max) for a fixed duration after any
                // kill.
                match self
                    .wait_for_exit_after_signal(FORCE_KILL_WAIT_TIMEOUT)
                    .await
                {
                    Ok(Some(exit_status)) => {
                        Ok(TerminationOutcome::force_kill_success(exit_status))
                    }
                    Ok(None) => {
                        let not_terminated_after_kill =
                            Self::wait_timeout_diagnostic(FORCE_KILL_WAIT_TIMEOUT);
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

                // Brief grace for Tokio's SIGCHLD reaper to catch up - see
                // `REAP_AFTER_SIGNAL_FAILURE_GRACE`.
                match self
                    .wait_for_exit_after_signal(REAP_AFTER_SIGNAL_FAILURE_GRACE)
                    .await
                {
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
    /// `kill` is a reasonable next step when [`terminate`](Self::terminate) returns `Err` and the
    /// caller is not interested in further graceful escalation.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if Tokio cannot kill or wait for the child process.
    pub async fn kill(&mut self) -> Result<(), TerminationError> {
        self.kill_inner(Self::start_kill_raw).await
    }

    async fn kill_inner<StartKill>(
        &mut self,
        mut start_kill: StartKill,
    ) -> Result<(), TerminationError>
    where
        StartKill: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.stdin().close();
        let mut diagnostics = TerminationDiagnostics::default();

        if let Err(err) = start_kill(self) {
            diagnostics.record_kill_signal_error(err);
            return Err(diagnostics.into_termination_failed(self.name.clone()));
        }

        if let Err(err) = self.wait_for_completion_unbounded_inner().await {
            diagnostics.record_kill_wait_error(err);
            return Err(diagnostics.into_termination_failed(self.name.clone()));
        }

        Ok(())
    }

    fn start_kill_raw(&mut self) -> Result<(), io::Error> {
        Self::start_kill_process_group(&mut self.child)
    }

    /// Sends `SIGKILL` to the child's process group on Unix and forwards to Tokio's
    /// `Child::start_kill` on every other platform.
    ///
    /// On Unix the child is the leader of a process group set up at spawn time, so targeting the
    /// group reaches any grandchildren the child has fork-execed. Tokio's stock `start_kill`
    /// targets only the child's PID and would orphan that subtree. On Windows the standard
    /// `TerminateProcess` semantics still apply; the pre-kill `CTRL_BREAK_EVENT` step in
    /// [`Self::terminate`] is what reaches the rest of the console process group there.
    fn start_kill_process_group(child: &mut tokio::process::Child) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match child.id() {
                Some(pid) => signal::send_kill_to_process_group(pid),
                // Already reaped. Tokio's start_kill would have surfaced this as an error;
                // matching its behavior keeps the caller paths identical.
                None => child.start_kill(),
            }
        }
        #[cfg(not(unix))]
        {
            child.start_kill()
        }
    }
}

#[cfg(test)]
mod tests;
