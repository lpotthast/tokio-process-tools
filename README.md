# tokio-process-tools

`tokio-process-tools` is a correctness-focused async subprocess orchestration library for Tokio. It wraps
`tokio::process::Command` for code that needs reviewable control over child-process output, readiness, stdin,
termination, and cleanup semantics.

This crate is intended for architectural boundaries: service supervisors, integration test harnesses, build tools,
development tools, and orchestration layers where a child process is part of the system's control flow. It is
deliberately more explicit than a terse command runner because each output and lifecycle choice affects reliability,
memory use, latency, or process cleanup.

## When To Use This

Use this crate when you need one or more of the following:

- an integration test must start a real service and wait until it is ready
- a build or development tool must stream live output and retain bounded diagnostic output for failure reports
- an orchestration layer must prevent child processes from leaking after failures or timeouts
- more than one subsystem needs the same stdout/stderr stream, such as logging plus readiness checks
- receiving startup output or blocking untrusted output volume are concerns

If plain `tokio::process::Command` already gives you enough control, this crate is probably more deliberate than
necessary. Its value is in making subprocess behavior explicit and predictable where lifecycle and output semantics are
load-bearing.

## Design Intent

Process spawning requires callers to choose the output backend, delivery guarantee, replay behavior, read chunk size,
and buffering limits at the call site. These choices are not hidden behind broad defaults because they decide whether
output may be dropped, whether startup output is retained, how much memory may be used, and whether slow consumers can
slow the child process down.

The intended result is that subprocess behavior is visible during review:

- whether output may be dropped or must apply backpressure
- whether late consumers can replay earlier output
- how much memory may be retained while collecting output
- whether a process must be explicitly awaited, terminated, killed, or marked with `must_not_be_terminated()` before the
  handle is dropped

The cost is a few more builder calls. The benefit is that subprocess behavior becomes part of the API contract instead
of an implicit side effect.

## Core Model

- `Process` wraps a `tokio::process::Command` and returns a typed `ProcessHandle`.
- Stdout and stderr are configured at spawn time as either `single_subscriber()` or `broadcast()` streams.
- Streams emit chunks; line consumers parse those chunks, and a final unterminated line is still surfaced at EOF.
- Delivery policy decides what happens when active consumers lag behind live process output.
- Replay policy decides whether consumers created after spawning can see earlier output.
- Collection options bound retained bytes and lines unless you explicitly choose trusted-unbounded
  collection.
- `ProcessHandle` remains armed until it is successfully waited, terminated, killed, or `must_not_be_terminated()` is
  called.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.9.0"
tokio = { version = "1", features = ["macros", "process", "rt-multi-thread"] }
```

Add `io-util`, `time`, or `fs` if your own code uses those Tokio modules in later examples.

## Quick Configuration Guide

When spawning a process, choose the stream settings that match how output will be consumed:

- Use `stream.single_subscriber()` when one consumer is enough and lower overhead matters.
- Use `stream.broadcast()` when the same stream must be logged, collected, and/or watched concurrently.
- Use `.best_effort_delivery().no_replay()` for fast live-only output where slow consumers may miss output.
- Use `.reliable_for_active_subscribers()` when active consumers must not miss output.
- Use replay, such as `.replay_last_bytes(...)`, when startup output must be available to consumers attached after the
  process was spawned.
- Use bounded collection options for untrusted output, and reserve `TrustedUnbounded` options for
  processes whose output volume is known and controlled.

New consumers start at the earliest output the stream can currently provide. With `.no_replay()`,
that is live output. With replay enabled and unsealed, it is the oldest retained output. After
`seal_output_replay()` on the `ProcessHandle` or `seal_replay()` on an individual stream, future
consumers start at live output while active consumers keep any unread data required by the delivery
policy.

## Examples

Examples use `use tokio_process_tools::*;` so the code stays focused on subprocess
configuration. In application code, prefer named imports for the types used at that boundary.

### Spawn and Wait

```rust,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let status = process.wait_for_completion(None).await.unwrap();

    println!("exit status: {status:?}");
}
```

### Wait and Collect Output

`wait_for_completion_with_output*()` attaches collectors after spawn, so replay retention is
required when startup output must be captured. Its timeout covers both process exit and the final
stdout/stderr drain.

```rust,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let line_collection = LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };

    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let ProcessOutput {
        status,
        stdout,
        stderr,
    } = process
        .wait_for_completion_with_output(
            None,
            LineOutputOptions {
                line_parsing_options: LineParsingOptions::default(),
                stdout_collection_options: line_collection,
                stderr_collection_options: line_collection,
            },
        )
        .await
        .unwrap();

    println!("exit status: {status:?}");
    println!("stdout lines: {:?}", stdout.lines());
    println!("stderr lines: {:?}", stderr.lines());
}
```

Use `wait_for_completion_with_raw_output()` when you need bytes instead of parsed lines.

### Wait for Readiness from Output

`wait_for_line()` and `wait_for_line_with_timeout()` are themselves consumers. With
`single_subscriber()`, that means only one active inspector, collector, or line waiter can exist at
a time; use `broadcast()` when you need readiness detection plus another consumer. Replay retention
lets a readiness waiter created after spawn still see startup output, and `seal_output_replay()`
ends that startup window for future consumers.

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("my-web-server"))
        .name("api")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_for_active_subscribers()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn server");

    let stdout = process.stdout();

    let _logs = stdout.inspect_lines(
        |line| {
            eprintln!("{line}");
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    match stdout
        .wait_for_line_with_timeout(
            |line| line.contains("Server listening on"),
            LineParsingOptions::default(),
            Duration::from_secs(30),
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
        .terminate(Duration::from_secs(3), Duration::from_secs(5))
        .await;
}
```

