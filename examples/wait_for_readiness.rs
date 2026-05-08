#![allow(clippy::wildcard_imports)]
//! Start a service and wait until it logs that it is ready.
//!
//! `wait_for_line` is itself a stream consumer with a required timeout, so under
//! `single_subscriber()` it would compete with anything else reading the stream. Reach for
//! `broadcast()` whenever readiness detection has to coexist with a logger. Pair it with
//! replay retention so a waiter attached after spawn can still observe startup lines, then
//! `seal_output_replay()` to free that history once the service is up.
//!
//! For a runnable demonstration, this example spawns a `sh` pipeline that simulates a web
//! server: it sleeps briefly, prints the readiness line, then sleeps for the rest of its
//! life so we can exercise the cleanup path. Swap the command for your real binary in
//! production; the rest of the structure is unchanged.
//!
//! Run: `cargo run --example wait_for_readiness` (Unix / macOS; on Windows, swap `sh` for
//! a comparable shell pipeline).

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;
use unwrap_infallible::UnwrapInfallible;

#[tokio::main]
async fn main() {
    let mut server = Command::new("sh");
    server
        .arg("-c")
        .arg("sleep 1; echo 'Server listening on :8080'; sleep 60");

    let mut process = Process::new(server)
        .name("api")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn server (need `sh` on PATH)");

    let stdout = process.stdout();

    let _logs = stdout
        .consume(ParseLines::inspect(LineParsingOptions::default(), |line| {
            eprintln!("{line}");
            Next::Continue
        }))
        .unwrap_infallible();

    match stdout
        .wait_for_line(
            Duration::from_secs(30),
            |line| line.contains("Server listening on"),
            LineParsingOptions::default(),
        )
        .await
    {
        Ok(WaitForLineResult::Matched) => {}
        Ok(WaitForLineResult::StreamClosed) => panic!("server exited before becoming ready"),
        Ok(WaitForLineResult::Timeout) => panic!("server did not become ready in time"),
        Err(err) => panic!("failed to read stdout: {err}"),
    }

    process.seal_output_replay();

    let _ = process
        .terminate(
            GracefulShutdown::builder()
                .unix_sigterm(Duration::from_secs(5))
                .windows_ctrl_break(Duration::from_secs(8))
                .build(),
        )
        .await;
}
