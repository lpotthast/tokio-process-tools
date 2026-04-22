# tokio-process-tools

`tokio-process-tools` is a correctness-focused async subprocess orchestration library for Tokio.
It wraps `tokio::process::Command` for code that needs reviewable control over child-process
output, readiness, stdin, termination, and cleanup semantics.

This crate is intended for architectural boundaries: service supervisors, integration test
harnesses, build tools, development tools, and orchestration layers where a child process is part
of the system's control flow. It is deliberately more explicit than a terse command runner because
each output and lifecycle choice affects reliability, memory use, latency, or process cleanup.

## When To Use This

Use this crate when you need one or more of the following:

- an integration test must start a real service and wait until it is ready
- a build or development tool must stream live output and retain bounded diagnostic output for
  failure reports
- an orchestration layer must prevent child processes from leaking after failures or timeouts
- more than one subsystem needs the same stdout/stderr stream, such as logging plus readiness checks
- receiving startup output or blocking untrusted output volume are concerns

If plain `tokio::process::Command` already gives you enough control, this crate is probably more
deliberate than necessary. Its value is in making subprocess behavior explicit and predictable
where lifecycle and output semantics are load-bearing.

## Design Intent

Process spawning requires callers to choose the output backend, delivery guarantee, replay
behavior, read chunk size, and buffering limits at the call site. These choices are not hidden
behind broad defaults because they decide whether output may be dropped, whether startup output is
retained, how much memory may be used, and whether slow consumers can slow the child process down.

The intended result is that subprocess behavior is visible during review:

- whether output may be dropped or must apply backpressure
- whether late consumers can replay earlier output
- how much memory may be retained while collecting output
- whether a process must be explicitly awaited, terminated, or detached before the handle is
  dropped

The cost is a few more builder calls. The benefit is that subprocess behavior becomes part of the
API contract instead of an implicit side effect.

## Core Model

- `Process` wraps a `tokio::process::Command` and returns a typed `ProcessHandle`.
- Stdout and stderr are configured at spawn time as either `single_subscriber()` or `broadcast()`
  streams.
- Streams emit chunks; line consumers parse those chunks and preserve a final line even without a
  trailing newline.
- Delivery policy decides what happens when active consumers lag behind live process output.
- Replay policy decides whether consumers created after spawning can see earlier output.
- Collection options bound retained bytes and lines unless you opt into trusted-output helpers.
- `ProcessHandle` remains armed until it is awaited, terminated, killed, or explicitly detached.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.9.0"
tokio = { version = "1", features = ["process", "sync", "io-util", "rt-multi-thread", "time", "macros", "fs"] }
```

## Quick Configuration Guide

When spawning a process, choose the stream settings that match how output will be consumed:

- Use `stream.single_subscriber()` when one consumer is enough and lower overhead matters.
- Use `stream.broadcast()` when the same stream must be logged, collected, and/or watched
  concurrently.
- Use `.best_effort_delivery().no_replay()` for fast live-only output where slow consumers may miss
  output.
- Use `.reliable_for_active_subscribers()` when active consumers must not miss output.
- Use replay, such as `.replay_last_bytes(...)`, when startup output must be available to consumers
  attached after the process was spawned.
- Use bounded collection options for untrusted output, and reserve `*_trusted` helpers for processes
  whose output volume is known and controlled.

## Examples

Examples use `use tokio_process_tools::*;` so the code stays focused on subprocess
configuration. In application code, prefer named imports for the types used at that boundary.

### Basic: Spawn and Wait for Completion

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("ls");
    let mut process = Process::new(cmd)
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn command");

    let status = process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
}

```

When waiting for or collecting lines, a final line without a trailing `\n` is still surfaced once
the stream reaches EOF.

### Basic: Spawn and Collect Output

