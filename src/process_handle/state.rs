use super::{ProcessHandle, RunningState};
use crate::output_stream::OutputStream;
use std::io;
use std::process::ExitStatus;

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Returns the OS process ID if the process hasn't exited yet.
    ///
    /// Once this process has been polled to completion this will return None.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub(super) fn try_reap_exit_status(&mut self) -> Result<Option<ExitStatus>, io::Error> {
        match self.child.try_wait() {
            Ok(Some(exit_status)) => {
                self.must_not_be_terminated();
                Ok(Some(exit_status))
            }
            Ok(None) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Checks if the process is currently running.
    ///
    /// Returns [`RunningState::Running`] if the process is still running,
    /// [`RunningState::Terminated`] if it has exited, or [`RunningState::Uncertain`]
    /// if the state could not be determined.
    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> RunningState {
        match self.try_reap_exit_status() {
            Ok(None) => RunningState::Running,
            Ok(Some(exit_status)) => RunningState::Terminated(exit_status),
            Err(err) => RunningState::Uncertain(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::long_running_command;
    use crate::{
        AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process, RunningState,
    };
    use assertr::prelude::*;
    use std::time::Duration;

    #[tokio::test]
    async fn is_running_reports_running_before_wait_and_terminated_after_wait() {
        let mut process = Process::new(long_running_command(Duration::from_secs(1)))
            .name(AutoName::program_only())
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

        match process.is_running() {
            RunningState::Running => {}
            RunningState::Terminated(exit_status) => {
                assert_that!(exit_status).fail("process should still be running");
            }
            RunningState::Uncertain(_) => {
                assert_that!(&process).fail("process state should not be uncertain");
            }
        }

        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap();

        match process.is_running() {
            RunningState::Running => {
                assert_that!(process).fail("process should not be running anymore");
            }
            RunningState::Terminated(exit_status) => {
                assert_that!(exit_status.code()).is_some().is_equal_to(0);
                assert_that!(exit_status.success()).is_true();
            }
            RunningState::Uncertain(_) => {
                assert_that!(process).fail("process state should not be uncertain");
            }
        }
    }
}
