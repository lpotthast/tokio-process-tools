use crate::async_drop::UnsupportedContext;
use crate::process_handle::ProcessHandle;
use crate::process_handle::termination::GracefulShutdown;
use crate::{OutputStream, RunningState, async_drop};
use std::ops::{Deref, DerefMut};

/// How a [`TerminateOnDrop`] reports that it was dropped in a context that cannot run its
/// graceful shutdown.
///
/// The policy only decides how the problem is reported. With either variant, the child's process
/// group / Job Object is killed immediately before the report is made. See
/// [`TerminateOnDrop`] for which contexts are unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminateOnDropFallback {
    /// Panic after killing the child.
    ///
    /// Use this when dropping outside a multithreaded Tokio runtime is a programming error that
    /// should fail loudly in normal execution and tests. This is what
    /// [`ProcessHandle::terminate_on_drop`] uses.
    ///
    /// If the thread is already unwinding from another panic, the drop logs a warning instead,
    /// because a second panic from `Drop` would abort the process and hide the original failure.
    Panic,

    /// Log an error through `tracing` after killing the child, without panicking.
    ///
    /// Use this when the wrapper may legitimately be dropped outside a supported context. Make
    /// sure a `tracing` subscriber records error events, or the lost graceful shutdown goes
    /// unnoticed.
    Trace,
}

/// A wrapper that terminates its process when dropped.
///
/// Created by [`ProcessHandle::terminate_on_drop`] or
/// [`ProcessHandle::terminate_on_drop_with_fallback`]. It dereferences to the wrapped
/// [`ProcessHandle`], so the process can still be used, awaited, or terminated explicitly.
///
/// # Drop behavior
///
/// When dropped, the wrapper first checks synchronously whether there is anything left to do.
/// It leaves the process alone if the handle's lifecycle was already closed, either by a
/// successful wait, `terminate`, or `kill`, or by [`ProcessHandle::must_not_be_terminated`].
/// Nothing is done either if the child has already exited.
///
/// Otherwise, the wrapper runs the [`GracefulShutdown`] it was created with. Because `Drop` is
/// synchronous, this is only possible when the wrapper is dropped from a thread inside a
/// **multithreaded** Tokio runtime: the drop then blocks the current thread (via
/// `block_in_place`) while the runtime drives the termination. The graceful shutdown cannot run
/// when the wrapper is dropped
///
/// - on a thread outside any Tokio runtime, e.g. in `main` after `Runtime::block_on` returned,
///   even while a runtime is still alive elsewhere,
/// - on a thread inside a current-thread Tokio runtime, which includes plain `#[tokio::test]`
///   (use `#[tokio::test(flavor = "multi_thread")]` instead),
/// - on a thread that is unwinding from a panic.
///
/// In these contexts, the process group / Job Object is killed immediately instead, and the
/// configured [`TerminateOnDropFallback`] decides how this is reported. The killed child is not
/// reaped, as that would require an async wait. On Unix, Tokio's orphan reaper collects it while
/// a runtime is still alive. Otherwise, the zombie entry is released when the parent process
/// exits.
///
/// If the graceful shutdown runs but fails, the error is logged through `tracing` and the
/// wrapped handle stays armed. Its own drop safeguard then attempts a forceful kill and raises
/// the generic "process was not terminated" panic. The original termination error is only
/// available through `tracing`, not through the panic payload.
///
/// # When to use
///
/// Prefer explicit cleanup where possible:
///
/// 1. Terminate the process with [`ProcessHandle::terminate`].
/// 2. Await its completion with the [`ProcessHandle::wait_for_completion`] builder.
/// 3. Await its completion with the same builder and terminate it on timeout using
///    `.or_terminate(...)`.
///
/// Reach for this wrapper when the process follows the lifecycle of some owner that cannot run
/// async cleanup itself, and the owner is dropped inside a multithreaded Tokio runtime.
#[derive(Debug)]
pub struct TerminateOnDrop<Stdout: OutputStream, Stderr: OutputStream = Stdout> {
    pub(crate) process_handle: ProcessHandle<Stdout, Stderr>,
    pub(crate) shutdown: GracefulShutdown,
    pub(crate) fallback: TerminateOnDropFallback,
}

