#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::io;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_process_tools::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt, Process, WaitError,
    WaitForCompletionOrTerminateResult, WaitOrTerminateError,
};
use unwrap_infallible::UnwrapInfallible;

#[tokio::test]
async fn wait_for_completion_disarms_cleanup_and_panic_guards() {
    let mut process = Process::new(long_running_command(Duration::from_millis(100)))
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

    process
        .wait_for_completion(Duration::from_secs(2))
        .await
        .unwrap()
        .expect_completed("process should complete");

    assert_that!(process.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn wait_for_completion_closes_stdin_before_waiting() {
    let cmd = tokio::process::Command::new("cat");
    let mut process = Process::new(cmd)
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

    let collector = collect_lines_into_vec(process.stdout()).unwrap_infallible();

    let Some(stdin) = process.stdin().as_mut() else {
        assert_that!(process.stdin().is_open()).fail("stdin should start open");
        return;
    };
    stdin.write_all(b"wait closes stdin\n").await.unwrap();
    stdin.flush().await.unwrap();

    let status = process
        .wait_for_completion(Duration::from_secs(2))
        .await
        .unwrap()
        .expect_completed("process should complete");

    assert_that!(status.success()).is_true();
    assert_that!(process.stdin().is_open()).is_false();

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().len()).is_equal_to(1);
    assert_that!(collected[0].as_str()).is_equal_to("wait closes stdin");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn or_terminate_returns_wait_failure_after_cleanup() {
    let mut process = Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let wait_error = WaitError::IoError {
        process_name: "long-running".into(),
        source: io::Error::other("synthetic wait failure"),
    };

    let result = process
        .terminate_after_wait_error(wait_error, default_graceful_shutdown())
        .await;

    assert_that!(process.is_drop_disarmed()).is_true();

    let (wait_error, termination_status) = match result {
        Err(WaitOrTerminateError::WaitFailed {
            wait_error,
            termination_status,
            ..
        }) => (wait_error, termination_status),
        other => {
            assert_that!(&other).fail(format_args!(
                "expected wait failure preserved after successful cleanup, got {other:?}"
            ));
            return;
        }
    };

    assert_that!(termination_status.code()).is_none();

    let WaitError::IoError { source, .. } = *wait_error else {
        assert_that!(()).fail("expected WaitError::IoError variant");
        return;
    };
    assert_that!(source.to_string().as_str()).is_equal_to("synthetic wait failure");
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn wait_for_completion_or_terminate_terminates_after_timeout() {
    let mut process = Process::new(long_running_command(Duration::from_secs(5)))
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

    let result = process
        .wait_for_completion(Duration::from_millis(10))
        .or_terminate(default_graceful_shutdown())
        .await
        .unwrap();
    let status = match result {
        WaitForCompletionOrTerminateResult::TerminatedAfterTimeout { result, timeout } => {
            assert_that!(timeout).is_equal_to(Duration::from_millis(10));
            result
        }
        other @ WaitForCompletionOrTerminateResult::Completed(_) => {
            assert_that!(&other).fail(format_args!(
                "expected timeout-driven termination, got {other:?}"
            ));
            return;
        }
    };

    assert_that!(status.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}
