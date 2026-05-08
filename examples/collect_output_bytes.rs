#![allow(clippy::wildcard_imports)]
//! Same as `wait_and_collect_lines`, but for output that is not UTF-8 line-oriented.
//!
//! Reach for `with_raw_output(...)` and `RawOutputOptions` when the child produces binary
//! blobs (image conversion, archive tools), framed protocols, or anything where line parsing
//! would corrupt or lose bytes. The shape of the chain is otherwise identical.
//!
//! Run: `cargo run --example wait_and_collect_raw_bytes`

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

    let raw_collection_options = RawCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };

    let ProcessOutput {
        status,
        stdout,
        stderr,
    } = process
        .wait_for_completion(Duration::from_secs(30))
        .with_raw_output(
            DEFAULT_OUTPUT_EOF_TIMEOUT,
            RawOutputOptions::symmetric(raw_collection_options),
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("exit status: {status:?}");
    println!(
        "stdout: {} bytes (truncated: {})",
        stdout.bytes.len(),
        stdout.truncated
    );
    println!(
        "stderr: {} bytes (truncated: {})",
        stderr.bytes.len(),
        stderr.truncated
    );
}
