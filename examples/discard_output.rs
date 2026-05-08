#![allow(clippy::wildcard_imports)]
//! Discard one or both of a child's output streams at the OS level.
//!
//! `stream.discard()` routes a slot to `/dev/null` (or its platform equivalent), so no pipe
//! is allocated and no reader task runs in the parent. Reach for it when you do not need
//! the output at all, or only need one of the two streams.
//!
//! A discarded stream still implements [`OutputStream`], [`Subscribable`], and
//! [`Consumable`], so generic helpers can target any backend uniformly. Its subscription
//! emits a single EOF event and then closes: any visitor consumed against a discarded
//! stream observes zero chunks and terminates immediately. The `.with_line_output(...)`
//! and `.with_raw_output(...)` stages on the `wait_for_completion(...)` builder are not in
//! scope on a handle whose stream is discarded; consume the captured slot manually before
//! waiting, as shown below.
//!
//! For the simpler symmetric "discard everything, only the exit status matters" case, use
//! `stdout_and_stderr(ProcessStreamBuilder::discard)`.
//!
//! Run: `cargo run --example discard_output`
//!
//! Note: this example uses `sh -c` so it runs on Unix and macOS. On Windows, swap the
//! command for one that writes to both stdout and stderr.

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut writes = Command::new("sh");
    writes.arg("-c").arg("echo on-stdout; echo on-stderr >&2");

    let mut process = Process::new(writes)
        .name(AutoName::program_only())
        .stdout(|stream| {
            stream
                .single_subscriber()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(ProcessStreamBuilder::discard)
        .spawn()
        .expect("failed to spawn command");

    let _logs = process
        .stdout()
        .consume(ParseLines::inspect(LineParsingOptions::default(), |line| {
            println!("stdout: {line}");
            Next::Continue
        }))
        .expect("no other consumer is attached yet");

    let status = process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("exit status: {status:?}");
    // stderr was routed to /dev/null by the kernel; the parent never saw "on-stderr".
}
