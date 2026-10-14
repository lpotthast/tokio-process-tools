use super::ProcessHandle;
#[cfg(any(unix, windows))]
use super::termination::GracefulShutdown;
use crate::output_stream::OutputStream;
use crate::panic_on_drop::PanicOnDrop;
#[cfg(any(unix, windows))]
use crate::terminate_on_drop::{TerminateOnDrop, TerminateOnDropFallback};

/// Drop-time behavior selected by the lifecycle methods on [`ProcessHandle`].
///
/// The state machine has two reachable states because every public lifecycle entry point either
/// keeps both safeguards on (`Armed`) or turns both off (`Disarmed`). There is no "panic only,
/// no cleanup" or "cleanup only, no panic" combination: the panic guard makes sense only when
/// paired with the kill that signals the misuse.
#[derive(Debug)]
pub(crate) enum DropMode {
    /// Cleanup is attempted on drop and the panic guard fires when it does.
    Armed { panic: PanicOnDrop },

    /// Both cleanup and the panic guard are off. Drop is a no-op for this handle's lifecycle.
    Disarmed,
}

impl<Stdout, Stderr> Drop for ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn drop(&mut self) {
        match &self.drop_mode {
            DropMode::Armed { .. } => {
                // We want users to explicitly await or terminate spawned processes.
                // If not done so, kill the process group/job now to have some sort of
                // last-resort cleanup. The panic guard will additionally raise a panic when this
                // method returns, signaling the misuse loudly. Targeting the group/job (rather
                // than the child's PID alone) catches any grandchildren the child has spawned,
                // which is the same invariant the explicit `kill()` path upholds.
                if let Err(err) = self.send_kill_signal() {
                    tracing::warn!(
                        process = %self.name,
                        error = %err,
                        "Failed to kill process while dropping an armed ProcessHandle"
                    );
                }
            }
            DropMode::Disarmed => {}
        }
    }
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    pub(super) fn new_armed_drop_mode() -> DropMode {
        DropMode::Armed {
            panic: armed_panic_guard(),
        }
    }

    /// Arms the kill-and-panic-on-drop safeguard for this `ProcessHandle`.
    ///
    /// While armed, dropping the handle without successfully terminating, killing, waiting for,
    /// or explicitly detaching the process performs a best-effort kill of the process group and
    /// then panics, so a leaked process is loud instead of silent.
    ///
    /// You typically do not need to call this, as every `ProcessHandle` starts armed. It exists
    /// to re-arm a handle after [`must_not_be_terminated`](Self::must_not_be_terminated). Calling
    /// it while the safeguard is already armed is safe and has no further effect.
    ///
    /// # Panic
    ///
    /// If the `ProcessHandle` is dropped without being awaited or terminated successfully
    /// after calling this method, a panic with a descriptive message points out the incorrect
    /// usage.
    pub fn must_be_terminated(&mut self) {
        match &mut self.drop_mode {
            DropMode::Armed { panic } if panic.is_armed() => {
                // Already armed; nothing to do.
            }
            _ => {
                self.drop_mode = DropMode::Armed {
                    panic: armed_panic_guard(),
                };
            }
        }
    }

    /// Disables the kill/panic-on-drop safeguards for this handle.
    ///
    /// Dropping the handle after calling this method will no longer signal, kill, or panic.
    /// However, this does **not** keep the library-owned stdio pipes alive. If the child still
    /// depends on stdin, stdout, or stderr being open, dropping the handle may still affect it.
    ///
    /// Use plain [`tokio::process::Command`] directly when you need a child process that can
    /// outlive the original handle without depending on captured stdio pipes.
    ///
    /// This is also the right opt-out after [`terminate`](Self::terminate) returns an
    /// unrecoverable error and the caller chooses to accept the failure instead of retrying or
    /// escalating to [`kill`](Self::kill).
    pub fn must_not_be_terminated(&mut self) {
        // Defuse the panic guard before swapping the variant so the dropped `PanicOnDrop` does
        // not fire when the old `Armed` value is dropped by the assignment.
        if let DropMode::Armed { panic } = &mut self.drop_mode {
            panic.defuse();
        }
        self.drop_mode = DropMode::Disarmed;
    }

    /// Test-only inspector: whether the drop guard is currently armed.
    #[doc(hidden)]
    pub fn is_drop_armed(&self) -> bool {
        matches!(&self.drop_mode, DropMode::Armed { panic } if panic.is_armed())
    }

    /// Whether the drop guard has been disarmed, either by a successful terminal call or by
    /// [`must_not_be_terminated`](Self::must_not_be_terminated).
    #[doc(hidden)]
    pub fn is_drop_disarmed(&self) -> bool {
        matches!(self.drop_mode, DropMode::Disarmed)
    }

    /// Wraps this process handle in a [`TerminateOnDrop`], which terminates the process when it is
    /// dropped.
    ///
    /// `shutdown` carries the same per-platform graceful policy as [`Self::terminate`]. See
    /// [`GracefulShutdown`] for how to construct it.
    ///
    /// The graceful shutdown can only run when the wrapper is dropped on a thread inside a
    /// **multithreaded** tokio runtime. Anywhere else, the process is killed immediately and the
    /// drop panics, as with [`TerminateOnDropFallback::Panic`]. Use
    /// [`Self::terminate_on_drop_with_fallback`] to choose a different fallback. See
    /// [`TerminateOnDrop`] for the full drop behavior.
    ///
    /// Prefer terminating or awaiting the process explicitly where possible.
    #[cfg(any(unix, windows))]
    pub fn terminate_on_drop(self, shutdown: GracefulShutdown) -> TerminateOnDrop<Stdout, Stderr> {
        self.terminate_on_drop_with_fallback(shutdown, TerminateOnDropFallback::Panic)
    }

    /// Like [`Self::terminate_on_drop`], but with an explicit choice of how a drop in a context
    /// that cannot run the graceful shutdown is reported.
    ///
    /// See [`TerminateOnDropFallback`] for the available policies and [`TerminateOnDrop`] for the
    /// full drop behavior.
    #[cfg(any(unix, windows))]
    pub fn terminate_on_drop_with_fallback(
        self,
        shutdown: GracefulShutdown,
        fallback: TerminateOnDropFallback,
    ) -> TerminateOnDrop<Stdout, Stderr> {
        TerminateOnDrop {
            process_handle: self,
            shutdown,
            fallback,
        }
    }
}

fn armed_panic_guard() -> PanicOnDrop {
    PanicOnDrop::new(
        "tokio_process_tools::ProcessHandle",
        "The process was not terminated.",
        "Successfully call `wait_for_completion`, `terminate`, or `kill`, or call `must_not_be_terminated` before the type is dropped!",
    )
}
