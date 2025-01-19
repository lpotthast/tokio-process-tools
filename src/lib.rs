mod collector;
mod inspector;
mod output_stream;
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
    use crate::ProcessHandle;
    use mockall::*;
    use std::fs::File;
    use std::io::Write;
    use std::process::Stdio;
    use tokio::process::Command;

    #[tokio::test]
    async fn test() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);
        let (status, stdout, stderr) = exec.wait_with_output().await.unwrap();
        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn manually_await_with_sync_inspector() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

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
        let out_callback = move |input: String| {
            out_mock.call_function(input);
        };

        let err_mock = MockFunctionCaller::new();
        let err_callback = move |input: String| {
            err_mock.call_function(input);
        };

        let out_inspector = exec.stdout().inspect(move |line| {
            out_callback(line);
        });
        let err_inspector = exec.stderr().inspect(move |line| {
            err_callback(line);
        });

        let status = exec.wait().await.unwrap();
        let () = out_inspector.abort().await.unwrap();
        let () = err_inspector.abort().await.unwrap();

        println!("{:?}", status);
    }

    #[tokio::test]
    async fn manually_await_with_collectors() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

        let temp_file_out = tempfile::tempfile().unwrap();
        let temp_file_err = tempfile::tempfile().unwrap();

        let out_collector = exec.stdout().collect(temp_file_out, |line, temp_file| {
            writeln!(temp_file, "{}", line).unwrap();
        });
        let err_collector = exec.stderr().collect(temp_file_err, |line, temp_file| {
            writeln!(temp_file, "{}", line).unwrap();
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
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

        let temp_file_out: File = tempfile::tempfile().unwrap();
        let temp_file_err: File = tempfile::tempfile().unwrap();

        let out_collector =
            exec.stdout()
                .collect_async(temp_file_out, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(())
                    })
                });
        let err_collector =
            exec.stderr()
                .collect_async(temp_file_err, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(())
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