The `wait_for_completion_with_output*` helpers attach collectors after the process has
been spawned, so use replay retention when startup output must be captured.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let cmd = Command::new("ls");
    let mut process = Process::new(cmd)
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn command");

    let line_collection_options = LineCollectionOptions::builder()
        .max_bytes(1.megabytes())
        .max_lines(1024)
        .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
        .build();

    let ProcessOutput { status, stdout, stderr } = process
        .wait_for_completion_with_output(
            WaitForCompletionWithOutputOptions::builder()
                .timeout(None)
                .line_output_options(
                    LineOutputOptions::builder()
                        .line_parsing_options(
                            LineParsingOptions::builder()
                                .max_line_length(16.kilobytes())
                                .overflow_behavior(LineOverflowBehavior::default())
                                .build(),
                        )
                        .stdout_collection_options(line_collection_options)
                        .stderr_collection_options(line_collection_options)
                        .build(),
                )
                .build(),
        )
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
    println!("Output: {:?}", stdout.lines());
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn command");

    let raw_collection_options = RawCollectionOptions::new(1.megabytes());

    let ProcessOutput { status, stdout, stderr } = process
        .wait_for_completion_with_raw_output(
            WaitForCompletionWithRawOutputOptions::builder()
                .timeout(None)
                .raw_output_options(
                    RawOutputOptions::builder()
                        .stdout_collection_options(raw_collection_options)
                        .stderr_collection_options(raw_collection_options)
                        .build(),
                )
                .build(),
        )
        .await
        .unwrap();

    println!("Exit status: {:?}", status);
    println!("Raw stdout bytes: {:?}", stdout.bytes);
    println!("Raw stderr bytes: {:?}", stderr.bytes);
}
```

The `*_with_output` convenience methods collect into memory and therefore require explicit
retention limits. `LineCollectionOptions` is built with a typed builder so the byte cap, line cap,
and overflow behavior are chosen at the call site. Use the `*_trusted` variants only when the child
process and its output volume are trusted.

## Stream Types: Which to choose?

When spawning a process, you first name it, then explicitly configure stdout/stderr stream
settings before `.spawn()`. Inside each stream configuration closure, choose `.broadcast()` or
`.single_subscriber()`, then choose delivery, replay, and stream options. This keeps the consumer
model visible where stream consumption behavior is configured, and it also allows stdout and stderr
to use different backends when that is useful.

### Single Subscriber (`stream.single_subscriber()`)

- ✅ More efficient (lower memory footprint)
- ✅ Configurable delivery behavior
- ✅ Optional replay for later subscribers when they attach after output already arrived
- ⚠️ **Only one active consumer allowed** - creating a second inspector/collector while one is
  still active will panic
- 💡 **Use when**: You only need one active way to consume output at a time

For `stream.single_subscriber()`, choose delivery, replay, and stream settings in the builder chain:
`.best_effort_delivery().no_replay().read_chunk_size(...).max_buffered_chunks(...)` starts each consumer at live output
and keeps output reading non-blocking when the active consumer lags. Dropping, canceling, timing
out, or completing a collector, inspector, or line waiter releases the stream so another consumer
can attach later. Use
`.reliable_for_active_subscribers()` when the active consumer must not miss chunks inside the
library, and choose a replay-enabled mode when output produced before or between consumers must be
retained. Replay may let a later sequential consumer observe retained output that an earlier
consumer also saw; `.no_replay()` discards output drained while no consumer is active.

The process builder intentionally has no defaults for delivery or replay. Pick
`.best_effort_delivery().no_replay()` for the fastest live-only path, or choose reliable delivery
and a replay mode when correctness needs it.

### Broadcast (`stream.broadcast()`)

- ✅ Multiple concurrent consumers
- ✅ Great for logging + collecting + monitoring simultaneously
- ✅ Fast, bounded, and best-effort by default
- ✅ Optional replay for subscribers that attach after output already arrived
- ✅ Optional reliable delivery for active subscribers
- ⚠️ Slightly higher runtime costs
- ⚠️ Slow consumers may lag and miss chunks when buffers overflow
- 💡 **Use when**: You need multiple operations on the same stream (e.g., log to console AND save to file)

Use `.best_effort_delivery().no_replay()` for a fast bounded mode with gap events for slow
subscribers. Use `.reliable_for_active_subscribers()` when active subscribers must not miss chunks
inside the library, and choose a replay-enabled mode when late subscribers need retained output.

Broadcast delivery and replay choices flow into the returned handle type. `StreamConfig`
remains available for direct stream construction; normal process spawning configures these choices
inside the stdout/stderr stream closure.

In `DeliveryGuarantee::ReliableForActiveSubscribers` mode, `max_buffered_chunks`
is the maximum unread chunk lag an active subscriber can have before output reading waits.

You cannot have bounded memory, lossless replay for future subscribers, and guaranteed non-blocking
process output at the same time.

For correctness-sensitive startup waits, retain replay only until the process has passed its
readiness phase:

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("my-web-server"))
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_for_active_subscribers()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let stdout = process.stdout();

    let _logs = stdout.inspect_lines(
        |line| {
            eprintln!("{line}");
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    let ready = stdout.wait_for_line_from_start_with_timeout(
        |line| line.contains("Server listening on"),
        LineParsingOptions::default(),
        Duration::from_secs(30),
    );

    process.seal_output_replay();

    match ready.await {
        Ok(WaitForLineResult::Matched) => {}
        Ok(WaitForLineResult::StreamClosed) => panic!("server exited before becoming ready"),
        Ok(WaitForLineResult::Timeout) => panic!("server failed to start in time"),
        Err(err) => panic!("server output stream failed: {err}"),
    }

    let _ = process.terminate(
        Duration::from_secs(3),
        Duration::from_secs(5),
    ).await;
}
```

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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .reliable_for_active_subscribers()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
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
        Duration::from_secs(3),  // interrupt timeout
        Duration::from_secs(5),  // terminate timeout
    ).await;
}
```

### Wait for Specific Output

Perfect for integration tests or ensuring services are ready:

Note: `wait_for_line` and `wait_for_line_with_timeout` are stream consumers. When using
`stream.single_subscriber()` for stdout or stderr, each method claims that stream while the returned
future is active. Once it matches, times out, is dropped, or observes stream closure, another
single-subscriber consumer can attach. Use `stream.broadcast()` if you need to wait for readiness
and also inspect or collect the same stream concurrently.
Also note: `wait_for_line(...)` only returns `Ok(Matched)` or `Ok(StreamClosed)`; `Ok(Timeout)` is
only produced by `wait_for_line_with_timeout(...)`. Stream read failures are returned as
`Err(StreamReadError)`.
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    // Wait for the server to be ready
    match process.stdout().wait_for_line_with_timeout(
        |line| line.contains("Server listening on"),
        LineParsingOptions::default(),
        Duration::from_secs(30),
    ).await {
        Ok(WaitForLineResult::Matched) => println!("Server is ready!"),
        Ok(WaitForLineResult::StreamClosed) => panic!("Server exited before becoming ready"),
        Ok(WaitForLineResult::Timeout) => panic!("Server failed to start in time"),
        Err(err) => panic!("Server output stream failed: {err}"),
    }
    // Now safe to make requests to the server
    // ...
    // Cleanup
    let _ = process.wait_for_completion_or_terminate(
        WaitForCompletionOrTerminateOptions::builder()
            .wait_timeout(Duration::from_secs(5))
            .interrupt_timeout(Duration::from_secs(3))
            .terminate_timeout(Duration::from_secs(5))
            .build(),
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
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
        WriteCollectionOptions::fail_fast(),
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
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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

Writer collectors also require `WriteCollectionOptions`; choose one of the explicit constructors:

- `WriteCollectionOptions::fail_fast()` stops collection and makes `Collector::wait()` return
  `CollectorError::SinkWrite` on the first sink write failure.
- `WriteCollectionOptions::log_and_continue()` logs each sink write failure with `tracing::warn!`
  and keeps collecting. Accepted failures can lose data.
- `WriteCollectionOptions::with_error_handler(...)` calls your handler for each sink write failure,
  so you can inspect the stream name, operation, attempted byte length, and source error before
  returning `SinkWriteErrorAction::Continue` or `SinkWriteErrorAction::Stop`.

`WriteCollectionOptions` keeps the handler type generic so custom handlers are statically
dispatched and allocation-free.

```rust ,no_run
# use tokio_process_tools::*;
use std::time::Duration;
use tokio::process::Command;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("cat");
    let mut process = Process::new(cmd)
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
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
    let line_collection_options = LineCollectionOptions::builder()
        .max_bytes(1.megabytes())
        .max_lines(1024)
        .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
        .build();

    let output = process
        .wait_for_completion_with_output(
            WaitForCompletionWithOutputOptions::builder()
                .timeout(Some(Duration::from_secs(2)))
                .line_output_options(
                    LineOutputOptions::builder()
                        .line_parsing_options(
                            LineParsingOptions::builder()
                                .max_line_length(16.kilobytes())
                                .overflow_behavior(LineOverflowBehavior::default())
                                .build(),
                        )
                        .stdout_collection_options(line_collection_options)
                        .stderr_collection_options(line_collection_options)
                        .build(),
                )
                .build(),
        )
        .await
        .unwrap();

    println!("Output: {:?}", output.stdout.lines());
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

    let mut process = Process::new(cmd)
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    // Monitor output.
    let collector = process
        .stdout()
        .collect_lines_into_vec(
            LineParsingOptions::default(),
            LineCollectionOptions::builder()
                .max_bytes(1.megabytes())
                .max_lines(1024)
                .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
                .build(),
        );
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
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(Some(Duration::from_secs(1)))
                .build(),
        )
        .await
        .unwrap();

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().len()).is_equal_to(1);
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
- **Ownership transfer**: `ProcessHandle::into_inner()` returns `(Child, Stdin, Stdout, Stderr)`. The returned `Child`
  does not own its `stdin` field; keep or use the returned `Stdin` instead. Dropping `Stdin::Open` closes the pipe, so
  the child may observe EOF and exit or otherwise change behavior.

