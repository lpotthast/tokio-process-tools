#![allow(clippy::wildcard_imports)]
//! Write to a child's stdin, close it to signal EOF, and collect the echoed output.
//!
//! Children always have stdin piped (there is no opt-out); the library only flushes/closes
//! the pipe when you ask. Calling `stdin().close()` lets a child observe EOF before any
//! wait helper runs (some programs only finish on EOF, not on idle). Terminal waits and
//! `kill()` close any still-open stdin automatically, so the manual close is only needed
//! when the child must see EOF *before* the wait.
//!
//! `process.stdin()` returns a `&mut Stdin`, an enum with `Open(ChildStdin)` / `Closed`
//! variants. `Stdin::as_mut()` projects that into `Option<&mut ChildStdin>`, returning
//! `None` once the slot has transitioned to `Closed` (either explicitly via
//! `stdin().close()` or implicitly when a wait helper or `kill()` cleaned it up). The
//! example below pattern-matches with `if let Some(stdin) = process.stdin().as_mut()` for
//! that reason.
//!
//! Run: `cargo run --example interactive_stdin`

use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let line_collection = LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };

    let mut process = Process::new(Command::new("cat"))
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

    if let Some(stdin) = process.stdin().as_mut() {
        stdin.write_all(b"hello\n").await.unwrap();
        stdin.write_all(b"world\n").await.unwrap();
        stdin.flush().await.unwrap();
    }
    process.stdin().close();

    let output = process
        .wait_for_completion(Duration::from_secs(30))
        .with_line_output(
            DEFAULT_OUTPUT_EOF_TIMEOUT,
            LineParsingOptions::default(),
            LineOutputOptions::symmetric(line_collection),
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("stdout lines: {:?}", output.stdout.lines());
}
