mod collector;
mod inspector;
mod output_stream;
mod panic_on_drop;
mod process_handle;
mod signal;
mod terminate_on_drop;

pub use collector::{Collector, CollectorError, Sink};
pub use inspector::{Inspector, InspectorError};
pub use output_stream::{
    LineOverflowBehavior, LineParsingOptions, Next, NumBytes, NumBytesExt, OutputStream, broadcast,
    single_subscriber,
};
pub use process_handle::{ProcessHandle, RunningState, TerminationError};
pub use terminate_on_drop::TerminateOnDrop;

#[cfg(test)]
mod test {
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::{LineParsingOptions, ProcessHandle, RunningState};
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::process::Command;

    #[tokio::test]
    async fn wait_with_output() {
        let cmd = Command::new("ls");
        let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("ls", cmd)
            .expect("Failed to spawn `ls` command");
        let (status, stdout, stderr) = process
            .wait_with_output(LineParsingOptions::default())
            .await
            .unwrap();
        assert_that(status.success()).is_true();
        assert_that(stdout).is_equal_to(&[
            "Cargo.lock",
            "Cargo.toml",
            "LICENSE-APACHE",
            "LICENSE-MIT",
            "README.md",
            "src",
            "target",
        ]);
        assert_that(stderr).is_empty();
    }

    #[tokio::test]
    async fn is_running() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1");
        let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("sleep", cmd)
            .expect("Failed to spawn `sleep` command");

        match process.is_running() {
            RunningState::Running => {}
            RunningState::Terminated(exit_status) => {
                assert_that(exit_status).fail("Process should be running");
            }
            RunningState::Uncertain(_) => {
                assert_that_ref(&process).fail("Process state should not be uncertain");
            }
        };

        let _exit_status = process.wait_for_completion(None).await.unwrap();

        match process.is_running() {
            RunningState::Running => {
                assert_that(process).fail("Process should not be running anymore");
            }
            RunningState::Terminated(exit_status) => {
                assert_that(exit_status.code()).is_some().is_equal_to(0);
                assert_that(exit_status.success()).is_true();
            }
            RunningState::Uncertain(_) => {
                assert_that(process).fail("Process state should not be uncertain");
            }
        };
    }

    #[tokio::test]
    async fn terminate() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1000");
        let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("sleep", cmd)
            .expect("Failed to spawn `sleep` command");
        process
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();
        match process.is_running() {
            RunningState::Running => {
                assert_that(process).fail("Process should not be running anymore");
            }
            RunningState::Terminated(exit_status) => {
                // Terminating a process with a signal results in no code being emitted (on linux).
                assert_that(exit_status.code()).is_none();
                assert_that(exit_status.success()).is_false();
            }
            RunningState::Uncertain(_) => {
                assert_that(process).fail("Process state should not be uncertain");
            }
        };
    }
}