## Advanced Processing

### Configuring Stream Read Sizes

You can customize both the read chunk size (buffer size for reading from the process) and the maximum number of chunks
that can be buffered:

**Read Chunk Size**: Controls the size of the buffer used when reading from stdout/stderr. Larger chunks reduce syscall
overhead but use more memory per read and therefore more memory overall. Default is 16 KB. Lower values may be chosen
for them to fit in smaller CPU caches. Read chunk size must be greater than zero; configuring a zero-byte read chunk
size panics.

**Max Buffered Chunks**: Controls how many chunks can be queued before backpressure is applied. Higher capacity allows
more buffering but uses more memory. Default is 128. Max buffered chunks must be greater than zero; configuring zero
buffered chunks panics.

In the default configuration, having 128 slots with 16kb (max) chunks each, a maximum of 2 megabytes is consumed per
stream.

```rust ,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("my-app"))
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(32.kilobytes())
                .max_buffered_chunks(256)
        })
        .spawn()
        .unwrap();
    // Your process handling code...
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    // Process raw chunks of bytes
    let chunk_collector = process.stdout().collect_chunks(
        Vec::new(),
        |chunk, buffer| {
            // Process raw bytes (e.g., binary protocol parsing)
            buffer.extend_from_slice(chunk.as_ref());
        }
    );

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
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

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
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

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let custom_collector = process.stdout().collect_lines_async(
        MyCollector::default(),
        StoreLine,
        LineParsingOptions::default()
    );

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let log_file = tokio::fs::File::create("output.log").await.unwrap();

    let collector = process.stdout().collect_lines_into_write_mapped(
        log_file,
        |line| format!("[stdout] {line}\n"),
        LineParsingOptions::default(),
        LineWriteMode::AsIs,
        WriteCollectionOptions::fail_fast(),
    );

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    let result = process.stdout().wait_for_line(
        |line| line.contains("Ready"),
        LineParsingOptions {
            max_line_length: 1.megabytes(),  // Protect against memory exhaustion
            overflow_behavior: LineOverflowBehavior::DropAdditionalData,
        },
    ).await;

    println!("wait result: {result:?}");
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
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
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
}
```

