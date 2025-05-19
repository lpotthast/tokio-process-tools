# tokio-process-tools

A library designed to assist with interacting and managing processes spawned via the `tokio` runtime.

## Features

- Inspecting stdout/stderr streams synchronously and asynchronously.
- Collecting stdout/stderr streams synchronously and asynchronously into some new structure.
- Waiting for specific output on stdout/stderr.
- Gracefully terminating a process or killing it if unresponsive.

## Example Usage

Here is an example that demonstrates how to spawn a process, handle its output, and gracefully terminate it using this
library.

```rust
use crate::broadcast::BroadcastOutputStream;
use crate::output_stream::Next;
use crate::ProcessHandle;

async fn start_process() {
    // Create a command.
    let cmd = tokio::process::Command::new("some-long-running-process");

    // Let tokio_process_tools spawn the command. It will automatically set up output capturing.
    // Use either:
    // - SingleSubscriberOutputStream: If you expect only a single consumer per stream.
    // - BroadcastOutputStream: If you expect to have multiple concurrent consumers per stream.
    let mut process =
        ProcessHandle::<BroadcastOutputStream>::spawn("my-process-handle", cmd).unwrap();

    // Access the stdout/stderr streams with their respective accessor functions.
    let stdout_inspector = process.stdout().inspect_lines(
        |line| {
            tracing::info!(line, "stdout");
            Next::Continue
        },
        LineParsingOptions::default()
    );
    let stderr_inspector = process.stderr().inspect_lines_async(
        async |line| {
            tracing::warn!(line, "stderr");
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // You can wait for a line fulfilling an assertion. Optionally with a timeout.
    // This is syntactic sugar over an `inspect_lines`.
    process
        .stdout()
        .wait_for_line(
            |line| line.contains("started successfully on port 8080"),
            LineParsingOptions::default()
        )
        .await;

    // Wait for the natural completion of the process (with an optional timeout).
    process
        .wait_for_completion(Some(std::time::Duration::from_secs(30)))
        .await
        .unwrap();

    // Or explicitly terminate the process.
    process
        .terminate(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(8),
        )
        .await
        .unwrap();

    // Or combine both.
    process
        .wait_for_completion_or_terminate(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(8),
        )
        .await
        .unwrap();
}
```

## Installation

Add the following line to your `Cargo.toml` to include this library as part of your project:

```toml
[dependencies]
tokio-process-tools = "0.5.0"
```

Ensure that the `tokio` runtime is also set up in your project. Only use the features you need!:

```toml
[dependencies]
tokio = { version = "1.45.0", features = ["full"] }
```

**IMPORTANT**:

This libraries `TerminateOnDrop` type requires your code to run in a multithreaded runtime! Dropping a
`TerminateOnDrop` in a single-threaded runtime will lead to a panic.

This also holds for unit tests! Annotate your tokio-enabled tests dealing with a `TerminateOnDrop` with

```rust
#[tokio::test(flavor = "multi_thread")]
async fn test() {}
```

to not run into problems.

## Contributing

We welcome contributions! Please fork the repository, create a feature branch, and send a pull request. Make sure your
code follows Rust’s idiomatic practices (`cargo fmt` and `cargo clippy` checks) and includes relevant tests.
