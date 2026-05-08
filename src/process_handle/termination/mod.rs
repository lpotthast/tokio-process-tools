use super::ProcessHandle;
use crate::error::{TerminationAction, TerminationError};
use crate::output_stream::OutputStream;
use std::io;
use std::process::ExitStatus;
use std::time::Duration;

mod diagnostics;
#[cfg(any(unix, windows))]
mod shutdown;

pub(in crate::process_handle) use diagnostics::TerminationDiagnostics;
#[cfg(any(unix, windows))]
pub use shutdown::{
    GracefulShutdown, GracefulShutdownBuilder, UnixGracefulPhase, UnixGracefulShutdown,
    UnixGracefulSignal, WindowsGracefulShutdown,
};

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

/// Label recorded in diagnostics for the forceful kill phase. Cross-platform because `kill()` is
/// available on every platform Tokio supports (the underlying `Child::start_kill()` is what runs
/// on targets where graceful escalation is unavailable).
#[cfg(unix)]
const KILL_LABEL: &str = "SIGKILL";
#[cfg(windows)]
const KILL_LABEL: &str = "TerminateProcess";
#[cfg(not(any(unix, windows)))]
const KILL_LABEL: &str = "kill";

/// One step of the cross-platform graceful-termination loop. Pre-computed by the per-platform
/// wrapper so the shared loop body has no platform branches.
#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy)]
struct TerminationStep {
    signal_label: &'static str,
    timeout: Duration,
}

