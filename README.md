# tokio-process-tools

A powerful library for spawning and managing processes in the Tokio runtime with advanced output handling capabilities.

When working with child processes in async Rust, you often need to:

- **Monitor output in real-time** without blocking
- **Wait for specific log messages** before proceeding
- **Collect output** for later analysis
- **Handle multiple concurrent consumers** of the same stream
- **Write input data to processes** programmatically
- **Gracefully terminate** processes with proper signal handling
- **Prevent spawned processes from leaking**, not being terminated properly

`tokio-process-tools` tries to make all of this simple and ergonomic.

## Features

- ✨ **Real-time Output Inspection** - Monitor stdout/stderr as they arrive, with sync and async callbacks
- 🔍 **Pattern Matching** - Wait for specific output before continuing execution
- 🎯 **Flexible Collection** - Gather output into vectors, files, or custom sinks
- 🔄 **Multiple Consumers** - Support for both single and broadcast (multi-consumer) stream consumption
- 📝 **Programmatic Process Input** - Write data to the processes stdin stream, close it when done
- ⚡ **Graceful Termination** - Automatic signal escalation (SIGINT → SIGTERM → kill, or platform equivalents)
- 🛡️ **Collection Safety** - Fine-grained control over buffers used when collecting stdout/stderr streams
- 🛡️ **Resource Safety** - Dropping a live, still-armed `ProcessHandle` performs best-effort cleanup and then panics
  loudly
- 📄 **Correct EOF Semantics** - Final lines are preserved even when a process exits without a trailing newline
- ⏱️ **Timeout Support** - Built-in timeout handling for all operations
- 🌊 **Backpressure Control** - Configurable behavior when consumers can't keep up

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.8.0"
tokio = { version = "1", features = ["process", "sync", "io-util", "rt-multi-thread", "time", "macros", "fs"] }
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

When waiting for or collecting lines, a final line without a trailing `\n` is still surfaced once
the stream reaches EOF.

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

Use `wait_for_completion_with_raw_output(...)` when you need bytes instead of parsed lines:

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("ls");
    let mut process = Process::new(cmd)
        .spawn_single_subscriber()
        .expect("Failed to spawn command");

    let RawOutput { status, stdout, stderr } = process
        .wait_for_completion_with_raw_output(None)
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
    println!("Raw stdout bytes: {:?}", stdout);
    println!("Raw stderr bytes: {:?}", stderr);
}
```

## Stream Types: Which to choose?

When spawning a process, you choose the stream type explicitly by calling either `.spawn_broadcast()` or
`.spawn_single_subscriber()`.

### Single Subscriber (`.spawn_single_subscriber()`)

- ✅ More efficient (lower memory footprint)
- ✅ Configurable backpressure handling
- ⚠️ **Only one consumer allowed** - creating a second inspector/collector will panic
- 💡 **Use when**: You only need one way to consume output (e.g., just collecting OR just monitoring)

For `.spawn_single_subscriber()`, the default backpressure policy is
`BackpressureControl::DropLatestIncomingIfBufferFull`, which favors not blocking the child process.
If you are waiting for a specific readiness line and prefer reliability over throughput, switch to
`BackpressureControl::BlockUntilBufferHasSpace` via the `Process` builder.

### Broadcast (`.spawn_broadcast()`)

- ✅ Multiple concurrent consumers
- ✅ Great for logging + collecting + monitoring simultaneously
- ⚠️ Slightly higher runtime costs
- ⚠️ Slow consumers may lag and miss chunks when buffers overflow
- 💡 **Use when**: You need multiple operations on the same stream (e.g., log to console AND save to file)

Broadcast streams are built on `tokio::sync::broadcast`, so they do not support blocking
backpressure. If a receiver falls behind, older chunks may be dropped for that receiver. If you
need reliable delivery with blocking backpressure, use `.spawn_single_subscriber()` together with
`BackpressureControl::BlockUntilBufferHasSpace` and fan out from that one reliable consumer
yourself.

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
        .stdout_backpressure_control(BackpressureControl::BlockUntilBufferHasSpace)
        .spawn_single_subscriber()
        .unwrap();

    // Inspect output in real-time
    let _stdout_monitor = process.stdout().inspect_lines(
        |line| {
            println!("stdout: {line}");
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // Let it run for a while
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Gracefully terminate
    let _ = process.terminate(
        Duration::from_secs(3),  // SIGINT timeout
        Duration::from_secs(5),  // SIGTERM timeout
    ).await;
}
```

### Wait for Specific Output

Perfect for integration tests or ensuring services are ready:

