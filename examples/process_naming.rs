#![allow(clippy::wildcard_imports)]
//! Demonstrate the [`ProcessName`] / [`AutoName`] / [`AutoNameSettings`] variants.
//!
//! Naming is required before stdout/stderr can be configured: the name appears in error
//! messages and tracing spans, so the library refuses to spawn anonymous processes.
//!
//! Prefer an explicit label (`"api-worker"`, `format!("agent-{id}")`) in production. The
//! `AutoName` variants are convenient for ad-hoc and test code, but any variant that
//! includes args, env, or cwd will surface those values in error messages; reach for them
//! only when the underlying values are safe to log.
//!
//! Run: `cargo run --example process_naming`

use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let id = 42;

    // Recommended: an explicit label that groups cleanly in logs / metrics.
    let mut process = Process::new(Command::new("true"))
        .name(format!("agent-{id}"))
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");

    // Other naming options:
    //
    //   .name(AutoName::program_only())              // just the program name
    //   .name(AutoName::program_with_args())         // program + args (may leak args)
    //   .name(AutoName::program_with_env_and_args()) // also includes env
    //   .name(AutoName::full())                      // cwd + env + program + args
    //   .name(AutoName::Debug)                       // full Command Debug repr
    //   .name(AutoNameSettings::builder() /* opt into specific fields */ .build())
    //
    // Check the redaction story before reaching for the variants that surface args, env,
    // or cwd in production logs.
}
