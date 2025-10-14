# tokio-process-tools

A powerful library for spawning and managing processes in the Tokio runtime with advanced output handling capabilities.

When working with child processes in async Rust, you often need to:

- **Monitor output in real-time** without blocking
- **Wait for specific log messages** before proceeding
- **Gracefully terminate** processes with proper signal handling
- **Collect output** for later analysis
- **Handle multiple concurrent consumers** of the same stream
- **Prevent spawned processes from leaking**, not being terminated properly

`tokio-process-tools` tries to make all of this simple and ergonomic.

## Features

- ✨ **Real-time Output Inspection** - Monitor stdout/stderr as they arrive, with sync and async callbacks
- 🔍 **Pattern Matching** - Wait for specific output before continuing execution
- 🎯 **Flexible Collection** - Gather output into vectors, files, or custom sinks
- 🔄 **Multiple Consumers** - Support for both single and broadcast (multi-consumer) stream consumption
- ⚡ **Graceful Termination** - Automatic signal escalation (SIGINT → SIGTERM → SIGKILL)
- 🛡️ **Collection Safety** - Fine-grained control over buffers used when collecting stdout/stderr streams
- 🛡️ **Resource Safety** - Panic-on-drop guards ensure processes are properly cleaned up
- ⏱️ **Timeout Support** - Built-in timeout handling for all operations
- 🌊 **Backpressure Control** - Configurable behavior when consumers can't keep up

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.6"
tokio = { version = "1", features = ["process", "sync", "io-util", "rt-multi-thread", "time"] }
```

## Examples

- When you only need one consumer per stdout/stderr stream, use `SingleSubscriberOutputStream`.
- When you need multiple consumer per stdout/stderr stream, use `BroadcastOutputStream`.

### Basic: Spawn and Collect Output

```rust ,no_run
use tokio_process_tools::single_subscriber::SingleSubscriberOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("ls");
    let mut process = ProcessHandle::<SingleSubscriberOutputStream>::spawn("ls", cmd)
        .expect("Failed to spawn command");

    // Collect all output
    let Output { status, stdout, stderr } = process
        .wait_for_completion_with_output(None, LineParsingOptions::default())
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
    println!("Output: {:?}", stdout);
}
```

### Monitor Output in Real-Time

```rust ,no_run
use tokio_process_tools::single_subscriber::SingleSubscriberOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("tail");
    cmd.arg("-f").arg("/var/log/app.log");

    let mut process = ProcessHandle::<SingleSubscriberOutputStream>::spawn("tail", cmd).unwrap();

    // Inspect output in real-time
    let _stdout_monitor = process.stdout_mut().inspect_lines(
        |line| {
            println!("stdout: {line}");
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // Let it run for a while
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Gracefully terminate
    process.terminate(
        Duration::from_secs(3),  // SIGINT timeout
        Duration::from_secs(5),  // SIGTERM timeout
    ).await.unwrap();
}
```

### Wait for Specific Output

Perfect for integration tests or ensuring services are ready:

```rust ,no_run
use tokio_process_tools::single_subscriber::SingleSubscriberOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("my-web-server");
    let mut process = ProcessHandle::<SingleSubscriberOutputStream>::spawn("server", cmd).unwrap();

    // Wait for the server to be ready
    match process.stdout_mut().wait_for_line_with_timeout(
        |line| line.contains("Server listening on"),
        LineParsingOptions::default(),
        Duration::from_secs(30),
    ).await {
        Ok(_) => println!("Server is ready!"),
        Err(_) => panic!("Server failed to start in time"),
    }

    // Now safe to make requests to the server
    // ...

    // Cleanup
    process.wait_for_completion_or_terminate(
        Duration::from_secs(5),   // Wait timeout
        Duration::from_secs(3),   // SIGINT timeout
        Duration::from_secs(5),   // SIGTERM timeout
    ).await.unwrap();
}
```

### Multiple Consumers (using BroadcastOutputStream)

```rust ,no_run
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("long-running-process");
    let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("process", cmd).unwrap();

    // Consumer 1: Log to console
    let _logger = process.stdout().inspect_lines(
        |line| {
            eprintln!("[LOG] {}", line);
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // Consumer 2: Collect to file
    let log_file = tokio::fs::File::create("output.log").await.unwrap();
    let _file_writer = process.stdout().collect_lines_into_write(
        log_file,
        LineParsingOptions::default()
    );

    // Consumer 3: Search for errors
    let error_collector = process.stdout().collect_lines(
        Vec::new(),
        |line, vec| {
            if line.contains("ERROR") {
                vec.push(line);
            }
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // Wait for completion
    process.wait_for_completion(None).await.unwrap();

    // Get collected errors
    let errors = error_collector.wait().await.unwrap();
    println!("Found {} errors", errors.len());
}
```

### Single Consumer (More Efficient)

When you only need one consumer per stream, use `SingleSubscriberOutputStream`:

```rust ,no_run
use tokio_process_tools::single_subscriber::SingleSubscriberOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("build");

    let mut process = ProcessHandle::<SingleSubscriberOutputStream>::spawn("cargo", cmd).unwrap();

    // Only one consumer allowed (more efficient than broadcast)
    let output_collector = process.stdout_mut().collect_lines_into_vec(
        LineParsingOptions::default()
    );

    let exit_status = process.wait_for_completion(None).await.unwrap();
    let output = output_collector.wait().await.unwrap();

    if !exit_status.success() {
        eprintln!("Build failed!");
        for line in output {
            eprintln!("{}", line);
        }
    }
}
```

### Async Output Processing

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("data-processor");
    let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("processor", cmd).unwrap();

    // Process output asynchronously (e.g., send to database)
    let _processor = process.stdout().inspect_lines_async(
        async |line| {
            // Simulate async processing
            process_line_in_database(&line).await;
            Next::Continue
        },
        LineParsingOptions::default()
    );

    process.wait_for_completion(None).await.unwrap();
}

async fn process_line_in_database(line: &str) {
    // Your async logic here
    tokio::time::sleep(Duration::from_millis(10)).await;
}
```

### Timeout with Automatic Termination

```rust ,no_run
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("potentially-hanging-process");
    let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("process", cmd).unwrap();

    // Automatically terminate if it takes too long
    match process.wait_for_completion_or_terminate(
        Duration::from_secs(30),  // Wait for 30s
        Duration::from_secs(3),   // Then send SIGINT, wait 3s
        Duration::from_secs(5),   // Then send SIGTERM, wait 5s
        // If still running, sends SIGKILL
    ).await {
        Ok(status) => println!("Completed with status: {:?}", status),
        Err(e) => eprintln!("Termination failed: {}", e),
    }
}
```

### Custom Line Parsing

The `LineParsingOptions` type controls how data is read from stdout/stderr streams.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("some-command");
    let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("cmd", cmd).unwrap();
    process.stdout().wait_for_line(
        |line| line.contains("Ready"),
        LineParsingOptions {
            max_line_length: 1.megabytes(),  // Protect against memory exhaustion
            overflow_behavior: LineOverflowBehavior::DropAdditionalData,
        },
    ).await;
}
```

## Stream Types: Which to Choose?

### BroadcastOutputStream

- ✅ Multiple concurrent consumers
- ✅ Great for logging + collecting + monitoring
- ❌ Slightly higher memory usage

### SingleSubscriberOutputStream

- ✅ More efficient (lower memory, no cloning)
- ✅ Single consumer only
- ✅ Configurable backpressure handling

## Graceful Termination

The logic of a graceful termination attempt goes like this:

1. Send **SIGINT** (Ctrl+C equivalent) - wait for shutdown
2. If still running, send **SIGTERM** - wait for shutdown
3. If still running, send **SIGKILL** - wait for shutdown

## Testing with tokio-process-tools

Perfect for integration tests:

```rust ,no_run
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::test]
async fn test_server_responds() {
    let cmd = tokio::process::Command::new("./target/debug/my-server");
    let mut server = ProcessHandle::<BroadcastOutputStream>::spawn("server", cmd).unwrap();

    // Wait for server to be ready
    server.stdout().wait_for_line(
        |line| line.contains("Listening on"),
        LineParsingOptions::default(),
    ).await;

    // Run your tests
    let response = reqwest::get("http://localhost:8080/health").await.unwrap();
    assert!(response.status().is_success());

    // Cleanup
    server.terminate(Duration::from_secs(2), Duration::from_secs(5)).await.unwrap();
}
```

**Note**: If using `TerminateOnDrop`, tests must use:

```rust ,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
}
```

## Advanced Features

### Automatic Termination on Drop

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = ProcessHandle::<BroadcastOutputStream>::spawn("cmd", cmd)
        .unwrap()
        .terminate_on_drop(Duration::from_secs(3), Duration::from_secs(5));

    // Process is automatically terminated when dropped.
    // Requires a multithreaded runtime!
}
```

