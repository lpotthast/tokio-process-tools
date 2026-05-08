#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::time::Duration;
use tokio_process_tools::{
    AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt, Process,
};

#[tokio::test]
async fn process_handle_seal_output_replay_seals_stdout_and_stderr() {
    let process = Process::new(long_running_command(Duration::from_millis(100)))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

    assert_that!(process.stdout().is_replay_sealed()).is_false();
    assert_that!(process.stderr().is_replay_sealed()).is_false();

    process.seal_output_replay();

    assert_that!(process.stdout().is_replay_sealed()).is_true();
    assert_that!(process.stderr().is_replay_sealed()).is_true();

    let mut process = process;
    let _ = process
        .wait_for_completion(Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn seal_output_replay_seals_broadcast_stdout_and_single_subscriber_stderr() {
    let process = Process::new(long_running_command(Duration::from_millis(100)))
        .name(AutoName::program_only())
        .stdout(|stdout| {
            stdout
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(|stderr| {
            stderr
                .single_subscriber()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

    assert_that!(process.stdout().is_replay_sealed()).is_false();
    assert_that!(process.stderr().is_replay_sealed()).is_false();

    process.seal_output_replay();

    assert_that!(process.stdout().is_replay_sealed()).is_true();
    assert_that!(process.stderr().is_replay_sealed()).is_true();

    let mut process = process;
    let _ = process
        .wait_for_completion(Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn seal_output_replay_seals_single_subscriber_stdout_and_broadcast_stderr() {
    let process = Process::new(long_running_command(Duration::from_millis(100)))
        .name(AutoName::program_only())
        .stdout(|stdout| {
            stdout
                .single_subscriber()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(|stderr| {
            stderr
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

    assert_that!(process.stdout().is_replay_sealed()).is_false();
    assert_that!(process.stderr().is_replay_sealed()).is_false();

    process.seal_output_replay();

    assert_that!(process.stdout().is_replay_sealed()).is_true();
    assert_that!(process.stderr().is_replay_sealed()).is_true();

    let mut process = process;
    let _ = process
        .wait_for_completion(Duration::from_secs(2))
        .await
        .unwrap();
}
