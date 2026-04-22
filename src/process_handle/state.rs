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