#### Automatic Name Generation

Use `.with_auto_name()` to auto-generate names from the command.

This is the DEFAULT behavior if no name-related builder function is called, using the
`program_only` setting. Process names are included in public errors and tracing logs, so
arguments, environment variables, current directories, and debug output should only be captured
when those values are safe to log.

```rust ,no_run
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // Only program name (DEFAULT). Name will be "ls"
    let mut process = Process::new(Command::new("ls"))
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
    // Include arguments in the name (explicit opt-in). Name will be "true \"--help\""
    let mut cmd = Command::new("true");
    cmd.arg("--help");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_args()))
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
    // Include environment variables and arguments in the name. Name will include "S3_ENDPOINT=127.0.0.1:9000 true \"--help\""
    let mut cmd = Command::new("true");
    cmd.arg("--help");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Using(AutoNameSettings::program_with_env_and_args()))
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
    // Full debug output (for troubleshooting). Name includes all tokio Command details.
    let mut cmd = Command::new("true");
    cmd.arg("--help");
    cmd.env("S3_ENDPOINT", "127.0.0.1:9000");

    let mut process = Process::new(cmd)
        .with_auto_name(AutoName::Debug)
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    process
        .wait_for_completion(
            WaitForCompletionOptions::builder()
                .timeout(None)
                .build(),
        )
        .await
        .unwrap();
}
```

