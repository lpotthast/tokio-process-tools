#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;

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
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio_process_tools::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process};

    let temp_dir = tempdir().unwrap();
    let output_file = temp_dir.path().join("stdin-result.txt");
    let output_file = output_file.to_str().unwrap().replace('\'', "'\"'\"'");

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("cat >/dev/null; printf eof > '{output_file}'"));

    let mut process = Process::new(cmd)
        .name("sh")
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
    use std::time::Duration;
    use tokio_process_tools::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process};

    let mut cmd = tokio::process::Command::new("yes");
    cmd.arg("tick");

    let mut process = Process::new(cmd)
        .name("yes")
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