### Custom Collectors

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = ProcessHandle::<BroadcastOutputStream>::spawn("cmd", cmd).unwrap();

    #[derive(Debug)]
    struct MyCollector {}

    impl MyCollector {
        fn process_line(&mut self, line: String) {
            dbg!(line);
        }
    }

    // Collect into any type implementing the Sink trait
    let custom_collector = process.stdout().collect_lines(
        MyCollector {},
        |line, custom| {
            custom.process_line(line);
            Next::Continue
        },
        LineParsingOptions::default()
    );

    let result = custom_collector.wait().await.unwrap();
}
```

### Mapped Output

Transform output before writing into sink supporting the returned by the map closure.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = ProcessHandle::<BroadcastOutputStream>::spawn("cmd", cmd).unwrap();

    let logs = Vec::new();

    let collector = process.stdout().collect_lines_into_write_mapped(
        logs, // We could also use a file handle here!
        |line| format!("[stdout] {line}\n"),
        LineParsingOptions::default()
    );
}
```

## Platform Support

- ✅ **Linux/macOS**: Using SIGINT, SIGTERM, SIGKILL
- ✅ **Windows**: Using CTRL_C_EVENT, CTRL_BREAK_EVENT

## Requirements

- Rust 1.85.0 or later (edition 2024)

## Contributing

Contributions are welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Ensure `cargo fmt` and `cargo clippy` pass
4. Add tests for new functionality
5. Submit a pull request

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