Note: `wait_for_line` and `wait_for_line_with_timeout` are stream consumers. When using
`.spawn_single_subscriber()`, calling either method claims the only stdout/stderr receiver, so no
other inspector or collector can be attached to that stream afterward. Use `.spawn_broadcast()` if
you need to wait for readiness and also inspect or collect the same stream elsewhere.
Also note: `wait_for_line(...)` only returns `Matched` or `StreamClosed`; `Timeout` is only produced
by `wait_for_line_with_timeout(...)`.
When line-oriented consumers miss chunks because of lossy buffering, they discard any partial line
in progress and resynchronize at the next newline instead of stitching bytes across the gap.

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
    match process.stdout().wait_for_line_with_timeout(
        |line| line.contains("Server listening on"),
        LineParsingOptions::default(),
        Duration::from_secs(30),
    ).await {
        WaitForLineResult::Matched => println!("Server is ready!"),
        WaitForLineResult::StreamClosed => panic!("Server exited before becoming ready"),
        WaitForLineResult::Timeout => panic!("Server failed to start in time"),
    }

    // Now safe to make requests to the server
    // ...

    // Cleanup
    let _ = process.wait_for_completion_or_terminate(
        Duration::from_secs(5),   // Wait timeout
        Duration::from_secs(3),   // SIGINT timeout
        Duration::from_secs(5),   // SIGTERM timeout
    ).await;
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
    let logger = process.stdout().inspect_lines(
        |line| {
            eprintln!("[LOG] {line}");
            Next::Continue
        },
        LineParsingOptions::default()
    );

    // Consumer 2: Collect to file
    let log_file = tokio::fs::File::create("output.log").await.unwrap();
    let file_writer = process.stdout().collect_lines_into_write(
        log_file,
        LineParsingOptions::default(),
        LineWriteMode::AppendLf,
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
    logger.wait().await.unwrap();
    let _log_file = file_writer.wait().await.unwrap();
    let errors = error_collector.wait().await.unwrap();
    println!("Found {} errors", errors.len());
}
```

`collect_lines_into_write(...)` and `collect_lines_into_write_mapped(...)` require an explicit
`LineWriteMode` because parsed lines no longer contain their original newline byte. Use
`LineWriteMode::AppendLf` to reconstruct line-oriented output or `LineWriteMode::AsIs` when your
mapper already adds delimiters.

## Input Handling (stdin)

### Writing to Process stdin

Process stdin is always (and automatically) configured as piped, allowing you to write data to processes
programmatically:

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::*;
use tokio::process::Command;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("cat");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

    // Write data to stdin.
    if let Some(stdin) = process.stdin().as_mut() {
        stdin.write_all(b"Hello, process!\n").await.unwrap();
        stdin.write_all(b"More input data\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    // Close stdin to signal EOF.
    process.stdin().close();

    // Collect output.
    let output = process
        .wait_for_completion_with_output(
            Some(Duration::from_secs(2)),
            LineParsingOptions::default()
        )
        .await
        .unwrap();

    println!("Output: {:?}", output.stdout);
}
```

### Interactive Process Communication

Send commands to interactive programs and wait for responses:

```rust ,no_run
use assertr::prelude::*;
use tokio_process_tools::*;
use tokio::process::Command;
use tokio::io::AsyncWriteExt;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let cmd = Command::new("sh");

    let mut process = Process::new(cmd).spawn_broadcast().unwrap();

    // Monitor output.
    let collector = process
        .stdout()
        .collect_lines_into_vec(LineParsingOptions::default());

    // Send commands to the shell.
    if let Some(stdin) = process.stdin().as_mut() {
        stdin
            .write_all(b"printf 'Hello from shell\\n'\nexit\n")
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    // Wait a bit for output.
    tokio::time::sleep(Duration::from_millis(500)).await;

    process.stdin().close();
    process
        .wait_for_completion(Some(Duration::from_secs(1)))
        .await
        .unwrap();

    let collected = collector.wait().await.unwrap();
    assert_that!(&collected).has_length(1);
    assert_that!(collected[0].as_str()).is_equal_to("Hello from shell");
}
```

### Key Points about stdin handling

- **Always Piped**: stdin is automatically configured as `Stdio::piped()` for all processes, only allowing controlled
  input.
- **Writing**: `stdin().as_mut()` directly exposes tokio's `ChildStdin` type allowing writes using the `AsyncWriteExt`
  trait methods, like `write_all()` and `flush()`.
- **Closing**: Call `stdin().close()` to let the process receive an EOF signal on its stdin stream and allowing no
  further writes.

## Advanced Processing

### Configuring Buffer Sizes

You can customize both the chunk size (buffer size for reading from the process) and the channel capacity (number of
chunks that can be buffered):

