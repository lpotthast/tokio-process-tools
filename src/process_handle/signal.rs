//! Manual signal delivery: user-callable shortcuts for sending a single platform signal to the
//! child's process group.
//!
//! These helpers sit alongside the termination flow (`process_handle::termination`) but are not
//! part of it. They preflight-reap an already-exited child, send the signal, and then re-probe on
//! signal-send failure so a freshly-exited child is observed as exited rather than as still
//! running. They are synchronous on purpose to keep the public `send_*_signal` API non-`async`.

use super::ProcessHandle;
use super::termination::TerminationDiagnostics;
use crate::error::{TerminationAction, TerminationError};
use crate::output_stream::OutputStream;
use std::io;
use std::process::ExitStatus;

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
    /// # Notes
    ///
    /// The post-failure exit-status probe is a single synchronous `try_wait`. On the rare race
    /// where the child has just exited but Tokio's SIGCHLD reaper has not yet observed it, the
    /// probe returns "still running" and this method surfaces the OS-level signal-send error
    /// (typically `EPERM` on macOS or `ESRCH` on Linux) as
    /// [`TerminationError::SignalFailed`]. The synchronous shape is deliberate so the public
    /// `send_*_signal` API stays non-async; reach for [`Self::terminate`] when you need the
    /// bounded SIGCHLD-grace wait.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if `SIGINT`
    /// could not be sent.
    #[cfg(unix)]
    pub fn send_interrupt_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_reaper(
            "SIGINT",
            |this| this.group.send_interrupt(),
            Self::try_reap_exit_status,
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
    /// # Notes
    ///
    /// The post-failure exit-status probe is a single synchronous `try_wait`. On the rare race
    /// where the child has just exited but Tokio's SIGCHLD reaper has not yet observed it, the
    /// probe returns "still running" and this method surfaces the OS-level signal-send error
    /// (typically `EPERM` on macOS or `ESRCH` on Linux) as
    /// [`TerminationError::SignalFailed`]. The synchronous shape is deliberate so the public
    /// `send_*_signal` API stays non-async; reach for [`Self::terminate`] when you need the
    /// bounded SIGCHLD-grace wait.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if `SIGTERM`
    /// could not be sent.
    #[cfg(unix)]
    pub fn send_terminate_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_reaper(
            "SIGTERM",
            |this| this.group.send_terminate(),
            Self::try_reap_exit_status,
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
    /// # Notes
    ///
    /// The post-failure exit-status probe is a single synchronous `try_wait`. On the rare race
    /// where the child has just exited but Tokio has not yet observed it, the probe returns
    /// "still running" and this method surfaces the OS-level
    /// `GenerateConsoleCtrlEvent` failure as [`TerminationError::SignalFailed`]. The synchronous
    /// shape is deliberate so the public `send_*_signal` API stays non-async; reach for
    /// [`Self::terminate`] when you need the bounded reaper-grace wait.
    ///
    /// # Errors
    ///
    /// Returns [`TerminationError`] if the process status could not be checked or if
    /// `CTRL_BREAK_EVENT` could not be delivered.
    #[cfg(windows)]
    pub fn send_ctrl_break_signal(&mut self) -> Result<(), TerminationError> {
        self.send_signal_with_reaper(
            "CTRL_BREAK_EVENT",
            |this| this.group.send_ctrl_break(),
            Self::try_reap_exit_status,
        )
    }

    /// Test-only fault-injection seam underneath `send_*_signal`.
    ///
    /// Drives a single signal-send with caller-supplied hooks for the signal send and the
    /// preflight/post-failure exit-status poll. Production code should call
    /// [`send_interrupt_signal`](Self::send_interrupt_signal),
    /// [`send_terminate_signal`](Self::send_terminate_signal), or
    /// [`send_ctrl_break_signal`](Self::send_ctrl_break_signal) instead.
    #[doc(hidden)]
    pub fn send_signal_with_reaper<SignalSender, Reaper>(
        &mut self,
        signal_name: &'static str,
        send_signal: SignalSender,
        mut try_reap_exit_status: Reaper,
    ) -> Result<(), TerminationError>
    where
        SignalSender: FnOnce(&mut Self) -> Result<(), io::Error>,
        Reaper: FnMut(&mut Self) -> Result<Option<ExitStatus>, io::Error>,
    {
        let mut diagnostics = TerminationDiagnostics::default();

        match try_reap_exit_status(self) {
            Ok(Some(_)) => {
                self.must_not_be_terminated();
                Ok(())
            }
            Ok(None) => match send_signal(self) {
                Ok(()) => Ok(()),
                // Sync probe only - the SIGCHLD-grace bounded wait lives on the `terminate()`
                // path. Keeping this sync avoids making the public `send_*_signal` APIs async.
                Err(signal_error) => match try_reap_exit_status(self) {
                    Ok(Some(_)) => {
                        self.must_not_be_terminated();
                        Ok(())
                    }
                    Ok(None) => {
                        diagnostics
                            .record(TerminationAction::SendSignal { signal_name }, signal_error);
                        Err(diagnostics.into_signal_failed(self.name.clone()))
                    }
                    Err(reap_error) => {
                        diagnostics
                            .record(TerminationAction::SendSignal { signal_name }, signal_error);
                        diagnostics.record(TerminationAction::CheckStatus, reap_error);
                        Err(diagnostics.into_signal_failed(self.name.clone()))
                    }
                },
            },
            Err(status_error) => {
                diagnostics.record(TerminationAction::CheckStatus, status_error);
                Err(diagnostics.into_signal_failed(self.name.clone()))
            }
        }
    }
}