### Interactive stdin

stdin is always piped. Call `stdin().close()` when the child must observe EOF before a later wait;
terminal waits and `kill()` also close any still-open stdin automatically.

```rust,no_run
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let line_collection = LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };

    let mut process = Process::new(Command::new("cat"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    if let Some(stdin) = process.stdin().as_mut() {
        stdin.write_all(b"hello\n").await.unwrap();
        stdin.write_all(b"world\n").await.unwrap();
        stdin.flush().await.unwrap();
    }

    process.stdin().close();

    let output = process
        .wait_for_completion_with_output(
            None,
            LineOutputOptions {
                line_parsing_options: LineParsingOptions::default(),
                stdout_collection_options: line_collection,
                stderr_collection_options: line_collection,
            },
        )
        .await
        .unwrap();

    println!("stdout lines: {:?}", output.stdout.lines());
}
```

### Timeout and Cleanup

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("potentially-hanging-process"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    match process
        .wait_for_completion_or_terminate(WaitForCompletionOrTerminateOptions {
            wait_timeout: Duration::from_secs(30),
            interrupt_timeout: Duration::from_secs(3),
            terminate_timeout: Duration::from_secs(5),
        })
        .await
    {
        Ok(status) => println!("completed with status: {status:?}"),
        Err(err) => eprintln!("wait or cleanup failed: {err}"),
    }
}
```

## Stream Decision Guide

- Choose `single_subscriber()` when lower overhead matters and you do not need concurrent
  consumers.
- Choose `broadcast()` when the same stream must be consumed concurrently, such as logging plus a
  readiness wait or logging plus collection.
- Choose `best_effort_delivery()` when bounded live output and non-blocking reading matter more
  than perfect delivery. Slow active consumers may miss chunks; line-oriented consumers then
  discard any in-progress partial line and resynchronize at the next newline.
- Choose `reliable_for_active_subscribers()` when active consumers must not miss chunks inside the
  library. This protects active consumers only; late consumers still need replay.
- Choose `.no_replay()` when only live output matters.
- Choose `.replay_last_bytes(...)`, `.replay_last_chunks(...)`, or `.replay_all()` when late
  consumers must see earlier output.
- Call `seal_output_replay()` once the startup window is over. Future consumers then start at live
  output while existing consumers continue with any unread data already guaranteed by the
  configured delivery policy.

## Further APIs

- Use `wait_for_completion_with_raw_output()`, `wait_for_completion_with_output_or_terminate()`,
  and `wait_for_completion_with_raw_output_or_terminate()` for other wait-and-collect
  combinations. Select `TrustedUnbounded` collection options only when the process and output
  volume are trusted.
- Use `inspect_lines()`, `inspect_lines_async()`, `inspect_chunks()`, `inspect_chunks_async()`,
  `collect_lines()`, `collect_lines_async()`, `collect_chunks()`, and `collect_chunks_async()`
  when output handling should run in background tasks instead of a terminal wait helper.
- Use `Collector::wait()` and `Inspector::wait()` to wait for background consumers to finish
  naturally.
- Use `Collector::cancel()` and `Inspector::cancel()` for cooperative cleanup. Cancellation is
  observed between stream events, preserves collector sinks, and waits for any in-flight async
  callback or writer call; it can hang if that work hangs.
- Use `Collector::abort()` and `Inspector::abort()` when cleanup must be forceful. Abort drops
  pending async callback/write futures and releases stream subscriptions, but collector
  sinks/writers are not returned.
- Use `Collector::cancel_or_abort_after(...)` and `Inspector::cancel_or_abort_after(...)` to try
  cooperative cancellation first and force-abort if the timeout elapses.
- Use `collect_lines_into_write()`, `collect_lines_into_write_mapped()`,
  `collect_chunks_into_write()`, and `collect_chunks_into_write_mapped()` to forward output into an
  async writer. `WriteCollectionOptions` controls sink-write failures, and `LineWriteMode`
  controls whether parsed lines get `\n` added back.
- Use `LineParsingOptions`, `LineOverflowBehavior`, `wait_for_line()`, and
  `wait_for_line_with_timeout()` when line parsing or readiness detection needs custom behavior.
- Use `send_interrupt_signal()`, `send_terminate_signal()`, `terminate()`, and `kill()` for manual
  shutdown control with typed `TerminationError` diagnostics, and use `seal_stdout_replay()`,
  `seal_stderr_replay()`, or `seal_output_replay()` when replay should end on one stream or both.

## Process Management

### Process Naming

You must choose how a process is named before configuring stdout and stderr. The name appears in
public errors and tracing fields, so prefer stable explicit labels for production workflows and
reserve argument, environment, current-directory, or debug capture for values that are safe to log.

```rust,no_run
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let id = 42;
    let mut process = Process::new(Command::new("true"))
        .name(format!("agent-{id}"))
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

    process.wait_for_completion(None).await.unwrap();
}
```

Canonical naming forms:

- `.name("api-worker")` for a stable explicit label.
- `.name(AutoName::program_only())` for safe program-only automatic naming.
- `.name(AutoName::program_with_args())` to include arguments.
- `.name(AutoName::program_with_env_and_args())` to include environment variables and arguments.
- `.name(AutoName::full())` to include current directory, environment variables, program, and
  arguments.
- `.name(AutoNameSettings::builder()...build())` for a custom automatic naming combination.
- `.name(AutoName::Debug)` for the full `Command` debug representation.

### Timeout with Automatic Termination

Use `wait_for_completion_or_terminate()` as shown in `Timeout and Cleanup` above when a timeout
should immediately trigger cleanup. If you also need output collection during that cleanup path,
use `wait_for_completion_with_output_or_terminate()` or
`wait_for_completion_with_raw_output_or_terminate()` with bounded or `TrustedUnbounded`
collection options.

### Automatic Termination on Drop

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let _process = Process::new(Command::new("some-command"))
        .name(AutoName::program_only())
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
}
```

