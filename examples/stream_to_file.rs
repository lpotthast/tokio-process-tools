#![allow(clippy::wildcard_imports)]
//! Stream a child's stdout to an `AsyncWrite` sink (here, a file on disk).
//!
//! Reach for this when the child will produce more output than is reasonable to hold in
//! memory: build tools, long-running servers in CI logs, anything you want on disk rather
//! than in `Vec<String>`. Parsed lines are written one per call;
//! [`LineWriteMode::AppendLf`] re-attaches the trailing newline that line parsing strips.
//! [`WriteCollectionOptions::log_and_continue`] keeps collection running if the file write
//! ever fails (disk full, broken pipe to a piped writer, etc.); use `fail_fast()` instead
//! when a sink failure should stop the collection.
//!
//! After the child exits the stream closes, the visitor drains, and `Consumer::wait`
//! returns the file handle so you can flush or close it deterministically.
//!
//! This example needs the `fs` Tokio feature (already enabled in dev-dependencies).
//!
//! Run: `cargo run --example stream_to_file`

use std::time::Duration;
use tokio::fs::File;
use tokio::process::Command;
use tokio_process_tools::visitors::write::WriteLines;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_with_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let log_path = std::env::temp_dir().join("tokio-process-tools-stream.log");
    let log_file = File::create(&log_path)
        .await
        .expect("failed to open log file");

    let log_collector = process
        .stdout()
        .consume_async(ParseLines::new(
            LineParsingOptions::default(),
            WriteLines::passthrough(
                "stdout",
                log_file,
                WriteCollectionOptions::log_and_continue(),
                LineWriteMode::AppendLf,
            ),
        ))
        .expect("no other consumer is attached yet");

    let _status = process
        .wait_for_completion(Duration::from_secs(60))
        .await
        .unwrap()
        .expect_completed("process should complete");

    // The outer `Result` reflects consumer-infrastructure outcomes (task join, stream
    // read); the inner one reflects whether the writer accepted every byte. With
    // `log_and_continue()`, write failures are logged so the inner result is `Ok` here.
    let _log_file = log_collector.wait().await.unwrap().unwrap();

    println!("wrote stdout to {}", log_path.display());
}
