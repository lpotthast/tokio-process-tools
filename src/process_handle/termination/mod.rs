use super::ProcessHandle;
use crate::error::{
    TerminationAttemptError, TerminationAttemptOperation, TerminationAttemptPhase, TerminationError,
};
use crate::output_stream::OutputStream;
use std::borrow::Cow;
use std::error::Error;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

/// Maximum time to wait for process termination after forceful kill.
///
/// This is a safety timeout since forceful kill should terminate processes immediately,
/// but there are rare cases where even forceful kill may not work.
#[cfg(any(unix, windows))]
const FORCE_KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

/// Grace window granted to Tokio's SIGCHLD reaper after a signal-send failure so a freshly-exited
/// child is observed as exited rather than as still running. Covers the brief race where the OS
/// rejects signals to a not-yet-reaped process group (`EPERM` on macOS, `ESRCH` on Linux).
#[cfg(any(unix, windows))]
const REAP_AFTER_SIGNAL_FAILURE_GRACE: Duration = Duration::from_millis(100);

#[cfg(unix)]
const INTERRUPT_LABEL: &str = "SIGINT";
#[cfg(unix)]
const TERMINATE_LABEL: &str = "SIGTERM";

#[cfg(windows)]
const GRACEFUL_LABEL: &str = "CTRL_BREAK_EVENT";

/// Label recorded in diagnostics for the forceful kill phase. Cross-platform because `kill()` is
/// available on every platform Tokio supports (the underlying `Child::start_kill()` is what runs
/// on targets where graceful escalation is unavailable).
#[cfg(unix)]
const KILL_LABEL: &str = "SIGKILL";
#[cfg(windows)]
const KILL_LABEL: &str = "TerminateProcess";
#[cfg(not(any(unix, windows)))]
const KILL_LABEL: &str = "kill";

/// Per-platform graceful-shutdown timeout budget passed to [`ProcessHandle::terminate`] and
/// related APIs.
///
/// The shape mirrors the platform's actual graceful-shutdown model. Cross-platform code
/// constructs the value under cfg gates and then passes it to the cross-platform `terminate(...)`
/// signature:
///
/// ```rust,ignore
/// use std::time::Duration;
/// use tokio_process_tools::GracefulTimeouts;
///
/// #[cfg(unix)]
/// let timeouts = GracefulTimeouts {
///     interrupt_timeout: Duration::from_secs(3),
///     terminate_timeout: Duration::from_secs(5),
/// };
/// #[cfg(windows)]
/// let timeouts = GracefulTimeouts {
///     graceful_timeout: Duration::from_secs(8),
/// };
///
/// process.terminate(timeouts).await?;
/// ```
///
/// On Unix the type carries two separate budgets, one per graceful phase
/// (`SIGINT` then `SIGTERM`). On Windows it carries a single budget because
/// `GenerateConsoleCtrlEvent` can only target a child's process group with `CTRL_BREAK_EVENT`;
/// sending the same event a second time cannot do more than the first send already did, so the
/// Windows `terminate(...)` runs exactly one graceful step.
///
/// This type is only available on Unix and Windows because the underlying graceful-shutdown
/// signals only exist there. On other Tokio-supported targets the spawn, wait,
/// output-collection, and [`ProcessHandle::kill`] APIs remain available; only the
/// graceful-termination surface (`terminate(...)`, `terminate_on_drop(...)`,
/// `wait_for_completion_or_terminate(...)`, the `send_*_signal(...)` methods, and this type) is
/// gated out.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GracefulTimeouts {
    /// Maximum time to wait after sending `SIGINT` before escalating to `SIGTERM`.
    #[cfg(unix)]
    pub interrupt_timeout: Duration,
    /// Maximum time to wait after sending `SIGTERM` before escalating to `SIGKILL`.
    #[cfg(unix)]
    pub terminate_timeout: Duration,
    /// Maximum time to wait after sending `CTRL_BREAK_EVENT` before escalating to
    /// `TerminateProcess`.
    #[cfg(windows)]
    pub graceful_timeout: Duration,
}