**Chunk Size**: Controls the size of the buffer used when reading from stdout/stderr. Larger chunks reduce syscall
overhead but use more memory per read and therefore more memory overall. Default is 16 KB. Lower values may be chosen
for them to fit in smaller CPU caches. Chunk size must be greater than zero; configuring a zero-byte chunk size panics.

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
    let processor = process.stdout().inspect_lines_async(
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
    processor.wait().await.unwrap();
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
    let mut process = Process::new(cmd)
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

    process.wait_for_completion(None).await.unwrap();
    let result = custom_collector.wait().await.unwrap();
}
```

Async collectors implement `AsyncLineCollector` or `AsyncChunkCollector`. This avoids allocating a
boxed future for every collected item. The API uses traits for now because stable Rust cannot yet
express the required `Send` bound on futures returned by `AsyncFn` callbacks.

```rust ,no_run
use std::borrow::Cow;
use tokio::process::Command;
use tokio_process_tools::*;

#[derive(Debug, Default)]
struct MyCollector {
    lines: Vec<String>,
}

struct StoreLine;

impl AsyncLineCollector<MyCollector> for StoreLine {
    async fn collect<'a>(
        &'a mut self,
        line: Cow<'a, str>,
        custom: &'a mut MyCollector,
    ) -> Next {
        custom.lines.push(line.into_owned());
        Next::Continue
    }
}

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

    let custom_collector = process.stdout().collect_lines_async(
        MyCollector::default(),
        StoreLine,
        LineParsingOptions::default()
    );

    process.wait_for_completion(None).await.unwrap();
    let result = custom_collector.wait().await.unwrap();
}
```

### Mapped Output

Transform parsed lines before writing them into an async writer:

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("some-command");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();

    let log_file = tokio::fs::File::create("output.log").await.unwrap();

    let collector = process.stdout().collect_lines_into_write_mapped(
        log_file,
        |line| format!("[stdout] {line}\n"),
        LineParsingOptions::default(),
        LineWriteMode::AsIs,
    );

    process.wait_for_completion(None).await.unwrap();
    let _log_file = collector.wait().await.unwrap();
}
```

### Custom Line Parsing

The `LineParsingOptions` type controls how data is read from stdout/stderr streams.
`LineOverflowBehavior::DropAdditionalData` now keeps discarding across chunk boundaries until the
next newline is seen.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("some-command");
    let mut process = Process::new(cmd)
        .spawn_broadcast()
        .unwrap();
    let result = process.stdout().wait_for_line(
        |line| line.contains("Ready"),
        LineParsingOptions {
            max_line_length: 1.megabytes(),  // Protect against memory exhaustion
            overflow_behavior: LineOverflowBehavior::DropAdditionalData,
        },
    ).await;

    println!("wait result: {result:?}");
    process.wait_for_completion(None).await.unwrap();
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
    let mut process = Process::new(Command::new("true"))
        .with_name(format!("agent-{id}"))
        .spawn_single_subscriber()
        .unwrap();

    process.wait_for_completion(None).await.unwrap();
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
    process.wait_for_completion(None).await.unwrap();

    // Include arguments in the name (DEFAULT). Name will be "true \"--help\""
    let mut cmd = Command::new("true");
    cmd.arg("--help");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_args()))
        .spawn_broadcast()
        .unwrap();
    process.wait_for_completion(None).await.unwrap();

    // Include environment variables and arguments in the name. Name will include "S3_ENDPOINT=127.0.0.1:9000 true \"--help\""
    let mut cmd = Command::new("true");
    cmd.arg("--help");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_env_and_args()))
        .spawn_broadcast()
        .unwrap();
    process.wait_for_completion(None).await.unwrap();

    // Full debug output (for troubleshooting). Name includes all tokio Command details.
    let mut cmd = Command::new("true");
    cmd.arg("--help");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Debug)
        .spawn_broadcast()
        .unwrap();
    process.wait_for_completion(None).await.unwrap();
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
        // If still running, kills the process
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

By default, `ProcessHandle` expects you to explicitly call `wait_for_completion(...)` or
`terminate(...)`. Dropping a live handle without doing so first triggers best-effort cleanup and
then panics to make the misuse obvious.

`terminate(...)` also handles the common race where the child exits just before signalling starts:
it reaps the exit status and returns success instead of surfacing a spurious signalling failure.

If you intentionally want the spawned child to outlive the handle, call
`must_not_be_terminated()` before dropping it:

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("some-command"))
        .spawn_broadcast()
        .unwrap();

    process.must_not_be_terminated();
    drop(process); // No signal/kill is sent.
}
```

Important: this still drops the library-owned stdin/stdout/stderr pipe endpoints. A child that is
still reading stdin or writing to stdout/stderr may therefore observe EOF or closed pipes. Use
plain `tokio::process::Command` directly when you need a child that can truly outlive the original
handle without captured stdio.

## Testing Integration

**Note**: If you use this libraries `TerminateOnDrop` under a test, ensure that a multithreaded runtime is used with:

```rust ,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
}
```

## Platform Support

- ✅ **Linux/macOS**: Using SIGINT, SIGTERM, then kill
- ✅ **Windows**: Using `CTRL_C_EVENT`, `CTRL_BREAK_EVENT`, then kill

## MSRV

- From version `0.8.0` onwards, the MSRV is `1.89.0`.

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
