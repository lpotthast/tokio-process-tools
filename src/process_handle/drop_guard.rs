#[cfg(any(unix, windows))]
use super::termination::GracefulTimeouts;
use super::{DropMode, ProcessHandle};
use crate::output_stream::OutputStream;
use crate::panic_on_drop::PanicOnDrop;
#[cfg(any(unix, windows))]
use crate::terminate_on_drop::TerminateOnDrop;

impl<Stdout, Stderr> Drop for ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    fn drop(&mut self) {
        match &self.drop_mode {
            DropMode::Armed { .. } => {
                // We want users to explicitly await or terminate spawned processes.
                // If not done so, kill the process group now to have some sort of last-resort
                // cleanup. The panic guard will additionally raise a panic when this method
                // returns, signalling the misuse loudly. Targeting the group (rather than the
                // child's PID alone) catches any grandchildren the child has fork-execed, which
                // is the same invariant the explicit `kill()` path upholds.
                if let Err(err) = drop_kill(&mut self.child) {
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

    /// Sets a panic-on-drop mechanism for this `ProcessHandle`.
    ///
    /// This method enables a safeguard that ensures that the process represented by this
    /// `ProcessHandle` is properly terminated or awaited before being dropped.
    /// If `must_be_terminated` is set and the `ProcessHandle` is
    /// dropped without successfully terminating, killing, waiting for, or explicitly detaching the
    /// process, an intentional panic will occur to prevent silent failure-states, ensuring that
    /// system resources are handled correctly.
    ///
    /// You typically do not need to call this, as every `ProcessHandle` is marked by default.
    /// Call `must_not_be_terminated` to clear this safeguard to explicitly allow dropping the
    /// process without terminating it.
    /// Calling this method while the safeguard is already enabled is safe and has no effect beyond
    /// keeping the handle armed.
    ///
    /// # Panic
    ///
    /// If the `ProcessHandle` is dropped without being awaited or terminated successfully
    /// after calling this method, a panic will occur with a descriptive message
    /// to inform about the incorrect usage.
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
    /// Also, the right opt-out after [`terminate`](Self::terminate) returns an unrecoverable error
    /// and the caller chooses to accept the failure instead of retrying or escalating to
    /// [`kill`](Self::kill).
    pub fn must_not_be_terminated(&mut self) {
        // Defuse the panic guard before swapping the variant so the dropped `PanicOnDrop` does
        // not fire when the old `Armed` value is dropped by the assignment.
        if let DropMode::Armed { panic } = &mut self.drop_mode {
            panic.defuse();
        }
        self.drop_mode = DropMode::Disarmed;
    }

    #[cfg(test)]
    pub(crate) fn is_drop_armed(&self) -> bool {
        matches!(&self.drop_mode, DropMode::Armed { panic } if panic.is_armed())
    }

    #[cfg(test)]
    pub(crate) fn is_drop_disarmed(&self) -> bool {
        matches!(self.drop_mode, DropMode::Disarmed)
    }

    /// Wrap this process handle in a `TerminateOnDrop` instance, terminating the controlled process
    /// automatically when this handle is dropped.
    ///
    /// `timeouts` carries the same per-platform graceful budget as [`Self::terminate`]; see
    /// [`GracefulTimeouts`] for how to construct it.
    ///
    /// **SAFETY: This only works when your code is running in a multithreaded tokio runtime!**
    ///
    /// Prefer manual termination of the process or awaiting it and relying on the (automatically
    /// configured) `must_be_terminated` logic, raising a panic when a process was neither awaited
    /// nor terminated before being dropped.
    #[cfg(any(unix, windows))]
    pub fn terminate_on_drop(self, timeouts: GracefulTimeouts) -> TerminateOnDrop<Stdout, Stderr> {
        TerminateOnDrop {
            process_handle: self,
            timeouts,
        }
    }
}

fn drop_kill(child: &mut tokio::process::Child) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        match child.id() {
            Some(pid) => crate::signal::send_kill_to_process_group(pid),
            None => child.start_kill(),
        }
    }
    #[cfg(not(unix))]
    {
        // Tokio's `Child::start_kill` works on every platform, so the Drop best-effort cleanup
        // remains available even on targets where the rest of the termination machinery is gated
        // out (anything that is neither `cfg(unix)` nor `cfg(windows)`).
        child.start_kill()
    }
}

fn armed_panic_guard() -> PanicOnDrop {
    PanicOnDrop::new(
        "tokio_process_tools::ProcessHandle",
        "The process was not terminated.",
        "Successfully call `wait_for_completion`, `terminate`, or `kill`, or call `must_not_be_terminated` before the type is dropped!",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::long_running_command;
    use crate::{
        BestEffortDelivery, BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, NoReplay,
    };
    use assertr::prelude::*;
    use std::time::Duration;

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

    #[tokio::test]
    async fn must_be_terminated_is_idempotent_when_already_armed() {
        let mut process = spawn_long_running_process();

        process.must_be_terminated();
        assert_that!(process.is_drop_armed()).is_true();

        process.kill().await.unwrap();
    }

    #[tokio::test]
    async fn must_be_terminated_re_arms_safeguards_after_opt_out() {
        let mut process = spawn_long_running_process();

        process.must_not_be_terminated();
        assert_that!(process.is_drop_disarmed()).is_true();

        process.must_be_terminated();
        assert_that!(process.is_drop_armed()).is_true();

        process.kill().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn must_not_be_terminated_lets_child_outlive_dropped_handle() {
        use nix::errno::Errno;
        use nix::sys::signal::{self, Signal};
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;

        let mut process = spawn_long_running_process();
        let pid = process.id().unwrap();

        process.must_not_be_terminated();
        assert_that!(process.is_drop_disarmed()).is_true();
        drop(process);

        let pid = Pid::from_raw(pid.cast_signed());
        assert_that!(signal::kill(pid, None).is_ok()).is_true();

        signal::kill(pid, Signal::SIGKILL).unwrap();
        match waitpid(pid, None) {
            Ok(_) | Err(Errno::ECHILD) => {}
            Err(err) => {
                assert_that!(err).fail(format_args!("waitpid failed: {err}"));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn must_not_be_terminated_still_closes_stdin_on_drop() {
        use nix::errno::Errno;
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;
        use std::fs;
        use tempfile::tempdir;

        let temp_dir = tempdir().unwrap();
        let output_file = temp_dir.path().join("stdin-result.txt");
        let output_file = output_file.to_str().unwrap().replace('\'', "'\"'\"'");

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("cat >/dev/null; printf eof > '{output_file}'"));

        let mut process = crate::Process::new(cmd)
            .name("sh")
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
        let pid = Pid::from_raw(process.id().unwrap().cast_signed());

        process.must_not_be_terminated();
        drop(process);

        match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || waitpid(pid, None)),
        )
        .await
        .unwrap()
        .unwrap()
        {
            Ok(_) | Err(Errno::ECHILD) => {}
            Err(err) => {
                assert_that!(err).fail(format_args!("waitpid failed: {err}"));
            }
        }

        assert_that!(fs::read_to_string(temp_dir.path().join("stdin-result.txt")).unwrap())
            .is_equal_to("eof");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn must_not_be_terminated_still_closes_stdout_pipe_on_drop() {
        use nix::errno::Errno;
        use nix::sys::wait::waitpid;
        use nix::unistd::Pid;

        let mut cmd = tokio::process::Command::new("yes");
        cmd.arg("tick");

        let mut process = crate::Process::new(cmd)
            .name("yes")
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
        let pid = Pid::from_raw(process.id().unwrap().cast_signed());

        process.must_not_be_terminated();
        drop(process);

        match tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || waitpid(pid, None)),
        )
        .await
        .unwrap()
        .unwrap()
        {
            Ok(_) | Err(Errno::ECHILD) => {}
            Err(err) => {
                assert_that!(err).fail(format_args!("waitpid failed: {err}"));
            }
        }
    }
}