Available auto-naming modes:

- `AutoName::Using(AutoNameSettings)` (default) - Configurable name generation. Defaults to
  `AutoNameSettings::program_only()`.
- `AutoName::Debug` - Full debug output with internal details (for debugging). May include
  command arguments, environment variables, and other sensitive data.

### Timeout with Automatic Termination

```rust ,no_run
use std::time::Duration;
use tokio_process_tools::*;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    let mut cmd = Command::new("potentially-hanging-process");
    let mut process = Process::new(cmd)
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    // Automatically terminate if waiting fails or takes too long
    match process.wait_for_completion_or_terminate(
        WaitForCompletionOrTerminateOptions::builder()
            .wait_timeout(Duration::from_secs(30))
            .interrupt_timeout(Duration::from_secs(3))
            .terminate_timeout(Duration::from_secs(5))
            .build(),
    ).await {
        Ok(status) => println!("Completed with status: {:?}", status),
        Err(e) => eprintln!("Wait or cleanup failed: {}", e),
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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
        .terminate_on_drop(Duration::from_secs(3), Duration::from_secs(5));
    // Process is automatically terminated when dropped.
    // Requires a multithreaded runtime!
}
```

By default, `ProcessHandle` expects you to explicitly call `wait_for_completion(...)`,
`terminate(...)`, or successfully call `kill()`. Dropping a live handle without doing so first
triggers best-effort cleanup and then panics to make the misuse obvious.

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
        .auto_name()
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    process.must_not_be_terminated();
    drop(process); // No signal/kill is sent.
}
```

Important: this still drops the library-owned stdin/stdout/stderr pipe endpoints. A child that is
still reading stdin or writing to stdout/stderr may therefore observe EOF or closed pipes. Use
`ProcessHandle::into_inner()` when you need to take ownership of the child together with its
library-managed stdin/stdout/stderr handles. If you discard the returned `Stdin`, the child's stdin
pipe is closed and the child may observe EOF. Use plain `tokio::process::Command` directly when you
need a child that can truly outlive the original handle without captured stdio.

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
- ✅ **Windows**: Using targeted `CTRL_BREAK_EVENT`, then kill

Windows does not support targeting `CTRL_C_EVENT` at a nonzero child process group. Programs that
only handle Ctrl+C need an application-specific graceful shutdown channel such as stdin, IPC, or an
explicit command protocol.

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
