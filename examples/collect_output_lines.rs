#![allow(clippy::wildcard_imports)]
//! Spawn a child, wait for it to exit, and collect its stdout/stderr as parsed lines.
//!
//! This is the canonical "spawn + wait + collect" pattern. The wait timeout bounds how long
//! the process is allowed to run; `eof_timeout` is a separate budget for stdout/stderr to
//! observe EOF and drain after the process has exited.
//!
//! The stream is configured with `replay_last_bytes(...)` because the collector is attached
//! by the wait helper *after* the child has already started running. Without replay,
//! anything the child wrote before the helper attaches would be lost.
//!
//! Run: `cargo run --example wait_and_collect_lines`

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let line_collection_options = LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };

    let ProcessOutput {
        status,
        stdout,
        stderr,
    } = process
        .wait_for_completion(Duration::from_secs(30))
        .with_line_output(
            DEFAULT_OUTPUT_EOF_TIMEOUT,
            LineParsingOptions::default(),
            LineOutputOptions::symmetric(line_collection_options),
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("exit status: {status:?}");
    println!("stdout lines: {:?}", stdout.lines());
    println!("stderr lines: {:?}", stderr.lines());
}