// Functionality available on all Tokio-supported platforms.
impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Forces the process to exit. Most users should call [`ProcessHandle::terminate`] instead.
    ///
    /// This is equivalent to sending `SIGKILL` on Unix or calling `TerminateProcess` on Windows,
    /// followed by a wait for the process to be reaped. On other Tokio-supported platforms it
    /// forwards to [`tokio::process::Child::start_kill`].
    ///
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
        self.stdin().close();
        let mut diagnostics = TerminationDiagnostics::default();

        if let Err(err) = self.send_kill_signal() {
            diagnostics.record(
                TerminationAction::SendSignal {
                    signal_name: KILL_LABEL,
                },
                err,
            );
            return Err(diagnostics.into_termination_failed(self.name.clone()));
        }

        if let Err(err) = self.wait_for_completion_unbounded().await {
            diagnostics.record(TerminationAction::WaitForExit, err);
            return Err(diagnostics.into_termination_failed(self.name.clone()));
        }

        Ok(())
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
    /// Terminates this process by dispatching the configured graceful-shutdown sequence first,
    /// then forcefully killing the process if it has not exited after the sequence completes.
    ///
    /// The signature is the same on every supported platform; the shape of `shutdown` is
    /// platform-conditional. See [`GracefulShutdown`] for how to construct one and
    /// [`UnixGracefulShutdown`] for guidance on choosing the Unix sequence.
    ///
    /// - On Unix the configured [`UnixGracefulShutdown`] dispatches one or more graceful signals
    ///   in order; each phase's `timeout` bounds how long to wait for the child to exit before
    ///   escalating to the next phase. After the last configured phase, `SIGKILL` runs as the
    ///   implicit forceful fallback.
    /// - On Windows this is a 2-phase termination: `CTRL_BREAK_EVENT` -> wait
    ///   `shutdown.windows.timeout` -> `TerminateProcess`. **Only one `CTRL_BREAK_EVENT`
    ///   is ever sent.** `GenerateConsoleCtrlEvent` can only target a child's process group with
    ///   `CTRL_BREAK_EVENT` (sending `CTRL_C_EVENT` would require `dwProcessGroupId = 0` and
    ///   broadcast to the parent), so a second graceful send would be the same event and cannot
    ///   do more than the first send already did.
    ///
    /// The forceful kill fallback adds one fixed 3-second wait on top of the graceful timeouts.
    ///
    /// # Timeouts are upper bounds, not delays
    ///
    /// Each per-phase timeout bounds the post-signal wait of its phase. The wait future resolves
    /// the instant Tokio's `SIGCHLD` reaper observes the child exit, so handler-less children
    /// (children that have no handler installed for the signal we send) typically die in
    /// microseconds via the kernel's default disposition (`Term`) and the configured timeout
    /// never fires for them. The timeout only matters when the child has installed a handler
    /// that takes time to complete.
    ///
    /// # What signal should I send?
    ///
    /// See [`UnixGracefulShutdown`] for the recommended single-signal sequences and a discussion
    /// of why mixing `SIGINT` and `SIGTERM` does not cover children with unknown signal handlers.
    ///
    /// # Windows interop note
    ///
    /// `tokio::signal::ctrl_c()` on Windows registers only for `CTRL_C_EVENT`; it does not catch
    /// `CTRL_BREAK_EVENT`. A child Rust binary that listens only on the cross-platform
    /// `tokio::signal::ctrl_c()` will not respond to this graceful step on Windows and will be
    /// terminated forcefully after `timeout`. To interoperate, such a child should
    /// additionally listen on `tokio::signal::windows::ctrl_break()`, or expose another
    /// shutdown channel (stdin sentinel, IPC, or a command protocol).
    ///
    /// # Per-phase timeout semantics
    ///
    /// Each per-phase timeout in `shutdown` bounds the post-signal wait of its phase:
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
        shutdown: GracefulShutdown,
    ) -> Result<ExitStatus, TerminationError> {
        #[cfg(unix)]
        {
            self.terminate_with_hooks(
                &shutdown.unix,
                Self::try_reap_exit_status,
                |this, signal| match signal {
                    UnixGracefulSignal::Interrupt => this.group.send_interrupt(),
                    UnixGracefulSignal::Terminate => this.group.send_terminate(),
                },
            )
            .await
        }
        #[cfg(windows)]
        {
            self.terminate_with_hooks(&shutdown.windows, Self::try_reap_exit_status, |this| {
                this.group.send_ctrl_break()
            })
            .await
        }
    }

    /// Test-only fault-injection seam underneath [`terminate`](Self::terminate). Drives the
    /// termination state machine with caller-supplied hooks for the preflight exit-status poll and
    /// the per-phase signal send. Production code should call [`terminate`](Self::terminate)
    /// instead, which wires `try_reap_exit_status` to the real `Child::try_wait` path and
    /// `send_signal` to the platform-appropriate process-group signaler.
    ///
    /// `try_reap_exit_status` is consulted once before the first signal send so an already-exited
    /// child is observed without sending. `send_signal` is called once per phase of `sequence`.
    ///
    /// Drop-guard semantics, escalation to the forceful kill fallback, and the post-success disarm,
    /// all described through the public `terminate`, are handled here.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if signaling or waiting for process termination fails.
    #[doc(hidden)]
    #[cfg(unix)]
    pub async fn terminate_with_hooks<ExitStatusReaper, SignalSender>(
        &mut self,
        sequence: &UnixGracefulShutdown,
        try_reap_exit_status: ExitStatusReaper,
        mut send_signal: SignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        ExitStatusReaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
        SignalSender: FnMut(&mut Self, UnixGracefulSignal) -> Result<(), io::Error>,
    {
        let phases = sequence.phases();
        let steps: Vec<TerminationStep> = phases
            .iter()
            .map(|phase| TerminationStep {
                signal_label: phase.signal.label(),
                timeout: phase.timeout,
            })
            .collect();

        self.run_termination_loop(&steps, try_reap_exit_status, |this, index, _step| {
            send_signal(this, phases[index].signal)
        })
        .await
    }

    /// Test-only fault-injection seam underneath [`terminate`](Self::terminate). Drives the
    /// termination state machine with caller-supplied hooks for the preflight exit-status poll
    /// and the signal send. Production code should call [`terminate`](Self::terminate)
    /// instead, which wires `try_reap_exit_status` to the real `Child::try_wait` path and
    /// `send_signal` to the platform-appropriate process-group signaller.
    ///
    /// `try_reap_exit_status` is consulted once before the signal send so an already-exited
    /// child is observed without sending. `send_signal` is called once (Windows always sends a
    /// single `CTRL_BREAK_EVENT`). Drop-guard semantics, escalation to the forceful kill
    /// fallback, and the post-success disarm all behave identically to `terminate`.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if signalling or waiting for process termination fails.
    #[doc(hidden)]
    #[cfg(windows)]
    pub async fn terminate_with_hooks<PreflightReaper, SignalSender>(
        &mut self,
        sequence: &WindowsGracefulShutdown,
        try_reap_exit_status: PreflightReaper,
        mut send_signal: SignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        PreflightReaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
        SignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        let steps = [TerminationStep {
            signal_label: "CTRL_BREAK_EVENT",
            timeout: sequence.timeout,
        }];

        self.run_termination_loop(&steps, try_reap_exit_status, |this, _index, _step| {
            send_signal(this)
        })
        .await
    }

    /// Cross-platform termination driver. Iterates `steps` in order, sending each phase's
    /// signal via `send_signal` and waiting up to its `timeout` for the child to exit before
    /// escalating. Falls back to the implicit forceful kill (`SIGKILL` on Unix,
    /// `TerminateProcess` on Windows) after the last step. `try_reap_exit_status` is consulted
    /// once before the first signal send so an already-exited child is observed without sending.
    ///
    /// `steps` must be non-empty. Per-platform wrappers enforce this: `UnixGracefulShutdown`
    /// rejects empty input at construction, and the Windows path always builds a single-element
    /// slice.
    #[cfg(any(unix, windows))]
    async fn run_termination_loop<PreflightReaper, SignalSender>(
        &mut self,
        steps: &[TerminationStep],
        mut try_reap_exit_status: PreflightReaper,
        mut send_signal: SignalSender,
    ) -> Result<ExitStatus, TerminationError>
    where
        PreflightReaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
        SignalSender: FnMut(&mut Self, usize, &TerminationStep) -> Result<(), io::Error>,
    {
        debug_assert!(
            !steps.is_empty(),
            "run_termination_loop requires at least one graceful step",
        );

        let result = 'termination: {
            let mut diagnostics = TerminationDiagnostics::default();
            let first_phase_label = steps.first().map_or(KILL_LABEL, |step| step.signal_label);

            match try_reap_exit_status(self) {
                Ok(Some(exit_status)) => {
                    break 'termination Ok(exit_status);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        process = %self.name,
                        signal = first_phase_label,
                        error = %err,
                        "Could not determine process state before termination. Attempting first graceful phase."
                    );
                    diagnostics.record(TerminationAction::CheckStatus, err);
                }
            }

            for (index, step) in steps.iter().enumerate() {
                let next_label = steps
                    .get(index + 1)
                    .map_or(KILL_LABEL, |next| next.signal_label);
                let send = &mut send_signal;

                let outcome = self
                    .attempt_graceful_phase(
                        step.signal_label,
                        next_label,
                        step.timeout,
                        &mut diagnostics,
                        &mut |this: &mut Self| send(this, index, step),
                    )
                    .await;

                if let Some(exit_status) = outcome {
                    break 'termination Ok(exit_status);
                }
            }

            self.attempt_forceful_kill(diagnostics).await
        };

        self.disarm_after_successful_termination(result)
    }

    /// Test-only helper for verifying that an `Err` termination result leaves the drop guards
    /// armed. Disarms only on `Ok`; production code uses [`terminate`](Self::terminate) which
    /// applies this internally. Exposed so integration tests can drive the disarm-on-success
    /// contract with synthetic results without going through a real signal-injection sequence.
    #[doc(hidden)]
    pub fn disarm_after_successful_termination<T>(
        &mut self,
        result: Result<T, TerminationError>,
    ) -> Result<T, TerminationError> {
        if result.is_ok() {
            self.must_not_be_terminated();
        }
        result
    }

    /// Send the graceful signal for one phase and wait up to `timeout` for the child to exit.
    ///
    /// Returns `Some(exit_status)` if the child exits during the phase. Returns `None` to escalate
    /// to the next phase, recording a diagnostic for whatever went wrong (signal-send failure,
    /// post-signal wait timeout, or wait error). When the signal send itself fails, also probes
    /// briefly with [`REAP_AFTER_SIGNAL_FAILURE_GRACE`] so a freshly-exited child is observed as
    /// exited rather than as still running.
    async fn attempt_graceful_phase<SignalSender>(
        &mut self,
        signal_name: &'static str,
        next_signal_name: &'static str,
        timeout: Duration,
        diagnostics: &mut TerminationDiagnostics,
        send_signal: &mut SignalSender,
    ) -> Option<ExitStatus>
    where
        SignalSender: FnMut(&mut Self) -> Result<(), io::Error>,
    {
        match send_signal(self) {
            Ok(()) => match self.wait_for_exit_after_signal(timeout).await {
                Ok(Some(exit_status)) => Some(exit_status),
                Ok(None) => {
                    let not_terminated = wait_timeout_error(timeout);
                    tracing::warn!(
                        process = %self.name,
                        signal = signal_name,
                        next_signal = next_signal_name,
                        error = %not_terminated,
                        "Graceful shutdown signal timed out. Attempting next shutdown phase."
                    );
                    diagnostics.record(TerminationAction::WaitForExit, not_terminated);
                    None
                }
                Err(wait_error) => {
                    tracing::warn!(
                        process = %self.name,
                        signal = signal_name,
                        next_signal = next_signal_name,
                        error = %wait_error,
                        "Wait for graceful shutdown failed. Attempting next shutdown phase."
                    );
                    diagnostics.record(TerminationAction::WaitForExit, wait_error);
                    None
                }
            },
            Err(send_error) => {
                tracing::warn!(
                    process = %self.name,
                    signal = signal_name,
                    next_signal = next_signal_name,
                    error = %send_error,
                    "Graceful shutdown signal could not be sent. Attempting next shutdown phase."
                );
                diagnostics.record(TerminationAction::SendSignal { signal_name }, send_error);

                match self
                    .wait_for_exit_after_signal(REAP_AFTER_SIGNAL_FAILURE_GRACE)
                    .await
                {
                    Ok(Some(exit_status)) => Some(exit_status),
                    Ok(None) => None,
                    Err(reap_error) => {
                        tracing::warn!(
                            process = %self.name,
                            signal = signal_name,
                            error = %reap_error,
                            "Could not determine process state after graceful signal send failed."
                        );
                        diagnostics.record(TerminationAction::CheckStatus, reap_error);
                        None
                    }
                }
            }
        }
    }

    async fn attempt_forceful_kill(
        &mut self,
        mut diagnostics: TerminationDiagnostics,
    ) -> Result<ExitStatus, TerminationError> {
        match self.send_kill_signal() {
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
                    Ok(Some(exit_status)) => Ok(exit_status),
                    Ok(None) => {
                        let not_terminated_after_kill = wait_timeout_error(FORCE_KILL_WAIT_TIMEOUT);
                        // Unlikely. See the note above.
                        tracing::error!(
                            process = %self.name,
                            kill_signal = KILL_LABEL,
                            "Process did not terminate after all termination attempts. Process may still be running. Manual intervention and investigation required!"
                        );
                        diagnostics
                            .record(TerminationAction::WaitForExit, not_terminated_after_kill);
                        Err(diagnostics.into_termination_failed(self.name.clone()))
                    }
                    Err(not_terminated_after_kill) => {
                        // Unlikely. See the note above.
                        tracing::error!(
                            process = %self.name,
                            kill_signal = KILL_LABEL,
                            "Process did not terminate after all termination attempts. Process may still be running. Manual intervention and investigation required!"
                        );
                        diagnostics
                            .record(TerminationAction::WaitForExit, not_terminated_after_kill);
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
                diagnostics.record(
                    TerminationAction::SendSignal {
                        signal_name: KILL_LABEL,
                    },
                    kill_error,
                );

                // Brief grace for Tokio's SIGCHLD reaper to catch up - see
                // `REAP_AFTER_SIGNAL_FAILURE_GRACE`.
                match self
                    .wait_for_exit_after_signal(REAP_AFTER_SIGNAL_FAILURE_GRACE)
                    .await
                {
                    Ok(Some(exit_status)) => {
                        return Ok(exit_status);
                    }
                    Ok(None) => {}
                    Err(reap_error) => {
                        tracing::warn!(
                            process = %self.name,
                            signal = KILL_LABEL,
                            error = %reap_error,
                            "Could not determine process state after forceful shutdown failed."
                        );
                        diagnostics.record(TerminationAction::CheckStatus, reap_error);
                    }
                }

                Err(diagnostics.into_termination_failed(self.name.clone()))
            }
        }
    }
}

fn wait_timeout_error(timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("process did not complete within {timeout:?}"),
    )
}
