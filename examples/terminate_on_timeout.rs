#![allow(clippy::wildcard_imports)]
//! Wait with a timeout, and force a graceful cleanup when the wait times out.
//!
//! For processes that may hang, chain `.or_terminate(shutdown)` onto a
//! `wait_for_completion(...)` builder. The configured graceful shutdown policy runs as part
//! of the wait, so you do not have to sequence cleanup yourself. The wait timeout bounds
//! the normal wait; `shutdown` controls which graceful signal sequence runs before the
//! forceful kill fallback.
//!
//! Run: `cargo run --example timeout_and_cleanup`. As written, the child sleeps for 60
//! seconds and the wait gives up after 2 seconds, so the example exercises the
//! `TerminatedAfterTimeout` branch.

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut sleep = Command::new("sleep");
    sleep.arg("60");

    let mut process = Process::new(sleep)
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
        .expect("failed to spawn command");

    match process
        .wait_for_completion(Duration::from_secs(2))
        .or_terminate(
            GracefulShutdown::builder()
                .unix_sigterm(Duration::from_secs(5))
                .windows_ctrl_break(Duration::from_secs(8))
                .build(),
        )
        .await
    {
        Ok(WaitForCompletionOrTerminateResult::Completed(status)) => {
            println!("completed with status: {status:?}");
        }
        Ok(WaitForCompletionOrTerminateResult::TerminatedAfterTimeout { result, timeout }) => {
            println!("terminated after waiting {timeout:?}; status: {result:?}");
        }
        Err(err) => eprintln!("wait or cleanup failed: {err}"),
    }
}
