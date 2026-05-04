# tokio-process-tools

[![Crates.io](https://img.shields.io/crates/v/tokio-process-tools.svg)](https://crates.io/crates/tokio-process-tools)
[![Docs.rs](https://docs.rs/tokio-process-tools/badge.svg)](https://docs.rs/tokio-process-tools)
[![CI](https://github.com/lpotthast/tokio-process-tools/actions/workflows/ci.yml/badge.svg)](https://github.com/lpotthast/tokio-process-tools/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.89.0-blue.svg)](https://github.com/lpotthast/tokio-process-tools/blob/main/Cargo.toml)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/tokio-process-tools.svg)](#license)

A correctness-focused async subprocess library for Tokio. It wraps `tokio::process::Command` and exposes explicit
controls over resource limits, output consumption, process termination, and cleanup.

Most subprocess code in the wild leaves a handful of decisions implicit:

- What happens when a log consumer falls behind? Is data dropped or is the child blocked?
- Can a stream consumer reliably receive all child startup output even when attached late?
- How much memory will the system need for stream processing? How much for line processing?
- When and how is the child terminated?
- What happens to the child if the parent panics?

Each of those questions may be answered with a hidden default. Some may be fine in isolation. But in aggregate, system
behavior becomes dangerously uncertain. The result can be: silent gaps, leaked children, unbounded buffering.

`tokio-process-tools` makes every one of those choices a required argument at the process construction call site. There
is no default delivery or replay policy, no default buffer sizes. The cost is a more verbose spawn call, the benefit is
that the choice is visible in code review and to whoever debugs the system at 3am.

## When to use this

Reach for it when you are developing any of:

- a build or development tool that streams live output and keeps a bounded tail for failure reports,
- an integration test that starts a real service and waits until it is ready,
- an orchestration layer that must not leak children after failures or timeouts,

Or when you need any of:

- one stream consumed by more than one subsystem (e.g. logging plus readiness checks),
- bounded memory in the face of untrusted or unknown output volume.

If `tokio::process::Command` already gives you enough control, this crate may add too much ceremony.

## Core model

The library has three layers, and most of the configuration in this README is about the
relationship between them:

- A **`Process`** is a spawn configuration. `Process::new(command)` starts the chain, `.spawn()` returns a
  `ProcessHandle`.
- An **output stream** is the buffered pipeline that sits between a child's stdout (or stderr) and your code. Each
  stream is configured at spawn time (see [Choosing stream settings](#choosing-stream-settings)) and emits raw chunks,
  which consumers parse on the fly.
- A **consumer** is anything that reads from a stream: an inspector that runs a callback, a collector that retains
  output for later, a `wait_for_line()` that resolves on a match. Consumers are implemented through a visitor pattern,
  allowing for custom extensions to be written.

Consumers attach after spawn, so anything the child writes before a consumer is in place is gone unless a replay policy
retains it for late arrivals.

A `ProcessHandle` is "armed" until it is successfully waited, terminated, killed, or explicitly excused with
`must_not_be_terminated()`. Dropping an armed handle without one of those calls runs best-effort cleanup and then
panics. Better to be loud than to silently leak children.
See [Automatic termination on drop](#automatic-termination-on-drop) for the opt-out.

## Quickstart

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.10.0"
tokio = { version = "1", features = ["macros", "process", "rt-multi-thread"] }
```

The interactive-stdin example further down uses `tokio::io::AsyncWriteExt`, which requires the `io-util` Tokio feature
in your own crate.

The minimum viable spawn: run a command to completion with a deadline and check its exit status. `stream.discard()`
routes the child's stdout and stderr to `/dev/null` at the OS level, so no pipe is allocated and no reader task runs in
the parent. Use this when you only care about whether the process succeeded, not what it wrote.

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| stream.discard())
        .spawn()
        .expect("failed to spawn command");

    let status = process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("exit status: {status:?}");
}
```

A discarded stream still implements `OutputStream` (so `process.stdout()` and `process.stderr()` still return
something), but it does not implement `TrySubscribable`: the consumer-attaching APIs (`wait_for_completion_with_output`,
`inspect_lines`, `collect_lines`, etc.) are not in scope on a handle whose stream is discarded. If you later decide you
do want to read the output, swap `discard()` for one of the streaming chains described in the next section.

Code throughout this README uses `use tokio_process_tools::*;` to keep examples short; prefer named imports in
application code. Byte sizes use the `NumBytes` type, and the `NumBytesExt` trait (re-exported at the crate root) lets
you write `1.megabytes()`, `16.kilobytes()`, or `512.bytes()` on integer (usize) literals.

## Choosing stream settings

Each stdout/stderr stream is configured along five axes: backend, delivery, replay, collection, and buffering.

**Backend: who consumes the stream?**

- `broadcast()`: Allows for multiple concurrent consumers, e.g. logging plus a line waiter.
- `single_subscriber()`: Exactly one consumer is allowed to be active at a time. A second consumer started while another
  is still running fails immediately with `StreamConsumerError::ActiveConsumer`. Reach for this when you only ever read
  the stream from one place at a time. It turns accidental fanout into a loud error at the point of mistake. Once a
  consumer is dropped again, a new one can be registered. Just never two at any given time. This implementation may be
  slightly more performant.
- `discard()`: No consumption at all. The matching child stdio slot is set to `Stdio::null()`, so the OS drops the
  bytes; no pipe is allocated and no reader task runs in the parent. The remaining axes (delivery, replay, buffering)
  are skipped because they have nothing to act on. Reach for this when only the exit status matters; pair it with a
  consumed stream on the other slot if you still want one stream's output (`.stdout(|s| s.discard()).stderr(|s|
  s.broadcast()...)` is fine).

**Delivery: what happens when a consumer can't keep up?**

- `reliable_for_active_subscribers()`: When an active consumer's buffer fills, the child is paused (its next write
  blocks) instead of dropping data. The guarantee is scoped to consumers that are *already attached*: a consumer that
  arrives after the child has produced output gets nothing unless you also configure a replay policy. Use when
  completeness of the stream / definitely receiving all output matters more than keeping the child unblocked.
- `best_effort_delivery()`: Slow consumers may observe gaps. Line consumers drop the in-progress partial line and
  resync at the next newline. Use when latency matters more than completeness. This never blocks the child.

**Replay: what does a consumer attached after spawn see?**

- `.no_replay()`: Live output only. A consumer that attaches after the child has already printed something will not
  see it.
- `.replay_last_bytes(...)` / `.replay_last_chunks(...)` / `.replay_all()`: The library retains output so a late
  consumer can start from history. The first two cap retention. `.replay_all()` is unbounded and should only be used
  for trusted output volume.

The typical lifecycle for a long-running service is: spawn with a replay policy, attach any consumers you want, then
call `seal_output_replay()` to release the retained history. The seal applies to *future* consumers: They start at live
output. Consumers that were already attached when the seal happens are unaffected, and whatever the delivery policy
guaranteed them is still there to read. The per-stream variants `seal_stdout_replay()` and `seal_stderr_replay()` cover
the case where one stream's startup phase ends before the other's.

**Collection: how much memory does the library hold?**

Use bounded `LineCollectionOptions` / `RawCollectionOptions` for untrusted output. Reserve `TrustedUnbounded` for
processes whose output volume is known and controlled. The bounded variants take a `CollectionOverflowBehavior` that
decides what to keep when the cap is reached: `DropAdditionalData` (default) keeps the first-retained output and
discards anything that arrives after the cap is hit, while `DropOldestData` keeps the newest output by evicting older
retained data, which gives you a ring-buffer-shaped tail.

**Buffering: how big a read, how deep a queue?**

Every chain ends with two buffering knobs:

- `.read_chunk_size(...)` is the size of each `read()` against the child's pipe. Smaller values let a consumer see
  partial results from large bursts sooner and cap memory. Larger values reduce per-burst syscall and per-chunk
  overhead.
- `.max_buffered_chunks(...)` is the depth of the in-process queue between the reader task and the consumer. Larger
  values absorb more bursty output before backpressure or dropping kicks in (depending on the delivery policy). Smaller
  values cap memory.

Pass `DEFAULT_READ_CHUNK_SIZE` and `DEFAULT_MAX_BUFFERED_CHUNKS` unless you have measured a reason to tune them. The
defaults (16 KB and 128 chunks) are tuned for general-purpose streaming. Both knobs must be named at the spawn site:
the spelled-out values keep the choice visible in code review and stop accidental mismatches between sites that all
silently rely on a default.

The async line-aware consumers (`inspect_lines_async`, `collect_lines_async`, `collect_lines_into_write`, and friends
that take an `AsyncLineSink`) materialize every parsed line as an owned `String` before invoking the per-line callback.
The synchronous line consumers can pass the line as `Cow::Borrowed` straight out of the chunk on the fast path, so they
avoid the allocation. Prefer the synchronous flavor when per-line work is non-blocking; reach for the async flavor only
when the per-line callback genuinely needs to `.await`.

### Consumers

tokio-process-tools provides the following consumers out of the box:

- `inspect_lines` / `inspect_chunks` (and their `_async` variants) run a callback per item and discard the data.
  Reach for these when only the side effect matters: forward to `tracing`, count occurrences, react to a milestone.
- `collect_lines` / `collect_chunks` (and `_async` variants) retain output in memory for later inspection. Reach for
  these when a test or caller wants to assert on what the child wrote *after* it has finished and
  `wait_for_completion_with_output` doesn't fit.
- `collect_lines_into_write` / `collect_chunks_into_write` (plus `_mapped` variants) forward output to an async
  writer (a file, a TCP sink, anything `AsyncWrite`). `WriteCollectionOptions` controls writer-failure handling and
  `LineWriteMode` controls whether parsed lines get the stripped `\n` reattached. See
  [Stream output to a sink](#stream-output-to-a-sink) for an end-to-end example.

See the [Custom consumer / stream visitor](#custom-consumer--stream-visitor) example for guidance on how to build your
own consumers.

### Consumer lifecycle

Each `Consumer<S>` handle **owns the spawned task**. Dropping the handle drops the task, which cancels the consumer
immediately. Bind it to a variable, e.g. `let _consumer = process.stdout().inspect_line(...)`, to silence the
unused-result warning (if you don't intend to interact with the handle). The `#[must_use]` annotations on every
consumer-creator is the compiler's reminder of this contract. If your process is long-lived, make sure to store consumer
handles somewhere, so that they are not dropped.

A `Consumer` exposes three ways to finish. The choice is a trade-off between recovering state (the writer, the
collected output) and not hanging:

- `wait()` lets the consumer drain naturally. It returns when the stream closes (the child exited and any retained
  replay has been read out) or the visitor returned `Next::Break`. Use this when there is a natural endpoint, and you
  want whatever the consumer produced; this is the path that gives you the writer back from a `collect_*_into_write`
  consumer.
- `cancel(timeout)` is the bounded cooperative-then-forceful shutdown. The consumer task is signaled to stop at the
  next safe point. If it observes the cancellation and exits before `timeout` elapses, you get
  `ConsumerCancelOutcome::Cancelled(sink)` with the sink preserved. If `timeout` elapses first, the task is aborted,
  and you get `ConsumerCancelOutcome::Aborted` with the sink dropped. Pick the timeout based on how long the
  consumer might legitimately need to flush.
- `abort()` is unconditional forceful shutdown. The task's in-flight future is dropped, which means sinks and
  writers are *not* returned (you cannot recover the file handle, the partially-collected `Vec`, etc.). Reach for
  this when not getting stuck matters more than recovering state, and you do not want to pay any timeout at all.

## Examples

Patterns past the basic spawn-and-wait shown in the [Quickstart](#quickstart).

### Wait and collect output

Like the Quickstart spawn, but the stdout/stderr that appeared during the run is captured and returned. The collector
here is attached inside the wait helper, after the child has already started producing output, which is why the stream
is configured with `replay_last_bytes(...)`. Without that policy, anything the child wrote before the helper attached
the collector would be lost (see [Core model](#core-model)). The wait timeout covers both process exit and the final
drain of stdout/stderr.

```rust,no_run
use std::time::Duration;
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
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let line_collection_options = LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    };
    
    let ProcessOutput {
        status,
        stdout,
        stderr,
    } = process
        .wait_for_completion_with_output(
            Duration::from_secs(30),
            LineOutputOptions {
                line_parsing_options: LineParsingOptions::default(),
                stdout_collection_options: line_collection_options,
                stderr_collection_options: line_collection_options,
            },
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("exit status: {status:?}");
    println!("stdout lines: {:?}", stdout.lines());
    println!("stderr lines: {:?}", stderr.lines());
}
```

Use `wait_for_completion_with_raw_output()` when the child's output is not UTF-8 line-oriented: binary blobs (image
conversion, archive tools), framed protocols, or anything where line parsing would corrupt or lose bytes. The shape of
the call is the same; substitute `RawOutputOptions` for `LineOutputOptions`, `RawCollectionOptions` for
`LineCollectionOptions`, and read the result as `CollectedBytes { bytes, truncated }` on each stream. No
`LineParsingOptions` is involved. `RawCollectionOptions` offers the same `Bounded { max_bytes, overflow_behavior }`
and `TrustedUnbounded` variants as the line equivalent.

### Wait for readiness from output

The classic "start a service in an integration test and wait until it logs that it is ready" pattern. `wait_for_line()`
is itself a stream consumer with a required timeout, so under `single_subscriber()` it competes with anything else
reading the stream. Reach for `broadcast()` whenever readiness detection has to coexist with a logger. Pair it with
replay retention so a waiter attached after spawn can still see startup lines, then `seal_output_replay()` to free that
history once the service is up.

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
        .wait_for_line(
            Duration::from_secs(30),
            |line| line.contains("Server listening on"),
            LineParsingOptions::default(),
        )
        .await
    {
        Ok(WaitForLineResult::Matched) => {}
        Ok(WaitForLineResult::StreamClosed) => panic!("server exited before becoming ready"),
        Ok(WaitForLineResult::Timeout) => panic!("server did not become ready in time"),
        Err(err) => panic!("failed to read stdout: {err}"),
    }

    process.seal_output_replay();

    #[cfg(unix)]
    let timeouts = GracefulTimeouts {
        interrupt_timeout: Duration::from_secs(3),
        terminate_timeout: Duration::from_secs(5),
    };
    #[cfg(windows)]
    let timeouts = GracefulTimeouts {
        graceful_timeout: Duration::from_secs(8),
    };

    let _ = process.terminate(timeouts).await;
}
```

### Interactive stdin

Children always have stdin piped (there is no opt-out), but the library only flushes/closes the pipe when you ask.
Calling `stdin().close()` lets a child observe EOF before any wait helper runs (some programs only finish on EOF, not
on idle). Terminal waits and `kill()` close any still-open stdin automatically, so the manual close is only needed
when the child must see EOF *before* the wait.

`process.stdin()` returns a `&mut Stdin`, an enum with `Open(ChildStdin)` and `Closed` variants. `Stdin::as_mut()`
projects that into `Option<&mut ChildStdin>`, returning `None` once the slot has transitioned to `Closed` (either
explicitly via `stdin().close()` or implicitly because a wait helper or `kill()` has already cleaned it up). The
example below pattern-matches with `if let Some(stdin) = process.stdin().as_mut()` for that reason. The `None` branch
has no other failure mode in the stdin-is-always-piped contract.

```rust,no_run
use std::time::Duration;
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
            Duration::from_secs(30),
            LineOutputOptions {
                line_parsing_options: LineParsingOptions::default(),
                stdout_collection_options: line_collection,
                stderr_collection_options: line_collection,
            },
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    println!("stdout lines: {:?}", output.stdout.lines());
}
```

### Stream output to a sink

When the child will produce more output than is reasonable to hold in memory (build tools, long-running servers in CI
logs, or anything you want on disk rather than in `Vec<String>`), point a collector at an `AsyncWrite` sink instead.
The example below tees stdout into a file. Parsed lines are written one per call, with `LineWriteMode::AppendLf`
re-attaching the trailing newline that line parsing strips. `WriteCollectionOptions::log_and_continue()` keeps
collection running if the file write ever fails (disk full, broken pipe to a piped writer, etc.); use `fail_fast()`
instead when a sink failure should stop the collection.

The collector is yours to drive. After the child exits the stream closes, the visitor drains, and `wait()` returns
the file handle so you can flush or close it deterministically.

This example needs the `fs` Tokio feature in your own crate, in addition to the features listed in the
[Quickstart](#quickstart).

```rust,no_run
use std::time::Duration;
use tokio::fs::File;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("my-noisy-tool"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_for_active_subscribers()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("failed to spawn command");

    let log_file = File::create("/tmp/output.log")
        .await
        .expect("failed to open log file");

    let log_collector = process.stdout().collect_lines_into_write(
        log_file,
        LineParsingOptions::default(),
        LineWriteMode::AppendLf,
        WriteCollectionOptions::log_and_continue(),
    );

    let _status = process
        .wait_for_completion(Duration::from_secs(60))
        .await
        .unwrap()
        .expect_completed("process should complete");

    // Stream is closed now that the child has exited; the collector drains. The outer
    // `Result` reflects consumer-infrastructure outcomes (task join, stream read), the
    // inner one reflects whether the writer accepted every byte; with
    // `log_and_continue()` write failures are logged so the inner result is `Ok` here.
    let _log_file = log_collector.wait().await.unwrap().unwrap();
}
```

### Timeout and Cleanup

For processes that may hang, `wait_for_completion_or_terminate()` rolls the "interrupt → terminate → kill"
escalation into the wait so you don't have to sequence cleanup yourself. The three durations correspond to: how long
to wait for a clean exit, how long to give the child after the interrupt signal, and how long to give it after the
terminate signal before falling back to `kill()`.

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

    #[cfg(unix)]
    let graceful_timeouts = GracefulTimeouts {
        interrupt_timeout: Duration::from_secs(3),
        terminate_timeout: Duration::from_secs(5),
    };
    #[cfg(windows)]
    let graceful_timeouts = GracefulTimeouts {
        graceful_timeout: Duration::from_secs(8),
    };

    match process
        .wait_for_completion_or_terminate(WaitForCompletionOrTerminateOptions {
            wait_timeout: Duration::from_secs(30),
            graceful_timeouts,
        })
        .await
    {
        Ok(WaitForCompletionOrTerminateResult::Completed(status)) => {
            println!("completed with status: {status:?}");
        }
        Ok(WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
            result,
            timeout,
        }) => {
            println!("terminated after waiting {timeout:?}; status: {result:?}");
        }
        Err(err) => eprintln!("wait or cleanup failed: {err}"),
    }
}
```

### Custom consumer / stream visitor

When the built-in `inspect_*` and `collect_*` consumers don't fit, implement [`StreamVisitor`] (sync) or
[`AsyncStreamVisitor`] (async) directly and attach it with `consume_with` / `consume_with_async`. The cases that pay
for a custom visitor are: maintaining state across chunks, reacting explicitly to gap notifications, or producing an
output type the built-ins don't expose. If all you need is a side effect per chunk or per line, reach for
`inspect_chunks` / `inspect_lines`; the closure form is simpler.

Pick `StreamVisitor` when `on_chunk` can return without `.await`-ing (counters, in-memory state machines, channel
`try_send`, simple synchronous I/O). Pick `AsyncStreamVisitor` when chunk handling needs to await (network sends,
async writers, channel `send` with backpressure). Both traits require `Send + 'static`, and both return their final
value via `into_output(self)` at the end of the consumer task's life.

The example below tracks how many bytes and chunks the stream produced, plus how many gap events the backend reported.
`on_gap` is invoked when one or more chunks were dropped between the previous and next observed chunk (only possible
under `best_effort_delivery()` overflow), and visitors that splice bytes across chunks should treat it as a reset
signal.

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[derive(Debug, Default)]
struct ChunkStats {
    bytes: usize,
    chunks: usize,
    gaps: usize,
}

impl StreamVisitor for ChunkStats {
    type Output = ChunkStats;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        self.bytes += chunk.as_ref().len();
        self.chunks += 1;
        Next::Continue
    }

    fn on_gap(&mut self) {
        self.gaps += 1;
    }

    fn into_output(self) -> ChunkStats {
        self
    }
}

#[tokio::main]
async fn main() {
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

    let counter = process
        .stdout()
        .consume_with(ChunkStats::default())
        .expect("no other consumer is attached yet");

    let _status = process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");

    let stats = counter.wait().await.unwrap();
    println!(
        "stdout: {} bytes across {} chunks ({} gaps)",
        stats.bytes, stats.chunks, stats.gaps,
    );
}
```

A few lifecycle quirks worth knowing. `on_gap()` is synchronous in both traits because gap notifications carry no
data to await on. `on_eof()` is skipped when `on_chunk` returned `Next::Break`, on the assumption that an
early-break visitor already has its result; cancellation and abort skip `on_eof` for the same reason. Whichever exit
path was taken, `into_output` always runs, so `Consumer::wait()` and `cancel()` always have a value to yield.

### Termination timeouts

`terminate(...)` always takes a single [`GracefulTimeouts`] argument; the signature is identical
on every supported platform. `GracefulTimeouts` itself carries platform-conditional fields,
because the underlying graceful-shutdown model is platform-conditional:

- On Unix it carries `interrupt_timeout` and `terminate_timeout`, matching the 3-phase
  `SIGINT` -> `SIGTERM` -> `SIGKILL` escalation.
- On Windows it carries a single `graceful_timeout`, matching the 2-phase
  `CTRL_BREAK_EVENT` -> `TerminateProcess` shutdown.

Cross-platform callers construct the value under cfg gates and then pass it to the
cross-platform `terminate(...)` and `wait_for_completion_or_terminate(...)` signatures:

```rust,ignore
use std::time::Duration;
use tokio_process_tools::GracefulTimeouts;

#[cfg(unix)]
let timeouts = GracefulTimeouts {
    interrupt_timeout: Duration::from_secs(3),
    terminate_timeout: Duration::from_secs(5),
};
#[cfg(windows)]
let timeouts = GracefulTimeouts {
    graceful_timeout: Duration::from_secs(8),
};

process.terminate(timeouts).await?;
```

Each user-supplied graceful timeout bounds the post-signal wait of its phase:

| Scenario per phase                        | Time spent in this phase                |
|-------------------------------------------|-----------------------------------------|
| Signal send succeeds, child exits         | ≤ user timeout (typically much less)    |
| Signal send succeeds, child does NOT exit | exactly user timeout, then escalate     |
| Signal send fails                         | up to 100 ms fixed grace, then escalate |

The 100 ms grace covers the small window where the child has already exited but Tokio's SIGCHLD reaper has not yet
observed it (`EPERM` on macOS, `ESRCH` on Linux against a not-yet-reaped process group). It replaces - never adds to -
the user timeout for a failed phase. Real permission denials still surface as a `TerminationError`.

`Duration::from_secs(0)` disables the post-signal wait for that phase. The graceful signal is still sent; the call
just collapses to "send the signal, do not wait, escalate immediately". On Unix that means each zero-duration phase
sends its signal (`SIGINT`, then `SIGTERM`) and rolls straight on to the next, eventually arriving at `SIGKILL`. On
Windows there is only the one phase, so a zero `graceful_timeout` reduces `terminate(...)` to "send
`CTRL_BREAK_EVENT`, then immediately `TerminateProcess`". Prefer small but non-zero values (e.g. 100 ms to a few
seconds) so well-behaved children can actually run their handlers.

The termination machinery `terminate(...)`, `terminate_on_drop(...)`, `wait_for_completion_or_terminate(...)`,
`WaitForCompletionOrTerminateOptions` and `GracefulTimeouts` is only available on Unix and Windows. On other targets
the spawn, wait, output-collection APIs and kill remain available. Graceful-termination control is not made available
until platform specific signals were implemented.

### When termination fails

If `terminate()` returns `Err`, the panic-on-drop guard stays armed: the library cannot verify cleanup from the
outside, so dropping would leak a process. Recover by retrying `terminate()`, escalating to `kill()`, calling
`must_not_be_terminated()` to accept the failure, or propagating the error and letting the panic-on-drop surface the
leak.

## Further APIs

The Quickstart and Examples cover the common path. The rest of the surface is small but purposeful. Here is when you
would reach for each piece.

### Wait helper variants

`wait_for_completion_with_output()` and friends come in four combinations: line vs. raw bytes, and plain wait vs.
wait-or-terminate. Pick the variant that matches what you need:

- Line vs. raw. Line collection parses on the fly and gives you `Vec<String>`-style output; raw collection gives you
  the bytes the child actually wrote, which matters when output isn't UTF-8 line-oriented.
- Plain vs. `_or_terminate`. The `_or_terminate` variants embed the same interrupt → terminate → kill escalation
  shown in [Timeout and Cleanup](#timeout-and-cleanup), so a hung child still gets cleaned up before the helper
  returns.

Each plain wait helper returns a `WaitForCompletionResult` (`Completed` or `Timeout`); the `_or_terminate` variants
return `WaitForCompletionOrTerminateResult` (`Completed` or `TerminatedAfterTimeout`). The examples above use
`.expect_completed(msg)` to panic on the non-completed branch when a timeout would be a hard failure; reach for
`.into_completed()` instead when the caller should handle both outcomes itself.

When you need output collection *and* hung-child cleanup together, reach for the combined
`wait_for_completion_with_output_or_terminate()` (or its `_with_raw_output_or_terminate()` sibling). It takes the
same `LineOutputOptions` / `RawOutputOptions` you would pass to `_with_output`, plus the three durations you would
pass to `_or_terminate`, with no extra ceremony.

For trusted, bounded-by-construction output, both `LineCollectionOptions` and `RawCollectionOptions` have a
`TrustedUnbounded` variant that opts out of memory caps.

### Line parsing

`LineParsingOptions` controls how chunks are split into lines and what happens at the limits.
`wait_for_line()` takes the same `LineParsingOptions` so its matcher sees the same lines a collector would. Three
fields:

- `max_line_length` (default 16 KB) caps a single line. It must be greater than zero; pass `NumBytes::MAX` explicitly
  to opt into unbounded line parsing on a trusted source. `LineOverflowBehavior` decides what happens past the cap:
  `DropAdditionalData` discards bytes until the next newline (default), and `EmitAdditionalAsNewLines` splits the
  long line into successive emitted lines so no data is lost.
- `buffer_compaction_threshold` (default `None`) is a steady-state memory knob. The parser's two internal buffers
  retain capacity for whichever was the largest line they have ever held; setting `Some(n)` forces buffers above `n`
  to be released between lines. Useful with mostly-small lines and occasional outliers (especially under
  `NumBytes::MAX`), counterproductive when typical lines are already near the threshold.

When chunks drop under `best_effort_delivery()`, line parsers conservatively discard the in-progress partial line and
resynchronize at the next newline rather than splicing across the gap. Callers should not assume that a missing
newline after a gap means "the next line continues the previous one"; treat each post-gap line as fresh.

### Manual process control

When the wait-and-cleanup helpers do not fit, drive shutdown by hand: `send_interrupt_signal()` and
`send_terminate_signal()` on Unix, `send_ctrl_break_signal()` on Windows, `terminate()` (escalation up to kill), and
`kill()` (immediate). Each returns a typed `TerminationError` describing which step failed. The signal-send methods
are idempotent against a child that has already exited: they reap the status and return `Ok(())` without sending a
signal at a possibly-recycled PID. `seal_stdout_replay()`, `seal_stderr_replay()`, and `seal_output_replay()` end
replay retention on one stream or both.

## Process management

### Process naming

A process name is required before stdout/stderr can be configured. The name appears in error messages and tracing
spans, and the library refuses to spawn anonymous processes because anonymous failures are hard to debug.

For production workflows, prefer a stable explicit label (`"api-worker"`, `format!("agent-{id}")`) so logs and
metrics group cleanly. The `AutoName` variants are convenient for ad-hoc and test code, but the variants that
include arguments, environment, or current directory will surface those values in error messages, so only use them
when those values are safe to log.

Canonical naming forms:

- **Explicit label:** `.name("api-worker")` or `.name(format!("agent-{id}"))`. The recommended default.
- **Safe automatic:** `.name(AutoName::program_only())`. Uses just the program name; no arguments, environment, or
  path are leaked into errors.
- **Detailed automatic:** `.name(AutoName::program_with_args())`, `.name(AutoName::program_with_env_and_args())`,
  `.name(AutoName::full())` (cwd + env + program + args), or `.name(AutoName::Debug)` for the full `Command` debug
  representation. Useful in tests; check the redaction story before using in production.
- **Custom automatic:** `.name(AutoNameSettings::builder()...build())` to opt into a specific subset of fields.

```rust,no_run
use std::time::Duration;
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

    process
        .wait_for_completion(Duration::from_secs(30))
        .await
        .unwrap()
        .expect_completed("process should complete");
}
```

### Automatic termination on drop

A live `ProcessHandle` that is dropped without a successful terminal wait, `terminate()`, or `kill()` runs best-effort
cleanup and then **panics**. The panic is deliberate: a silent leak of a child process is much harder to detect than a
loud panic in tests, and most "I forgot to wait" bugs would otherwise survive into production.

For long-lived processes that follow a service's own lifecycle, opting in to `.terminate_on_drop(...)` swaps the panic
for an async termination attempt during drop. The signature mirrors `terminate(...)`: two timeouts on Unix, one on
Windows.

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::*;

#[tokio::main]
async fn main() {
    #[cfg(unix)]
    let timeouts = GracefulTimeouts {
        interrupt_timeout: Duration::from_secs(3),
        terminate_timeout: Duration::from_secs(5),
    };
    #[cfg(windows)]
    let timeouts = GracefulTimeouts {
        graceful_timeout: Duration::from_secs(8),
    };

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
        .terminate_on_drop(timeouts);
}
```

`terminate_on_drop` requires a multithreaded Tokio runtime, because drop runs the termination attempt on Tokio's
blocking pool. If the attempt itself fails, the inner handle still falls back to the panic path so the failure is not
silent. Under `#[tokio::test]`, this means opting in to the multi-thread flavor:

```rust,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
}
```

When the child genuinely needs to outlive the handle (e.g. a daemon you spawned intentionally), call
`must_not_be_terminated()`. That suppresses both the kill and the panic on drop. The library still drops its owned
stdio, however, so the child loses access to its piped stdin and the parent loses access to its stdout/stderr at the
same moment.

To keep the child *and* its `Stdin`/`Stdout`/`Stderr` together, use `ProcessHandle::into_inner()` to take ownership of
all four.

## Platform support

The termination escalation is per-platform. Both platforms try a graceful step first (so the child can flush, close
sockets, persist state) and only escalate to an unconditional kill if the child stays alive. Pick the timeouts you
pass to `terminate()` and the `_or_terminate` helpers based on how long your child legitimately needs at each step.
Each step targets the child's process group rather than its PID alone, so grandchildren are signaled with the
leader; see [Subprocess trees](#subprocess-trees) for the spawn-time setup that makes this work.

- **Linux/macOS:** 3-phase escalation. `SIGINT` → `SIGTERM` → `SIGKILL`, all delivered to the child's process group
  via `killpg`. The two distinct graceful signals matter for real children: idiomatic async Rust binaries use
  `tokio::signal::ctrl_c()` (which on Unix listens only for `SIGINT`), and Python child processes turn `SIGINT` into
  a `KeyboardInterrupt` exception that runs `try/finally` cleanup, while `SIGTERM` falls through to the runtime's
  default handler. `GracefulTimeouts` therefore exposes two `Duration`s on Unix
  (`interrupt_timeout`, `terminate_timeout`).
- **Windows:** 2-phase shutdown. `CTRL_BREAK_EVENT` to the console process group → `TerminateProcess` on the leader
  as the kill fallback. Exactly one `CTRL_BREAK_EVENT` is sent. `GracefulTimeouts` therefore exposes a single
  `graceful_timeout` on Windows.
- **Other platforms:** the spawn, wait, output-collection, and `kill(...)` APIs remain available - `kill(...)`
  forwards to Tokio's `Child::start_kill()` and works on every Tokio-supported target. Only the graceful-termination
  machinery (`terminate(...)`, `terminate_on_drop(...)`, `wait_for_completion_or_terminate(...)`,
  `WaitForCompletionOrTerminateOptions`, `GracefulTimeouts`, and `send_*_signal(...)`) is gated out, because there is
  no OS primitive to deliver a graceful signal against. Best-effort cleanup on drop still goes through
  `Child::start_kill()` too, so unawaited handles do not leak the child.

The Windows path uses `CTRL_BREAK_EVENT` rather than `CTRL_C_EVENT` because `GenerateConsoleCtrlEvent` accepts only
`CTRL_BREAK_EVENT` for nonzero process groups. `CTRL_C_EVENT` requires `dwProcessGroupId = 0`, which broadcasts to
the entire console (including this parent process), so it is not usable for targeting a single child group. There is
no separate `SIGINT` vs. `SIGTERM` distinction on Windows, so the manual signal-send surface is split per platform:
`send_interrupt_signal()` and `send_terminate_signal()` exist only on Unix, and Windows exposes a single
`send_ctrl_break_signal()`. The library never delivers two graceful events back to back during `terminate()`.

> **Windows interop note.** `tokio::signal::ctrl_c()` on Windows registers only for `CTRL_C_EVENT`; it does not catch
> `CTRL_BREAK_EVENT`. A child Rust binary that listens only on the cross-platform `tokio::signal::ctrl_c()` will not
> respond to this library's graceful step on Windows and will be terminated forcefully after `graceful_timeout`. To
> interoperate, such a child should additionally listen on `tokio::signal::windows::ctrl_break()` (or use another
> shutdown channel like a stdin sentinel, IPC, or a command protocol).

A child that handles neither signal at all on either platform still needs its own graceful shutdown channel (stdin,
IPC, or a command protocol) to participate in cooperative shutdown.

Signal sends (`send_interrupt_signal` and `send_terminate_signal` on Unix, `send_ctrl_break_signal` on Windows, and
the equivalent steps inside `terminate()`) check for an already-exited child first and return `Ok(())` without sending
a signal in that case, so calling them after the child has died is harmless rather than racy.

## Subprocess trees

Every spawned child is set up as the leader of its own process group, so termination signals reach the whole subtree
rather than only the child the library spawned. If your child fork-execs further processes (an `npm start` that
launches `node`, a shell wrapper that launches the real binary, a build tool that fans out workers), the grandchildren
inherit the group and are signaled along with the leader by `send_interrupt_signal()` / `send_terminate_signal()` on
Unix, `send_ctrl_break_signal()` on Windows, `terminate()`, and `kill()`.

- **Linux/macOS:** the child is spawned with `process_group(0)`, making its PID equal to its PGID. `SIGINT`, `SIGTERM`,
  and `SIGKILL` are delivered with `killpg`, which targets the whole group.
- **Windows:** the child is spawned with `CREATE_NEW_PROCESS_GROUP`. `CTRL_BREAK_EVENT` is delivered to that console
  process group via `GenerateConsoleCtrlEvent`. The forceful-kill fallback (`TerminateProcess`) still targets the
  leader, so a graceful interrupt window is the right place to let cooperative children unwind.

A grandchild that explicitly detaches itself (`setsid`/`setpgid` to leave the group, double-fork into init, or its own
job-object on Windows) is by definition outside the group and will not be signaled. That is correct: such children
have asked to outlive their parent. If you do not own the leaf, and it leaks descendants of its own, run it under a
small reaper (`tini` is the usual choice) so the descendants have a designated owner.

## MSRV

MSRV is `1.89.0`.

## Contributing

Contributions are welcome!

1. Create a feature branch.
2. Run `just verify` on your changes.
3. Don't forget to add a CHANGELOG entry under the `# [Unreleased]` section.
4. Open a pull request describing the behavior change and any relevant README updates.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