impl<Stdout, Stderr> Deref for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    type Target = ProcessHandle<Stdout, Stderr>;

    fn deref(&self) -> &Self::Target {
        &self.process_handle
    }
}

impl<Stdout, Stderr> DerefMut for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.process_handle
    }
}

impl<Stdout, Stderr> TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Returns whether drop has nothing left to terminate, closing the handle's lifecycle if the
    /// child turns out to have exited already.
    ///
    /// This only probes the child with a non-blocking `try_wait`, so it works in every thread
    /// context, including those that cannot run the graceful shutdown.
    fn nothing_to_terminate(&mut self) -> bool {
        if self.process_handle.is_drop_disarmed() {
            tracing::debug!(
                process = %self.process_handle.name,
                "Process lifecycle already closed or detached. Leaving the process alone."
            );
            return true;
        }

        match self.process_handle.is_running() {
            RunningState::Terminated(_) => {
                tracing::debug!(
                    process = %self.process_handle.name,
                    "Process already terminated."
                );
                self.process_handle.must_not_be_terminated();
                true
            }
            RunningState::Running => false,
            RunningState::Uncertain(err) => {
                tracing::warn!(
                    process = %self.process_handle.name,
                    error = %err,
                    "Could not determine process state during drop. Attempting termination anyway."
                );
                false
            }
        }
    }

    async fn terminate_gracefully(&mut self) {
        tracing::debug!(process = %self.process_handle.name, "Terminating process.");

        let shutdown = self.shutdown.clone();
        match self.process_handle.terminate(shutdown).await {
            Ok(exit_status) => {
                tracing::debug!(
                    process = %self.process_handle.name,
                    ?exit_status,
                    "Successfully terminated process."
                );
            }
            Err(err) => {
                tracing::error!(
                    process = %self.process_handle.name,
                    error = %err,
                    error_details = ?err,
                    "Failed to terminate process during drop."
                );
            }
        }
    }

    /// Kills the process group immediately and reports, according to the configured fallback,
    /// that the graceful shutdown could not run in `context`.
    fn terminate_forcefully(&mut self, context: UnsupportedContext) {
        if let Err(err) = self.process_handle.start_kill() {
            tracing::error!(
                process = %self.process_handle.name,
                error = %err,
                error_details = ?err,
                "Failed to kill process during drop. The process may be leaked."
            );
            // Close the lifecycle anyway. Otherwise, the wrapped handle's drop safeguard would
            // repeat the kill and then panic regardless of the configured fallback.
            self.process_handle.must_not_be_terminated();
        }

        let name = &self.process_handle.name;
        let reason = context.describe();

        // Even under the `Panic` fallback, a panic during an unwind would abort the process and
        // hide the original panic, so the unwinding case is always just logged.
        if context == UnsupportedContext::Panicking {
            tracing::warn!(
                process = %name,
                "TerminateOnDrop was dropped {reason}. Killed the process instead of shutting it \
                 down gracefully."
            );
            return;
        }

        match self.fallback {
            TerminateOnDropFallback::Trace => {
                tracing::error!(
                    process = %name,
                    "TerminateOnDrop was dropped {reason}, which cannot run the graceful \
                     shutdown. Killed the process instead. Terminate the process explicitly \
                     before dropping it, or drop it on a thread inside a multithreaded tokio \
                     runtime."
                );
            }
            TerminateOnDropFallback::Panic => {
                panic!(
                    "TerminateOnDrop for process '{name}' was dropped {reason}, which cannot \
                     run the graceful shutdown. The process was killed before this panic. \
                     Terminate the process explicitly before dropping it, drop it on a thread \
                     inside a multithreaded tokio runtime, or use TerminateOnDropFallback::Trace."
                );
            }
        }
    }
}

impl<Stdout, Stderr> Drop for TerminateOnDrop<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn drop(&mut self) {
        if self.nothing_to_terminate() {
            return;
        }

        if let Err(context) = async_drop::try_run_future(self.terminate_gracefully()) {
            self.terminate_forcefully(context);
        }
    }
}
