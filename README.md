# tokio-process-tools

A correctness-focused async subprocess library for Tokio. It wraps
`tokio::process::Command` and exposes explicit controls over output streaming, readiness
detection, stdin, termination, and cleanup.

Most subprocess code in the wild leaves a handful of decisions implicit — what happens
when a log consumer falls behind, whether a readiness check attached after spawn can
still see startup output, how much memory the library will hold on your behalf, what
happens to the child if the parent panics or times out. Each of those defaults is fine
in isolation and dangerous in aggregate: silent gaps, leaked children, unbounded
buffers.

`tokio-process-tools` makes every one of those choices a required argument at the call
site. There is no default delivery policy, no default replay policy, no default
collection bound. The cost is a more verbose spawn; the benefit is that the choice is
visible to code review and to whoever debugs the system at 3am.

## When to use this

Reach for it when you need any of:

- an integration test that starts a real service and waits until it is ready,
- a build or development tool that streams live output and keeps a bounded tail for failure reports,
- an orchestration layer that must not leak children after failures or timeouts,
- one stream consumed by more than one subsystem (e.g. logging plus readiness checks),
- bounded memory in the face of untrusted or unknown output volume.

If `tokio::process::Command` already gives you enough control, this crate is overkill.

## Core model

The library has three layers, and most of the configuration in this README is about the
relationship between them:

```text
   Process ──spawn──▶ ProcessHandle
                         │
                  ┌──────┴──────┐
              stdout         stderr        ← each is an output stream
                  │             │
            ┌─────┼─────┐ ┌─────┼─────┐
         consumer …  consumer …       ← inspectors, collectors, line waiters
```

- A **`Process`** is a spawn configuration. `Process::new(command)` starts the chain;
  `.spawn()` returns a `ProcessHandle`.
