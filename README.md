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

### Basic: Spawn and Wait for Completion

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("ls");
    let mut process = Process::new(cmd)
        .spawn_single_subscriber()
        .expect("Failed to spawn command");

    let status = process
        .wait_for_completion(None)
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
}

```

### Basic: Spawn and Collect Output

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("ls");
    let mut process = Process::new(cmd)
        .spawn_single_subscriber()
        .expect("Failed to spawn command");

    let Output { status, stdout, stderr } = process
        .wait_for_completion_with_output(None, LineParsingOptions::default())
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
    println!("Output: {:?}", stdout);
}
```

## Stream Types: Which to choose?

When spawning a process, you choose the stream type explicitly by calling either `.spawn_broadcast()` or
`.spawn_single_subscriber()`.

### Single Subscriber (`.spawn_single_subscriber()`)

- ✅ More efficient (lower memory, no cloning)
- ✅ Single consumer only
- ✅ Configurable backpressure handling
- 💡 **Use when**: You only need one way to consume output (e.g., just collecting OR just monitoring)
- ☝️Requires `.stdout_mut()` and `.stderr_mut()` for interaction with streams.

### Broadcast (`.spawn_broadcast()`)

- ✅ Multiple concurrent consumers
- ✅ Great for logging + collecting + monitoring simultaneously
- ℹ️ Slightly higher runtime costs
- 💡 **Use when**: You need multiple operations on the same stream (e.g., log to console AND save to file)
- ☝️Uses `.stdout()` and `.stderr()` for interaction with streams.

## Output Handling

### Monitor Output in Real-Time

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("tail");
    cmd.arg("-f").arg("/var/log/app.log");

    let mut process = Process::new(cmd)
        .spawn_single_subscriber()
        .unwrap();

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
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("my-web-server");
    let mut process = Process::new(cmd)
        .spawn_single_subscriber()
        .unwrap();

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
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("long-running-process");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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

### Configuring Buffer Sizes

You can customize both the chunk size (buffer size for reading from the process) and the channel capacity (number of
chunks that can be buffered):

**Chunk Size**: Controls the size of the buffer used when reading from stdout/stderr. Larger chunks reduce syscall
overhead but use more memory per read and therefore more memory overall. Default is 16 KB. Lower values may be chosen
for them to fit in smaller CPU caches.

**Channel Capacity**: Controls how many chunks can be queued before backpressure is applied. Higher capacity allows more
buffering but uses more memory. Default is 128.

In the default configuration, having 128 slots with 16kb (max) chunks each, a maximum of 2 megabytes is consumed per
stream.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("my-app"))
        // Configure chunk sizes (how much data is read at once).
        .stdout_chunk_size(4.kilobytes())
        .stderr_chunk_size(4.kilobytes())
        // Or set both at once.
        .chunk_sizes(32.kilobytes())
        
        // Configure channel capacities (how many chunks can be buffered).
        .stdout_capacity(64)
        .stderr_capacity(64)
        // Or set both at once.
        .capacities(256)

        .spawn_broadcast()
        .unwrap();

    // Your process handling code...
    process.wait_for_completion(None).await.unwrap();
}
```

### Chunk-Based Processing

For binary data or when you need raw bytes instead of lines:

```rust ,no_run
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("cat");
    cmd.arg("binary-file.dat");

    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("data-processor");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("some-command");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();
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

### Process Naming

You can control how processes are named for logging and debugging. The library provides two approaches:

#### Explicit Names

Use `.with_name()` to set a custom name:

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let id = 42;
    let mut process = Process::new(Command::new("agent"))
        .with_name(format!("agent-{id}"))
        .spawn_single_subscriber()
        .unwrap();
}
```

#### Automatic Name Generation

Use `.with_auto_name()` to auto-generate names from the command.

This is the DEFAULT behavior if no name-related builder function is called (using `program_with_args` setting).

```rust ,no_run
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // Only program name. Name will be "ls"
    let mut process = Process::new(Command::new("ls"))
        .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
        .spawn_broadcast()
        .unwrap();

    // Include arguments in the name (DEFAULT). Name will be "cargo test --all-features"
    let mut cmd = Command::new("cargo");
    cmd.arg("test").arg("--all-features");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_args()))
        .spawn_broadcast()
        .unwrap();

    // Include environment variables and arguments in the name. Name will be "S3_ENDPOINT=127.0.0.1:9000 cargo test --all-features"
    let mut cmd = Command::new("cargo");
    cmd.arg("test").arg("--all-features");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_env_and_args()))
        .spawn_broadcast()
        .unwrap();

    // Full debug output (for troubleshooting). Name includes all tokio Command details.
    let mut cmd = Command::new("server");
    cmd.arg("--port").arg("8080");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Debug)
        .spawn_broadcast()
        .unwrap();
}
```

Available auto-naming modes:

- `AutoName::Using(AutoNameSettings)` (default) - Configurable name generation
- `AutoName::Debug` - Full debug output with internal details (for debugging)

### Timeout with Automatic Termination

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("potentially-hanging-process");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

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
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let process = Process::new(cmd)
        .spawn_broadcast()
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