#[cfg(any(unix, windows))]
impl GracefulTimeouts {
    /// Combined graceful-shutdown budget, used for downstream output-collection deadlines.
    pub(crate) fn total(self) -> Duration {
        #[cfg(unix)]
        {
            self.interrupt_timeout
                .saturating_add(self.terminate_timeout)
        }
        #[cfg(windows)]
        {
            self.graceful_timeout
        }
    }
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminationOutcome {
    pub(crate) exit_status: ExitStatus,
    pub(crate) output_collection_timeout_extension: Duration,
}

#[cfg(any(unix, windows))]
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

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy)]
enum GracefulTerminationPhase {
    #[cfg(unix)]
    Interrupt,
    Terminate,
}

#[cfg(any(unix, windows))]
impl GracefulTerminationPhase {
    fn attempt_phase(self) -> TerminationAttemptPhase {
        match self {
            #[cfg(unix)]
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
    #[cfg(any(unix, windows))]
    fn record_preflight_status_error(&mut self, error: impl Error + Send + Sync + 'static) {
        self.record(
            TerminationAttemptPhase::Preflight,
            TerminationAttemptOperation::CheckStatus,
            None,
            error,
        );
    }

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
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

    #[cfg(any(unix, windows))]
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
            Some(KILL_LABEL),
            error,
        );
    }