- An **output stream** is the buffered pipeline that sits between a child's stdout (or
  stderr) and your code. Each stream is configured at spawn time — see
  [Choosing stream settings](#choosing-stream-settings) — and emits raw chunks, which
  line consumers parse on the fly.
- A **consumer** is anything that reads from a stream: an inspector that runs a
  callback, a collector that retains output for later, a `wait_for_line()` that resolves
  on a match.

The two timelines matter and don't always line up. Stream configuration is fixed at
spawn time — backend, delivery, replay, collection, buffering — and the reader task
starts producing output the moment the child does. Consumers, by contrast, attach
*afterwards*, and a consumer that arrives even a few milliseconds late can miss the
output that has already gone past. That gap is exactly what the replay policy on a
stream covers, and it is the reason a number of choices in this README are about
"what does a consumer attached after spawn see?" rather than "how do I read the
stream?".

A `ProcessHandle` is "armed" until it is successfully waited, terminated, killed, or
explicitly excused with `must_not_be_terminated()`. Dropping an armed handle without one
of those calls runs best-effort cleanup and then panics — better to be loud than to
silently leak children. See [Automatic termination on drop](#automatic-termination-on-drop)
for the opt-out.

## Quickstart

Add to your `Cargo.toml`:

```toml
[dependencies]
tokio-process-tools = "0.9.0"
tokio = { version = "1", features = ["macros", "process", "rt-multi-thread"] }
```

The interactive-stdin example below uses `tokio::io::AsyncWriteExt`, which requires the `io-util`
Tokio feature in your own crate.

## Choosing stream settings

Each stdout/stderr stream is configured along five axes — backend, delivery, replay,
collection, and buffering. The axes are not independent: a `single_subscriber()` stream
that you want to read from concurrently is a contradiction; `reliable_for_active_subscribers()`
without a replay policy gives a late consumer nothing; an unbounded collector on a
`broadcast()` stream is the easiest way to leak memory. The descriptions below introduce
each axis in turn, and the [Examples](#examples) show common combinations end-to-end.

**Backend — who consumes the stream?**

- `single_subscriber()` — exactly one consumer is allowed to be active at a time. If a
  second consumer (inspector, collector, or `wait_for_line()`) is started while another
  is still running, the second one fails immediately with
  `StreamConsumerError::ActiveConsumer`. This is the right default when you only ever
  read the stream from one place — it turns accidental fanout (a pattern that would
  silently work-then-break later) into a loud error at the point of mistake.
- `broadcast()` — multiple concurrent consumers, e.g. logging plus a readiness wait.

**Delivery — what happens when a consumer can't keep up?**

- `best_effort_delivery()` — slow consumers may observe gaps; line consumers drop the
  in-progress partial line and resync at the next newline. Use when bounded live latency
  matters more than completeness.
- `reliable_for_active_subscribers()` — when an active consumer's buffer fills, the
  child is paused (its next write blocks) instead of dropping data. The guarantee is
  scoped to consumers that are *already attached*: a consumer that arrives after the
  child has produced output gets nothing unless you also configure a replay policy. Use
  this when completeness of the stream matters more than keeping the child unblocked.

**Replay — what does a consumer attached after spawn see?**

- `.no_replay()` — live output only. A consumer that attaches after the child has
  already printed something will not see it.
- `.replay_last_bytes(...)` / `.replay_last_chunks(...)` / `.replay_all()` — the library
  retains output so a late consumer can start from history. The first two cap retention;
  `.replay_all()` is unbounded and should only be used for trusted output volume.

The typical lifecycle for a long-running service is: spawn with a replay policy, attach
a `wait_for_line()` (and any logger you want), let it match, then call
`seal_output_replay()` to release the retained history. The seal applies to *future*
consumers — they will start at live output. Consumers that were already attached when
the seal happened are unaffected; whatever the delivery policy guaranteed them is still
there to read.

The per-stream variants `seal_stdout_replay()` and `seal_stderr_replay()` exist for the
case where one stream's startup phase ends before the other's.

**Collection — how much memory does the library hold?**

Use bounded `LineCollectionOptions` / `RawCollectionOptions` for untrusted output. Reserve
`TrustedUnbounded` for processes whose output volume is known and controlled.

**Buffering — chunk read size and pipeline depth**

Every chain ends with two buffering knobs:

- `.read_chunk_size(...)` is the size of each `read()` against the child's pipe. Larger
  values reduce syscall overhead; smaller values reduce latency between the child
  writing a byte and a consumer seeing it.
- `.max_buffered_chunks(...)` is the depth of the in-process queue between the reader
  task and the consumer. Larger values absorb more bursty output before backpressure or
  dropping kicks in (depending on the delivery policy); smaller values cap memory.

Pass `DEFAULT_READ_CHUNK_SIZE` and `DEFAULT_MAX_BUFFERED_CHUNKS` unless you have measured
a reason to tune them — the defaults are tuned for general-purpose streaming.

---

Byte sizes throughout the API use the `NumBytes` type. The `NumBytesExt` trait (re-exported
at the crate root) lets you write `1.megabytes()`, `16.kilobytes()`, or `512.bytes()` on
integer literals.

## Examples

Examples use `use tokio_process_tools::*;` to keep them focused on subprocess
configuration. In application code, prefer named imports.

### Spawn and wait

The minimum viable spawn: run a command to completion with a deadline and check its
exit status. Output is streamed through but not retained. Use this when you only care
about whether the process succeeded, not what it wrote.

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
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
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

### Wait and collect output

Same as above, but the stdout/stderr that appeared during the run is captured and
returned. The collector here is attached inside the wait helper, after the child has
already started producing output, which is why the stream is configured with
`replay_last_bytes(...)` — without it, anything the child wrote before the helper got
to attach the collector would be lost (see [Core model](#core-model)). The wait timeout
covers both process exit and the final drain of stdout/stderr.

```rust,no_run
use std::time::Duration;
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

    println!("exit status: {status:?}");
    println!("stdout lines: {:?}", stdout.lines());
    println!("stderr lines: {:?}", stderr.lines());
}
```

Use `wait_for_completion_with_raw_output()` when the child's output is not UTF-8
line-oriented — binary blobs (image conversion, archive tools), framed protocols, or
anything where line parsing would corrupt or lose bytes. The shape of the call is the
same; substitute `RawOutputOptions` for `LineOutputOptions`, `RawCollectionOptions` for
`LineCollectionOptions`, and read the result as `CollectedBytes { bytes, truncated }`
on each stream — no `LineParsingOptions` involved. `RawCollectionOptions` offers the
same `Bounded { max_bytes, overflow_behavior }` and `TrustedUnbounded` variants as the
line equivalent.

### Wait for readiness from output

The classic "start a service in an integration test and wait until it logs that it is
ready" pattern. `wait_for_line()` is itself a stream consumer with a required timeout,
so under `single_subscriber()` it competes with anything else reading the stream —
reach for `broadcast()` whenever readiness detection has to coexist with a logger.
Pair it with replay retention so a waiter attached after spawn can still see startup
lines, then `seal_output_replay()` to free that history once the service is up.

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

    let _ = process
        .terminate(Duration::from_secs(3), Duration::from_secs(5))
        .await;
}
```

### Interactive stdin

Children always have stdin piped — there is no opt-out — but the library only
flushes/closes the pipe when you ask. Calling `stdin().close()` lets a child observe
EOF before any wait helper runs (some programs only finish on EOF, not on idle).
Terminal waits and `kill()` close any still-open stdin automatically, so the manual
close is only needed when the child must see EOF *before* the wait.

`process.stdin()` returns a `&mut Stdin`, an enum with `Open(ChildStdin)` and `Closed`
variants. `Stdin::as_mut()` projects that into `Option<&mut ChildStdin>`, returning
`None` once the slot has transitioned to `Closed` (either explicitly via
`stdin().close()` or implicitly because a wait helper or `kill()` has already cleaned
it up). The example below pattern-matches with `if let Some(stdin) = process.stdin().as_mut()`
for that reason — there is no other failure mode for the `None` branch in the
stdin-is-always-piped contract.

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

When the child is going to produce more output than is reasonable to hold in memory —
think build tools, long-running servers in CI logs, or anything you want to keep on disk
rather than in `Vec<String>` — point a collector at an `AsyncWrite` sink instead. The
example below tees stdout into a file: parsed lines are written one per call, with
`LineWriteMode::AppendLf` re-attaching the trailing newline that line parsing strips.
`WriteCollectionOptions::log_and_continue()` keeps collection running if the file write
ever fails (disk full, broken pipe to a piped writer, etc.); use `fail_fast()` instead
when a sink failure should stop the collection. The collector is yours to drive: after
the child exits the stream closes and `wait()` returns the file handle so you can flush
or close it deterministically.

This example needs the `fs` Tokio feature in your own crate, in addition to the features
listed in the [Quickstart](#quickstart).

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

For processes that may hang. `wait_for_completion_or_terminate()` rolls the
"interrupt → terminate → kill" escalation into the wait, so you don't have to sequence
cleanup yourself. The three durations correspond to: how long to wait for a clean exit,
how long to give the child after the interrupt signal, and how long to give it after
the terminate signal before falling back to `kill()`.

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

## Further APIs

The Quickstart and Examples cover the common path. The rest of the surface is small but
purposeful — here is when you would reach for each piece.

### More wait-and-collect helpers

`wait_for_completion_with_output()` and friends come in four combinations: line vs. raw
bytes, and plain wait vs. wait-or-terminate. Pick the variant that matches what you
need:

- Line vs. raw — line collection parses on the fly and gives you `Vec<String>`-style
  output; raw collection gives you the bytes the child actually wrote, which matters
  when output isn't UTF-8 line-oriented.
- Plain vs. `_or_terminate` — the `_or_terminate` variants embed the same
  interrupt → terminate → kill escalation shown in [Timeout and Cleanup](#timeout-and-cleanup),
  so a hung child still gets cleaned up before the helper returns.

For trusted, bounded-by-construction output, both `LineCollectionOptions` and
`RawCollectionOptions` have a `TrustedUnbounded` variant that opts out of memory caps.

### Background consumers (without a wait helper)

The wait helpers attach their own consumers internally and tear them down on return.
When you want a consumer that lives independently of any wait — say, a logger that
keeps running after `wait_for_line()` has resolved, or a sink that should keep
draining for as long as the child is alive — call the consumer methods on the stream
directly:

- `inspect_lines` / `inspect_lines_async` and `inspect_chunks` / `inspect_chunks_async`
  run a callback per item and discard the data. Reach for these when only the side
  effect matters: forward to `tracing`, count occurrences, react to a milestone in the
  log.
- `collect_lines` / `collect_lines_async` and `collect_chunks` / `collect_chunks_async`
  retain output in memory for later inspection. Reach for these when the test or
  caller wants to assert on what the child wrote *after* it has finished and
  `wait_for_completion_with_output` doesn't fit (different timeout, different
  lifecycle, post-mortem on failure).
- `collect_lines_into_write` / `collect_lines_into_write_mapped` and
  `collect_chunks_into_write` / `collect_chunks_into_write_mapped` forward output to
  an async writer — a file, a TCP sink, anything `AsyncWrite`. Reach for these when
  the output should be durable but you don't want to pay for in-memory retention.
  `WriteCollectionOptions` (`fail_fast`, `log_and_continue`, or a custom error
  handler) controls what happens when the writer fails; `LineWriteMode` controls
  whether parsed lines get the stripped trailing `\n` reattached. See
  [Stream output to a sink](#stream-output-to-a-sink) for an end-to-end example.

Each of these returns a `Consumer` handle. Unlike the wait helpers, nothing else will
end them for you — the next subsection is about how to do that.

### Background consumer lifecycle

A `Consumer` exposes four ways to finish, and the choice between them is a trade-off
between recovering the consumer's state (the writer, the collected output) and not
getting stuck:

- `wait()` — let the consumer drain naturally. It returns when the stream closes (the
  child exited and any retained replay has been read out) or the callback returned
  `Next::Stop`. Use this when there is a natural endpoint and you want whatever the
  consumer produced; this is the path that gives you the writer back from a
  `collect_*_into_write` consumer.
- `cancel()` — cooperative shutdown. The consumer task is signalled to stop at the
  next safe point, and sinks and writers are preserved and returned. Safe when the
  callback or writer respects cancellation; if it doesn't, this can hang.
- `abort()` — forceful shutdown. The task's in-flight future is dropped, which means
  sinks and writers are *not* returned (you cannot recover the file handle, the
  partially-collected `Vec`, etc.). Reach for this when not getting stuck matters more
  than recovering state.
- `cancel_or_abort_after(timeout)` — try cooperative, fall back to forceful after the
  timeout. The pragmatic default for "I want to stop but I don't want to hang." Pick
  the timeout based on how long the consumer might legitimately need to flush.

### Line parsing

`LineParsingOptions` and `LineOverflowBehavior` control how chunks are split into lines
and what happens when a single line exceeds the configured limits. `wait_for_line()`
takes the same `LineParsingOptions` so its matcher sees the same lines a collector
would.

### Manual process control

When the wait-and-cleanup helpers do not fit, drive shutdown by hand:
`send_interrupt_signal()`, `send_terminate_signal()`, `terminate()` (escalation up to
kill), and `kill()` (immediate). Each returns a typed `TerminationError` describing
which step failed. `seal_stdout_replay()`, `seal_stderr_replay()`, and
`seal_output_replay()` end replay retention on one stream or both.

## Process management

### Process naming

A process name is required before stdout/stderr can be configured — the name appears in
error messages and tracing spans, and the library refuses to spawn anonymous processes
because anonymous failures are hard to debug.

For production workflows, prefer a stable explicit label (`"api-worker"`,
`format!("agent-{id}")`) so logs and metrics group cleanly. The `AutoName` variants are
convenient for ad-hoc and test code, but the variants that include arguments,
environment, or current directory will surface those values in error messages — only
use them when those values are safe to log.

Canonical naming forms:

- **Explicit label** — `.name("api-worker")` or `.name(format!("agent-{id}"))`. The
  recommended default.
- **Safe automatic** — `.name(AutoName::program_only())`. Uses just the program name;
  no arguments, environment, or path are leaked into errors.
- **Detailed automatic** — `.name(AutoName::program_with_args())`,
  `.name(AutoName::program_with_env_and_args())`, `.name(AutoName::full())` (cwd + env +
  program + args), or `.name(AutoName::Debug)` for the full `Command` debug
  representation. Useful in tests; check the redaction story before using in production.
- **Custom automatic** — `.name(AutoNameSettings::builder()...build())` to opt into a
  specific subset of fields.

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

### Timeout with automatic termination

Use `wait_for_completion_or_terminate()` as shown in [Timeout and Cleanup](#timeout-and-cleanup)
when a timeout should immediately trigger cleanup. If you also need output collection during that cleanup path,
use `wait_for_completion_with_output_or_terminate()` or
`wait_for_completion_with_raw_output_or_terminate()` with bounded or `TrustedUnbounded`
collection options.

### Automatic termination on drop

A live `ProcessHandle` that is dropped without a successful terminal wait,
`terminate()`, or `kill()` runs best-effort cleanup and then **panics**. The panic is
deliberate: a silent leak of a child process is much harder to detect than a loud panic
in tests, and most "I forgot to wait" bugs would otherwise survive into production.

For long-lived processes that follow a service's own lifecycle, opting in to
`.terminate_on_drop(...)` swaps the panic for an async termination attempt during drop:

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

`terminate_on_drop` requires a multithreaded Tokio runtime, because drop runs the
termination attempt on Tokio's blocking pool. If the attempt itself fails, the inner
handle still falls back to the panic path so the failure is not silent. Under
`#[tokio::test]`, this means opting in to the multi-thread flavor:

```rust,no_run
#[tokio::test(flavor = "multi_thread")]
async fn test() {
    // ...
}
```

When the child genuinely needs to outlive the handle (e.g. a daemon you spawned
intentionally), call `must_not_be_terminated()`. That suppresses both the kill and the
panic on drop, but the library still drops its owned stdio. To keep the child *and* its
`Stdin`/`Stdout`/`Stderr` together, use `ProcessHandle::into_inner()` to take ownership
of all four.

## Platform support

The termination escalation is per-platform but follows the same idea: try the gentle
signal first (so the child can flush, close sockets, persist state), then a stronger
signal, then unconditional kill. Pick the timeouts you pass to `terminate()` and the
`_or_terminate` helpers based on how long your child legitimately needs at each step.

- **Linux/macOS** — `SIGINT` → `SIGTERM` → kill.
- **Windows** — `CTRL_BREAK_EVENT` → kill.

The Windows path uses `CTRL_BREAK_EVENT` rather than `CTRL_C_EVENT` because Windows does
not allow targeting `CTRL_C_EVENT` at a nonzero child process group. A child that only
handles Ctrl+C will not respond to either signal, and needs its own graceful shutdown
channel (stdin, IPC, or a command protocol).

## MSRV

MSRV is `1.89.0`.

## Contributing

Contributions are welcome. Tests live next to the module they cover; please update or
add them alongside any behavior change.

1. Create a feature branch.
2. Run `cargo fmt`, `cargo clippy --all-targets --all-features -- -D warnings`, and
   `cargo test`.
3. Open a pull request describing the behavior change and any relevant README updates.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
