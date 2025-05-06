mod collector;
mod inspector;
mod output_stream;
mod panic_on_drop;
mod process_handle;
mod signal;
mod terminate_on_drop;

pub use collector::{Collector, CollectorError, Sink};
pub use inspector::{Inspector, InspectorError};
pub use output_stream::{OutputStream, OutputType, WaitError, WaitFor};
pub use process_handle::{IsRunning, ProcessHandle, TerminationError};
pub use terminate_on_drop::TerminateOnDrop;

#[cfg(test)]
mod test {
    use crate::output_stream::Next;
    use crate::{IsRunning, ProcessHandle};
    use assertr::prelude::*;
    use mockall::*;
    use std::fs::File;
    use std::io::Write;
    use std::time::Duration;
    use tokio::process::Command;

    #[tokio::test]
    async fn test() {
        let cmd = Command::new("ls");
        let mut process = ProcessHandle::spawn("ls", cmd).expect("Failed to spawn `ls` command");
        let (status, stdout, stderr) = process.wait_with_output().await.unwrap();
        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn running_to_completion() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1");
        let mut process =
            ProcessHandle::spawn("sleep", cmd).expect("Failed to spawn `sleep` command");

        process.wait().await.unwrap();

        match process.is_running() {
            IsRunning::Running => {
                assert_that(process).fail("Process should not be running anymore");
            }
            IsRunning::NotRunning(exit_status) => {
                assert_that(exit_status.code()).is_some().is_equal_to(0);
                assert_that(exit_status.success()).is_true();
            }
            IsRunning::Uncertain(_) => {
                assert_that(process).fail("Process state should not be uncertain");
            }
        };
    }

    #[tokio::test]
    async fn manual_termination() {
        let mut cmd = Command::new("sleep");
        cmd.arg("1000");
        let mut process =
            ProcessHandle::spawn("sleep", cmd).expect("Failed to spawn `sleep` command");
        process
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .unwrap();

        match process.is_running() {
            IsRunning::Running => {
                assert_that(process).fail("Process should not be running anymore");
            }
            IsRunning::NotRunning(exit_status) => {
                // Terminating a process with a signal results in no code being emitted (on linux).
                assert_that(exit_status.code()).is_none();
                assert_that(exit_status.success()).is_false();
            }
            IsRunning::Uncertain(_) => {
                assert_that(process).fail("Process state should not be uncertain");
            }
        };
    }

    #[tokio::test]
    async fn manually_await_with_sync_inspector() {
        let cmd = Command::new("ls");
        let mut process = ProcessHandle::spawn("ls", cmd).expect("Failed to spawn `ls` command");

        #[automock]
        trait FunctionCaller {
            fn call_function(&self, input: String);
        }

        let mut out_mock = MockFunctionCaller::new();
        out_mock
            .expect_call_function()
            .with(predicate::eq("Cargo.lock".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("Cargo.toml".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("LICENSE-APACHE".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("LICENSE-MIT".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("README.md".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("src".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("target".to_string()))
            .times(1)
            .return_const(());

        let err_mock = MockFunctionCaller::new();

        let out_inspector = process.stdout().inspect_lines(move |line| {
            out_mock.call_function(line);
            Next::Continue
        });
        let err_inspector = process.stderr().inspect_lines(move |line| {
            err_mock.call_function(line);
            Next::Continue
        });

        let status = process.wait().await.unwrap();
        let () = out_inspector.abort().await.unwrap();
        let () = err_inspector.abort().await.unwrap();

        assert_that(status.success()).is_true();
    }

    #[tokio::test]
    async fn manually_await_with_collectors() {
        let cmd = Command::new("ls");
        let mut exec = ProcessHandle::spawn("ls", cmd).expect("Failed to spawn `ls` command");

        let temp_file_out = tempfile::tempfile().unwrap();
        let temp_file_err = tempfile::tempfile().unwrap();

        let out_collector = exec
            .stdout()
            .collect_lines(temp_file_out, |line, temp_file| {
                writeln!(temp_file, "{}", line).unwrap();
                Next::Continue
            });
        let err_collector = exec
            .stderr()
            .collect_lines(temp_file_err, |line, temp_file| {
                writeln!(temp_file, "{}", line).unwrap();
                Next::Continue
            });

        let status = exec.wait().await.unwrap();
        let stdout = out_collector.abort().await.unwrap();
        let stderr = err_collector.abort().await.unwrap();

        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn manually_await_with_async_collectors() {
        let cmd = Command::new("ls");
        let mut exec = ProcessHandle::spawn("ls", cmd).expect("Failed to spawn `ls` command");

        let temp_file_out: File = tempfile::tempfile().unwrap();
        let temp_file_err: File = tempfile::tempfile().unwrap();

        let out_collector =
            exec.stdout()
                .collect_lines_async(temp_file_out, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(Next::Continue)
                    })
                });
        let err_collector =
            exec.stderr()
                .collect_lines_async(temp_file_err, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(Next::Continue)
                    })
                });

        let status = exec.wait().await.unwrap();
        let stdout = out_collector.abort().await.unwrap();
        let stderr = err_collector.abort().await.unwrap();

        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }
}