    fn record_kill_wait_error(&mut self, error: impl Error + Send + Sync + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::WaitForExit,
            Some(KILL_LABEL),
            error,
        );
    }

    #[cfg(any(unix, windows))]
    fn record_kill_status_error(&mut self, error: impl Error + Send + Sync + 'static) {
        self.record(
            TerminationAttemptPhase::Kill,
            TerminationAttemptOperation::CheckStatus,
            Some(KILL_LABEL),
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

    #[cfg(any(unix, windows))]
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

// Cross-platform termination methods. `kill()` and the Drop best-effort cleanup work on every
// Tokio-supported platform via `tokio::process::Child::start_kill()`, so they stay available
// even on targets where graceful-termination escalation is not.
impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Forces the process to exit. Most users should call [`ProcessHandle::terminate`] instead.
    ///
    /// This is equivalent to sending `SIGKILL` on Unix or calling `TerminateProcess` on Windows,
    /// followed by wait. On other Tokio-supported platforms it forwards to
    /// [`tokio::process::Child::start_kill`].
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

    pub(super) async fn kill_inner<StartKill>(
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

    pub(super) fn start_kill_raw(&mut self) -> Result<(), io::Error> {
        Self::start_kill_process_group(&mut self.child)
    }

    /// Sends `SIGKILL` to the child's process group on Unix and forwards to Tokio's
    /// `Child::start_kill` everywhere else.
    ///
    /// On Unix the child is the leader of a process group set up at spawn time, so targeting the
    /// group reaches any grandchildren the child has fork-execed. Tokio's stock `start_kill`
    /// targets only the child's PID and would orphan that subtree. On Windows the standard
    /// `TerminateProcess` semantics still apply; the pre-kill `CTRL_BREAK_EVENT` step in
    /// [`Self::terminate`] is what reaches the rest of the console process group there. On other
    /// Tokio-supported platforms there is no library-managed process-group setup, so `start_kill`
    /// targets the child directly.
    pub(super) fn start_kill_process_group(
        child: &mut tokio::process::Child,
    ) -> Result<(), io::Error> {
        #[cfg(unix)]
        {
            match child.id() {
                Some(pid) => crate::signal::send_kill_to_process_group(pid),
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

// Graceful-termination methods. Only available on Unix and Windows because they rely on platform
// signal primitives that have no cross-platform analogue.
#[cfg(any(unix, windows))]
impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Manually send `SIGINT` to this process's process group via `killpg`.
    ///
    /// `SIGINT` is the dedicated user-interrupt signal, distinct from the `SIGTERM` delivered by
    /// [`Self::send_terminate_signal`]. The signal targets the child's process group, so any
    /// grandchildren the child has fork-execed are signaled together with the leader.
    ///
    /// If the process has already exited, this reaps it and returns `Ok(())` instead of
    /// attempting to signal a stale PID or process group. If the signal send fails because the
    /// child exited after the preflight check, this also reaps it and returns `Ok(())`.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// This method is Unix-only because Windows has no targetable `SIGINT` analogue:
    /// `GenerateConsoleCtrlEvent` only accepts `CTRL_BREAK_EVENT` for nonzero process groups.
    /// On Windows, use `send_ctrl_break_signal` instead.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if `SIGINT`
    /// could not be sent.
    #[cfg(unix)]
    pub fn send_interrupt_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_preflight_reap(
            GracefulTerminationPhase::Interrupt,
            INTERRUPT_LABEL,
            crate::signal::send_interrupt,
        )
    }

    /// Manually send `SIGTERM` to this process's process group via `killpg`.
    ///
    /// `SIGTERM` is the conventional "asked to terminate" signal sent by service supervisors and
    /// the operating system at shutdown. The signal targets the child's process group, so any
    /// grandchildren the child has fork-execed are signaled together with the leader.
    ///
    /// If the process has already exited, this reaps it and returns `Ok(())` instead of
    /// attempting to signal a stale PID or process group. If the signal send fails because the
    /// child exited after the preflight check, this also reaps it and returns `Ok(())`.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// This method is Unix-only because Windows has no targetable `SIGTERM` analogue:
    /// `GenerateConsoleCtrlEvent` only accepts `CTRL_BREAK_EVENT` for nonzero process groups.
    /// On Windows, use `send_ctrl_break_signal` instead.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if `SIGTERM`
    /// could not be sent.
    #[cfg(unix)]
    pub fn send_terminate_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_preflight_reap(
            GracefulTerminationPhase::Terminate,
            TERMINATE_LABEL,
            crate::signal::send_terminate,
        )
    }

    /// Manually deliver `CTRL_BREAK_EVENT` to this process's console process group via
    /// `GenerateConsoleCtrlEvent`.
    ///
    /// `CTRL_BREAK_EVENT` is the only console control event that can be targeted at a nonzero
    /// process group: `CTRL_C_EVENT` requires `dwProcessGroupId = 0` and would be broadcast to
    /// every process sharing the calling console (including the parent), so it is not usable to
    /// terminate a single child group. There is therefore no separate `SIGINT` vs. `SIGTERM`
    /// distinction on Windows; this single method covers the entire graceful-shutdown surface.
    ///
    /// If the process has already exited, this reaps it and returns `Ok(())` instead of
    /// attempting to signal a stale PID or process group. If the signal send fails because the
    /// child exited after the preflight check, this also reaps it and returns `Ok(())`.
    ///
    /// Prefer to call `terminate` instead, if you want to make sure this process is terminated.
    ///
    /// This method is Windows-only. On Unix, use `send_interrupt_signal` or
    /// `send_terminate_signal` instead.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if
    /// `CTRL_BREAK_EVENT` could not be delivered.
    #[cfg(windows)]
    pub fn send_ctrl_break_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_preflight_reap(
            GracefulTerminationPhase::Terminate,
            GRACEFUL_LABEL,
            crate::signal::send_ctrl_break,
        )
    }

    /// Terminates this process by sending platform graceful shutdown signals first, then killing
    /// the process if it does not complete after receiving them.
    ///
    /// The signature is the same on every supported platform; the shape of `timeouts` is
    /// platform-conditional. See [`GracefulTimeouts`] for how to construct one.
    ///
    /// - On Unix this is a 3-phase escalation: `SIGINT` -> wait `timeouts.interrupt_timeout` ->
    ///   `SIGTERM` -> wait `timeouts.terminate_timeout` -> `SIGKILL`. The two distinct graceful
    ///   signals matter in practice: idiomatic async Rust binaries use `tokio::signal::ctrl_c()`
    ///   (which on Unix listens only for `SIGINT`), and Python child processes turn `SIGINT`
    ///   into a `KeyboardInterrupt` exception that runs `try/finally` cleanup, while `SIGTERM`
    ///   falls through to the runtime's default handler.
    /// - On Windows this is a 2-phase termination: `CTRL_BREAK_EVENT` -> wait
    ///   `timeouts.graceful_timeout` -> `TerminateProcess`. **Only one `CTRL_BREAK_EVENT` is
    ///   ever sent.** `GenerateConsoleCtrlEvent` can only target a child's process group with
    ///   `CTRL_BREAK_EVENT` (sending `CTRL_C_EVENT` would require `dwProcessGroupId = 0` and
    ///   broadcast to the parent), so a second graceful send would be the same event and cannot
    ///   do more than the first send already did.
    ///
    /// The forceful kill fallback adds one fixed 3-second wait on top of the graceful timeouts.
    ///
    /// # Windows interop note
    ///
    /// `tokio::signal::ctrl_c()` on Windows registers only for `CTRL_C_EVENT`; it does not catch
    /// `CTRL_BREAK_EVENT`. A child Rust binary that listens only on the cross-platform
    /// `tokio::signal::ctrl_c()` will not respond to this graceful step on Windows and will be
    /// terminated forcefully after `graceful_timeout`. To interoperate, such a child should
    /// additionally listen on `tokio::signal::windows::ctrl_break()`, or expose another
    /// shutdown channel (stdin sentinel, IPC, or a command protocol).
    ///
    /// # Timeouts
    ///
    /// Each per-phase timeout in `timeouts` bounds the post-signal wait of its phase:
    ///
    /// - Signal send succeeds: wait up to the user-supplied timeout, then escalate.
    /// - Signal send fails: replace the user timeout with a fixed 100 ms grace so Tokio's
    ///   reaper can catch up to a child that just exited (the OS rejects signals to a not-yet-
    ///   reaped process group with `EPERM` on macOS or `ESRCH` on Linux). Real permission
    ///   denials still surface as an error after the grace elapses.
    ///
    /// `Duration::from_secs(0)` disables the post-signal wait entirely and effectively forces
    /// the call into the forceful kill (`SIGKILL` on Unix, `TerminateProcess` on Windows).
    /// Prefer small but non-zero values (e.g. 100 ms to a few seconds).
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
    /// Returns [`TerminationError`] if signalling or waiting for process termination fails.
    pub async fn terminate(
        &mut self,
        timeouts: GracefulTimeouts,
    ) -> Result<ExitStatus, TerminationError> {
        self.terminate_detailed(timeouts)
            .await
            .map(|outcome| outcome.exit_status)
    }

    pub(crate) async fn terminate_detailed(
        &mut self,
        timeouts: GracefulTimeouts,
    ) -> Result<TerminationOutcome, TerminationError> {
        #[cfg(unix)]
        {
            self.terminate_inner_with_preflight_reaper(
                timeouts,
                Self::try_reap_exit_status,
                Self::send_interrupt_signal_raw,
                Self::send_terminate_signal_raw,
            )
            .await
        }
        #[cfg(windows)]
        {
            self.terminate_inner_with_preflight_reaper(
                timeouts,
                Self::try_reap_exit_status,
                Self::send_ctrl_break_signal_raw,
            )
            .await
        }
    }

    #[cfg(all(test, unix))]
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

    #[cfg(all(test, windows))]
    async fn terminate_inner<GracefulSignalSender>(
        &mut self,
        graceful_timeout: Duration,
        send_graceful_signal: GracefulSignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        GracefulSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.terminate_inner_detailed(graceful_timeout, send_graceful_signal)
            .await
            .map(|outcome| outcome.exit_status)
    }

    #[cfg(all(test, unix))]
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
            GracefulTimeouts {
                interrupt_timeout,
                terminate_timeout,
            },
            Self::try_reap_exit_status,
            send_interrupt_signal,
            send_terminate_signal,
        )
        .await
    }

    #[cfg(all(test, windows))]
    async fn terminate_inner_detailed<GracefulSignalSender>(
        &mut self,
        graceful_timeout: Duration,
        send_graceful_signal: GracefulSignalSender,
    ) -> Result<TerminationOutcome, TerminationError>
    where
        GracefulSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        self.terminate_inner_with_preflight_reaper(
            GracefulTimeouts { graceful_timeout },
            Self::try_reap_exit_status,
            send_graceful_signal,
        )
        .await
    }

    #[cfg(unix)]
    async fn terminate_inner_with_preflight_reaper<
        PreflightReaper,
        InterruptSignalSender,
        TerminateSignalSender,
    >(
        &mut self,
        timeouts: GracefulTimeouts,
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
                        signal = INTERRUPT_LABEL,
                        error = %err,
                        "Could not determine process state before termination. Attempting interrupt signal."
                    );
                    diagnostics.record_preflight_status_error(err);
                }
            }
            if let Some(exit_status) = self
                .attempt_graceful_phase(
                    INTERRUPT_LABEL,
                    TERMINATE_LABEL,
                    timeouts.interrupt_timeout,
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
                    TERMINATE_LABEL,
                    KILL_LABEL,
                    timeouts.terminate_timeout,
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

    #[cfg(windows)]
    async fn terminate_inner_with_preflight_reaper<PreflightReaper, GracefulSignalSender>(
        &mut self,
        timeouts: GracefulTimeouts,
        mut try_reap_exit_status: PreflightReaper,
        mut send_graceful_signal: GracefulSignalSender,
    ) -> Result<TerminationOutcome, TerminationError>
    where
        PreflightReaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
        GracefulSignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
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
                        signal = GRACEFUL_LABEL,
                        error = %err,
                        "Could not determine process state before termination. Attempting graceful signal."
                    );
                    diagnostics.record_preflight_status_error(err);
                }
            }

            if let Some(exit_status) = self
                .attempt_graceful_phase(
                    GRACEFUL_LABEL,
                    KILL_LABEL,
                    timeouts.graceful_timeout,
                    GracefulTerminationPhase::Terminate,
                    &mut diagnostics,
                    &mut send_graceful_signal,
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

        match try_reap_exit_status(self) {
            Ok(Some(_)) => {
                self.must_not_be_terminated();
                Ok(())
            }
            Ok(None) => match send_signal(&self.child) {
                Ok(()) => Ok(()),
                // Sync probe only - the SIGCHLD-grace bounded wait lives on the `terminate()`
                // path. Keeping this sync avoids making the public `send_*_signal` APIs async.
                Err(signal_error) => match try_reap_exit_status(self) {
                    Ok(Some(_)) => {
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

    #[cfg(unix)]
    fn send_interrupt_signal_raw(&mut self) -> Result<(), io::Error> {
        crate::signal::send_interrupt(&self.child)
    }

    #[cfg(unix)]
    fn send_terminate_signal_raw(&mut self) -> Result<(), io::Error> {
        crate::signal::send_terminate(&self.child)
    }

    #[cfg(windows)]
    fn send_ctrl_break_signal_raw(&mut self) -> Result<(), io::Error> {
        crate::signal::send_ctrl_break(&self.child)
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
                            kill_signal = KILL_LABEL,
                            "Process did not terminate after all termination attempts. Process may still be running. Manual intervention and investigation required!"
                        );
                        diagnostics.record_kill_wait_error(not_terminated_after_kill);
                        Err(diagnostics.into_termination_failed(self.name.clone()))
                    }
                    Err(not_terminated_after_kill) => {
                        // Unlikely. See the note above.
                        tracing::error!(
                            process = %self.name,
                            kill_signal = KILL_LABEL,
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
                    signal = KILL_LABEL,
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
                            signal = KILL_LABEL,
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
}

#[cfg(test)]
mod tests;
