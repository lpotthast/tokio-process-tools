#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::process::Stdio;
use std::time::Duration;
use tokio_process_tools::{
    AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process, RunningState, Stdin,
};

mod stdin {
    use super::*;

    #[tokio::test]
    async fn open_stdin_reports_open_until_closed() {
        let mut child = long_running_command(Duration::from_secs(5))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let child_stdin = child.stdin.take().unwrap();
        let mut stdin = Stdin::Open(child_stdin);

        assert_that!(stdin.is_open()).is_true();
        assert_that!(stdin.as_mut().is_some()).is_true();

        stdin.close();

        assert_that!(stdin.is_open()).is_false();
        assert_that!(stdin.as_mut()).is_none();

        child.kill().await.unwrap();
    }
}

mod is_running {
    use super::*;

    #[tokio::test]
    async fn does_not_disarm_drop_guards_when_process_has_exited() {
        let mut process = Process::new(long_running_command(Duration::from_millis(50)))
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Observe the exit through the query API. Even with the child reaped, the drop
        // guards must remain armed because is_running is documented as a pure status query.
        let _state = process.is_running();

        assert_that!(process.is_drop_armed()).is_true();

        // Detach explicitly so the test does not panic when `process` drops.
        process.must_not_be_terminated();
    }

    #[tokio::test]
    async fn reports_running_before_wait_and_terminated_after_wait() {
        let mut process = Process::new(long_running_command(Duration::from_secs(1)))
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
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
            .unwrap()
            .expect_completed("process should have exited within the wait timeout");

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

mod into_inner {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use unwrap_infallible::UnwrapInfallible;

    #[tokio::test]
    async fn returns_stdin_with_pipe_still_open() {
        let cmd = tokio::process::Command::new("cat");
        let process = Process::new(cmd)
            .name("cat")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, mut stdin, stdout, _stderr) = process.into_inner();
        assert_that!(child.stdin.is_none()).is_true();

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_that!(child.try_wait().unwrap().is_none()).is_true();

        let collector = collect_lines_into_vec(&stdout).unwrap_infallible();

        let Some(stdin_handle) = stdin.as_mut() else {
            assert_that!(stdin.is_open()).fail("stdin should be returned open");
            return;
        };
        stdin_handle
            .write_all(b"stdin stayed open\n")
            .await
            .unwrap();
        stdin_handle.flush().await.unwrap();

        stdin.close();

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_that!(status.success()).is_true();

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines().len()).is_equal_to(1);
        assert_that!(collected[0].as_str()).is_equal_to("stdin stayed open");
    }

    #[tokio::test]
    async fn defuses_panic_guard() {
        let process = Process::new(long_running_command(Duration::from_secs(5)))
            .name("long-running")
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn supports_handles_built_with_owned_name() {
        let process = Process::new(long_running_command(Duration::from_secs(5)))
            .name(format!("sleeper-{}", 7))
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let (mut child, _stdin, _stdout, _stderr) = process.into_inner();
        child.kill().await.unwrap();
        let _status = child.wait().await.unwrap();
    }
}
