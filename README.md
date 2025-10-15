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
tokio-process-tools = "0.7"
tokio = { version = "1", features = ["process", "sync", "io-util", "rt-multi-thread", "time"] }
```

## Examples

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

## Stream Types: Which to Choose?

As shown by the first example, the ProcessHandle is generic over how the stdout/stderr streams are processed and made 
available for consumption. There are two implementations to choose from:

### SingleSubscriberOutputStream

- ✅ More efficient (lower memory, no cloning)
- ✅ Single consumer only
- ✅ Configurable backpressure handling
- 💡 **Use when**: You only need one way to consume output (e.g., just collecting OR just monitoring)

### BroadcastOutputStream

- ✅ Multiple concurrent consumers
- ✅ Great for logging + collecting + monitoring simultaneously
- ❌ Slightly higher memory usage
- 💡 **Use when**: You need multiple operations on the same stream (e.g., log to console AND save to file)

## Output Handling

### Monitor Output in Real-Time

```rust ,no_run
use std::time::Duration;
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
use std::time::Duration;
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

### Working with Multiple Consumers

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
                vec.push(line.into_owned());
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

## Advanced Processing

### Chunk-Based Processing

For binary data or when you need raw bytes instead of lines:

```rust ,no_run
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = tokio::process::Command::new("cat");
    cmd.arg("binary-file.dat");

    let mut process = ProcessHandle::<BroadcastOutputStream>::spawn("cat", cmd).unwrap();

    // Process raw chunks of bytes
    let chunk_collector = process.stdout().collect_chunks(
        Vec::new(),
        |chunk, buffer| {
            // Process raw bytes (e.g., binary protocol parsing)
            buffer.extend_from_slice(chunk.as_ref());
        }
    );

    process.wait_for_completion(None).await.unwrap();
    let all_bytes = chunk_collector.wait().await.unwrap();

    println!("Collected {} bytes", all_bytes.len());
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
        |line| {
            let line = line.into_owned();
            async move {
                // Simulate async processing
                process_line_in_database(&line).await;
                Next::Continue
            }
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
            custom.process_line(line.into_owned());
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

    let log_file = tokio::fs::File::create("output.log").await.unwrap();

    let collector = process.stdout().collect_lines_into_write_mapped(
        log_file,
        |line| format!("[stdout] {line}\n"),
        LineParsingOptions::default()
    );
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

## Process Management

### Timeout with Automatic Termination

```rust ,no_run
use std::time::Duration;
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

### Automatic Termination on Drop

```rust ,no_run
use std::time::Duration;
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

## Testing Integration

**Note**: If you use this libraries `TerminateOnDrop` under a test, ensure that a multithreaded runtime is used with:

```rust ,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
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