By default, dropping a live `ProcessHandle` without a successful terminal wait, `terminate()`, or
`kill()` triggers best-effort cleanup and then panics. `terminate_on_drop()` attempts async
termination on drop instead of taking the normal immediate panic path, and it requires a
multithreaded Tokio runtime. If drop-time termination fails, the inner handle can still remain
armed and trigger its fallback cleanup/panic path.

If the child must outlive the handle, call `must_not_be_terminated()` before dropping it. That
suppresses kill/panic-on-drop, but it still drops the library-owned stdio handles. Use
`ProcessHandle::into_inner()` when you need to keep the child together with captured `Stdin`,
`Stdout`, and `Stderr`.

## Testing Integration

`TerminateOnDrop` requires a multithreaded Tokio runtime. Under tests, use:

```rust,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
}
```

## Platform Support

- Linux/macOS: `SIGINT`, then `SIGTERM`, then kill
- Windows: targeted `CTRL_BREAK_EVENT`, then kill

Windows does not support targeting `CTRL_C_EVENT` at a nonzero child process group. Programs that
only handle Ctrl+C need an application-specific graceful shutdown channel such as stdin, IPC, or an
explicit command protocol.

## MSRV

MSRV is `1.89.0`.

## Contributing

Contributions are welcome:

1. Create a feature branch.
2. Run `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test`.
3. Add or update tests next to the changed module.
4. Open a pull request with the behavior change and any relevant README updates.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
