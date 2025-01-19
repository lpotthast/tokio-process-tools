# tokio-process-tools

A library designed to assist with interacting and managing processes spawned via the `tokio` runtime.

## Features

- Inspecting stdout/stderr streams asynchronously.
- Collecting stdout/stderr streams asynchronously into some new structure.
- Waiting for specific output on stdout/stderr.
- Gracefully terminating a processes or killing it if unresponsive.

## Example Usage

Here is an example that demonstrates how to spawn a process, handle its output, and gracefully terminate it using this
library.

```rust
async fn start_server() -> tokio_process_tools::TerminateOnDrop {
    let child = tokio::process::Command::new("some-long-running-process")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let handle = tokio_process_tools::ProcessHandle::new_from_child_with_piped_io(
        "my-process-handle",
        child,
    );

    let _out_inspector = handle.stdout().inspect(|stdout_line| {
        tracing::debug!(stdout_line, "some-long-running-process wrote");
    });
    let _err_inspector = handle.stdout().inspect(|stderr_line| {
        tracing::debug!(stderr_line, "some-long-running-process wrote");
    });

    handle
        .stdout()
        .wait_for(|line| line.contains("started successfully on port 8080"))
        .await;

    handle.terminate_on_drop(
        Some(std::time::Duration::from_secs(10)),
        Some(std::time::Duration::from_secs(10)),
    )
}
```

## Installation

Add the following line to your `Cargo.toml` to include this library as part of your project:

```toml
[dependencies]
tokio-process-tools = "0.1.0"
```

Ensure that the `tokio` runtime is also set up in your project. Only use the features you need!:

```toml
[dependencies]
tokio = { version = "1.43.0", features = ["full"] }
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
