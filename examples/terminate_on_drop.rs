#![allow(clippy::wildcard_imports)]
//! Opt out of the panic-on-drop guard with `terminate_on_drop`.
//!
//! A live `ProcessHandle` that is dropped without a successful terminal wait, `terminate()`,
//! or `kill()` runs best-effort cleanup and then **panics**. The panic is deliberate: a
//! silent leak of a child process is much harder to detect than a loud panic in tests, and
//! most "I forgot to wait" bugs would otherwise survive into production.
//!
//! For long-lived processes that follow a service's own lifecycle, opting in to
//! `.terminate_on_drop(...)` swaps the panic for an async termination attempt during drop.
//! The signature mirrors `terminate(...)`: a `GracefulShutdown` carrying a
//! `UnixGracefulShutdown` on Unix and a `WindowsGracefulShutdown` on Windows.
//!
//! `terminate_on_drop` requires a multithreaded Tokio runtime, because drop runs the
//! termination attempt on Tokio's blocking pool. If the attempt itself fails, the inner
//! handle still falls back to the panic path so the failure is not silent. Under
//! `#[tokio::test]`, opt in to the multi-thread flavor:
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread")]
//! async fn test() { /* ... */ }
//! ```
//!
//! When a child genuinely needs to outlive the handle (e.g. a daemon you spawned
//! intentionally), call `must_not_be_terminated()` instead, which suppresses both the kill
//! and the panic on drop. To keep the child *and* its `Stdin`/`Stdout`/`Stderr` together,
//! use `ProcessHandle::into_inner()` to take ownership of all four.
//!
//! Run: `cargo run --example terminate_on_drop`

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut sleep = Command::new("sleep");
    sleep.arg("60");

    let _process = Process::new(sleep)
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
        .unwrap()
        .terminate_on_drop(
            GracefulShutdown::builder()
                .unix_sigterm(Duration::from_secs(5))
                .windows_ctrl_break(Duration::from_secs(8))
                .build(),
        );

    // Do some work while the child is alive...
    tokio::time::sleep(Duration::from_millis(100)).await;

    // When `_process` is dropped here, the configured graceful shutdown runs in the
    // background. No panic, no leak.
}
