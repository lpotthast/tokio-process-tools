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

The library has three layers, and most of the configuration in this README is about the relationship between them:

- A **`Process`** is a spawn configuration. `Process::new(command)` starts the chain, `.spawn()` returns a
  `ProcessHandle`.
- An **output stream** is the buffered pipeline that sits between a child's stdout (or stderr) and your code. Each
  stream is configured at spawn time (see [Choosing stream settings](#choosing-stream-settings)) and emits raw chunks,
  which consumers parse on the fly.
- A **consumer** is anything that reads from a stream: an inspector that runs a callback, a collector that retains
  output for later, a `wait_for_line()` that resolves on a match. Consumers are implemented through a visitor pattern,
  allowing for custom extensions to be written.

Consumers can only attach *after* `spawn()` returns, but the child is already running by then and may have written
output before the first consumer registers. A replay policy is what closes that gap: with replay enabled, every new
subscription is delivered the retained history before live events, so even a consumer attached on the very next line
sees the bytes that landed during the spawn-to-attach window. Without a replay policy the stream does not retain those
early bytes (a synchronously-attached consumer may still see them by luck of scheduling; see the Replay axis below).

A `ProcessHandle` is "armed" until it is successfully waited, terminated, killed, or explicitly excused with
`must_not_be_terminated()`. Dropping an armed handle without one of those calls runs best-effort cleanup and then
panics. Better to be loud than to silently leak children. See
[Automatic termination on drop](#automatic-termination-on-drop) for the opt-out.

## Quickstart

Add `tokio-process-tools` to your `Cargo.toml` and ensure you are using a multithreaded runtime:

```toml
[dependencies]
tokio-process-tools = "0.11.1"
tokio = { version = "1", features = ["macros", "process", "rt-multi-thread"] }
```

Spawn a child and wait for it to exit, collecting its stdout and stderr output as parsed lines while waiting,
terminating the process the after 30s wait timeout gracefully, waiting at max 5s before falling back to an abrupt kill:

```rust,no_run
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::{
    AutoName, CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_OUTPUT_EOF_TIMEOUT,
    DEFAULT_READ_CHUNK_SIZE, GracefulShutdown, LineCollectionOptions, LineOutputOptions, LineParsingOptions,
    NumBytesExt, Process, ProcessOutput,
};

#[tokio::main]
async fn main() {
    let mut process = Process::new(Command::new("ls"))
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .lossy_without_backpressure()
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
        .wait_for_completion(Duration::from_secs(30))
        .with_line_output(
            DEFAULT_OUTPUT_EOF_TIMEOUT,
            LineParsingOptions::default(),
            LineOutputOptions::symmetric(LineCollectionOptions::Bounded {
                max_bytes: 1.megabytes(),
                max_lines: 1024,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            }),
        )
        .or_terminate(
            GracefulShutdown::builder()
                .unix_sigterm(Duration::from_secs(5))
                .windows_ctrl_break(Duration::from_secs(5))
                .build(),
        )
        .await
        .unwrap()
        .expect_completed("process should complete before timeout");

    println!("exit status: {status:?}");
    println!("stdout lines: {:?}", stdout.lines());
    println!("stderr lines: {:?}", stderr.lines());
}
```

The runnable examples under [`examples/`](examples/) cover the common shapes end-to-end. Each example file explains
what it shows. Run any of them with `cargo run --example <name>`:

| Example                                                    | What it shows                                                                                             |
|------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------|
| [`collect_output_lines`](examples/collect_output_lines.rs) | Spawn, wait, collect stdout/stderr as parsed lines. The canonical "spawn + wait + collect" shape.         |
| [`collect_output_bytes`](examples/collect_output_bytes.rs) | Same as above but for binary / non-line-oriented output.                                                  |
| [`custom_visitor`](examples/custom_visitor.rs)             | Implement a custom `StreamVisitor` for requirements not fulfillable with the provided visitors.           |
| [`discard_output`](examples/discard_output.rs)             | Run a child solely for its exit status, or capture one stream while discarding the other at the OS level. |
| [`interactive_stdin`](examples/interactive_stdin.rs)       | Write to a child's stdin, close it to signal EOF, and collect the echoed output.                          |
| [`process_naming`](examples/process_naming.rs)             | Shows different process name settings.                                                                    |
| [`stream_to_file`](examples/stream_to_file.rs)             | Forward output to an `AsyncWrite` sink (here, a file on disk).                                            |
| [`terminate_on_drop`](examples/terminate_on_drop.rs)       | Opt out of the panic-on-drop guard.                                                                       |
| [`terminate_on_timeout`](examples/terminate_on_timeout.rs) | Cap the wait, force a graceful shutdown when it times out.                                                |
| [`wait_for_readiness`](examples/wait_for_readiness.rs)     | Start a service and wait until it logs that it is ready, with logging running concurrently.               |

## Choosing stream settings

Each stdout/stderr stream is configured along five axes (backend, delivery, replay, collection, buffering) at spawn
time. The runnable examples above show common combinations end-to-end; the per-axis reference below explains each knob
on its own.

### Axes

**Backend.** `broadcast()` for multiple concurrent consumers (e.g. logging plus a line waiter), `single_subscriber()`
for one active consumer at a time (a second one fails with `StreamConsumerError::ActiveConsumer`; slightly faster, loud
on accidental fanout), `discard()` to drop the bytes at the OS level via `Stdio::null()` (the remaining axes are
skipped because they have nothing to act on).

**Delivery.** Both options name the trade-off in full: you pick what you get *and* what you pay.
`reliable_with_backpressure()` keeps active subscribers gap-free by pausing the reader task when their buffer fills,
which fills the kernel pipe and blocks the child's next write; the reliability is scoped to subscribers already
attached at the time each chunk is produced, so a late attacher still depends on replay for earlier output.
`lossy_without_backpressure()` never pauses the reader: when a slow subscriber's buffer fills, the chunk is dropped
for that subscriber rather than blocking the child, and line consumers discard the in-progress partial line and
resync at the next newline.

**Replay.** Replay retains output the stream has already produced and delivers it to every new subscriber before live
events resume. Its primary purpose is to close the spawn-to-attach window: the child is running by the time `spawn()`
returns, so any consumer (even one attached on the very next line) is registered too late to observe the bytes the
child wrote in between. The same buffer also lets a consumer that attaches much later (a postmortem collector, a
freshly subscribed log viewer) pick up recent context instead of starting blind. Choose between `.no_replay()` (live
only; the stream itself does not retain any history for late subscribers), `.replay_last_bytes(...)` /
`.replay_last_chunks(...)` (capped retention), and `.replay_all()` (unbounded; trusted volume only). Long-running
services typically spawn with a replay policy, attach consumers, then call `seal_output_replay()` (or per-stream
`seal_stdout_replay()` / `seal_stderr_replay()`) to release retained history. The seal applies to *future* consumers;
existing ones keep the snapshot they were given at attach time.

Prefer an explicit replay policy over relying on whatever the system pipe buffer happens to retain: the policy is
specified in code and behaves the same across operating systems.

**Buffering.** `.read_chunk_size(...)` is the size of each `read()` against the child's pipe;
`.max_buffered_chunks(...)` is the depth of the in-process queue between the reader task and the consumer. Pass
`DEFAULT_READ_CHUNK_SIZE` (16 KB) and `DEFAULT_MAX_BUFFERED_CHUNKS` (128) unless you have measured a reason to tune.
Both knobs must be named at the spawn site so the choice is visible in code review.

**Collection.** Bounded `LineCollectionOptions` / `RawCollectionOptions` for untrusted output, `TrustedUnbounded` for
known-volume sources. Bounded variants take a `CollectionOverflowBehavior`: `DropAdditionalData` (default) keeps the
first-retained output and discards anything past the cap; `DropOldestData` keeps the newest output by evicting older
data, giving a ring-buffer-shaped tail.

## Visitors and Consumers

A streams output can be observed using a `StreamVisitor` / `AsyncStreamVisitor`.

`stream.consume(visitor)` (and `consume_async` for an async visitor) is the single entry point. The bundled visitor
families under `tokio_process_tools::visitors` cover common requirements. Consult type-level docs for full information:

- `visitors::inspect` (`InspectChunks`, `InspectChunksAsync`, `InspectLines`, `InspectLinesAsync`): run a callback per
  event and discard. Reach for these when only the side effect matters.
- `visitors::collect` (`CollectChunks`, `CollectChunksAsync`, `CollectLines`, `CollectLinesAsync`): retain output in a
  caller-supplied `Sink`, with `CollectedBytes` and `CollectedLines` as ready-made sinks. The `fold(sink, step)`
  constructor reads at the call site like an explicit fold.
- `visitors::write` (`WriteChunks`, `WriteLines`): forward into an async writer. `passthrough` for identity, `mapped`
  to transform first. `WriteCollectionOptions` controls writer-failure handling; `LineWriteMode` controls whether
  parsed lines get the stripped `\n` reattached.
- `visitors::wait::WaitForLine`: break the parser the first time a predicate accepts a line. The backends'
  `wait_for_line` helpers wrap this with a `tokio::time::timeout`.

The line-aware variants compose with `ParseLines` (e.g. `ParseLines::inspect(opts, f)`,
`ParseLines::collect(opts, sink, f)`); see their rustdoc for the trait selection between the sync and async paths. The
async line-aware variants pay one allocation per line because the parser borrow cannot be held across `.await`; prefer
the synchronous flavor when the per-line callback is non-blocking.

See [`examples/custom_visitor.rs`](examples/custom_visitor.rs) for guidance on building your own visitor by
implementing `StreamVisitor` or `AsyncStreamVisitor` directly.

### Consumer lifecycle

Each `Consumer<S>` owns its spawned task. Dropping the handle cancels the consumer immediately. Bind it at least to a
variable (e.g. `let _consumer = stream.consume(...)`) to silence the `#[must_use]` warning when you do not intend to
interact with the handle. For long-lived processes, store the handle somewhere so it is not dropped.

A consumer exposes three terminal methods, trading off state recovery against promptness:

| Method            | Returns                                                                                                                                               | Use when                                                            |
|-------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------|
| `wait()`          | the visitor's output (e.g. the writer back from `WriteLines`) when the stream closes or the visitor returns `Next::Break`                             | there is a natural endpoint and you want what the consumer produced |
| `cancel(timeout)` | `ConsumerCancelOutcome::Cancelled(sink)` if the task observes the cancellation in time, `ConsumerCancelOutcome::Aborted` if the timeout elapses first | bounded cooperative-then-forceful shutdown is acceptable            |
| `abort()`         | nothing; the in-flight future is dropped, sinks and writers cannot be recovered                                                                       | not getting stuck matters more than recovering state                |

## Wait builder

`wait_for_completion(timeout)` returns a staged builder you `.await` to run. It allows to configure three orthogonal
axes:

- **The wait itself.** `wait_for_completion(timeout)` always closes any still-open stdin first, then waits for process
  exit within `timeout`.
- **Output collection.** None (default), `.with_line_output(eof_timeout, options)`, or `.with_raw_output(eof_timeout,
  options)`. Line collection parses on the fly into a `Vec<String>`-style output; raw collection returns the bytes the
  child wrote. Collect raw output when the process does not produce UTF-8 line-oriented data.
- **Graceful termination.** None (default) or `.or_terminate(shutdown)`, which embeds the same configured
  `GracefulShutdown` from [Graceful shutdown policy](#graceful-shutdown-policy). Gracefully terminates the process
  should it not exit withing the timeout provided to `wait_for_completion`.

`eof_timeout` (passed alongside output options) starts after the process has completed (or, with `.or_terminate(...)`,
after cleanup termination has succeeded). It bounds the post-exit drain of stdout/stderr. The constant
`DEFAULT_OUTPUT_EOF_TIMEOUT` (3 s) is a reasonable starting point. For trusted, bounded-by-construction output, both
`LineCollectionOptions` and `RawCollectionOptions` have a `TrustedUnbounded` variant that opts out of memory caps.

## Line parsing

`LineParsingOptions` controls how chunks are split into lines, and `wait_for_line()` takes the same options so its
matcher sees the same lines a collector would. The options cap a single line (`max_line_length`), pick what to do with
overflow (`LineOverflowBehavior::DropAdditionalData` or `EmitAdditionalAsNewLines`), and optionally compact the parser's
retained buffer capacity (`buffer_compaction_threshold`). See [`LineParsingOptions`] for the full per-field semantics,
including the `NumBytes::MAX` opt-out for trusted unbounded line parsing.

When chunks drop under `lossy_without_backpressure()`, line parsers conservatively discard the in-progress partial line and
resynchronize at the next newline rather than splicing across the gap.

## Manual process control

When the wait-and-cleanup helpers do not fit, drive shutdown by hand: `send_interrupt_signal()` and
`send_terminate_signal()` on Unix, `send_ctrl_break_signal()` on Windows, `terminate()` (escalation up to kill), and
`kill()` (immediate). Each returns a typed `TerminationError` describing which step failed. The signal-send methods are
idempotent against a child that has already exited: they reap the status and return `Ok(())` without sending a signal
at a possibly-recycled PID.

## Process management

### Process naming

A process name is required before stdout/stderr can be configured. The name appears in error messages and tracing
fields, and the library refuses to spawn anonymous processes because anonymous failures are hard to debug. The default
automatic name (`AutoName::program_only()`) captures only the program name and is safe for production. An explicit
label (`"api-worker"`, `format!("agent-{id}")`) is still useful when you want a stable, human-chosen identifier so
logs and metrics group cleanly across restarts. The richer `AutoName` variants (and `AutoNameSettings` opt-ins) that
include arguments, environment variables, or the current directory will surface those values in error messages, so
check redaction before reaching for them.

| Form               | How to construct                                                                  | Notes                                             |
|--------------------|-----------------------------------------------------------------------------------|---------------------------------------------------|
| Explicit           | `.name("api-worker")` / `.name(format!("agent-{id}"))`                            | Stable, human-chosen identifier.                  |
| Safe automatic     | `.name(AutoName::program_only())`                                                 | Program name only; never leaks args, env, or cwd. |
| Detailed automatic | `.name(AutoName::program_with_args())` / `program_with_env_and_args()` / `full()` | Includes args / env / cwd; check redaction.       |
| Debug-formatted    | `.name(AutoName::Debug)`                                                          | Full `Command` Debug repr; may include internals. |
| Custom automatic   | `.name(AutoNameSettings::builder()...build())`                                    | Opt into a specific subset of fields.             |

See [`examples/process_naming.rs`](examples/process_naming.rs) for a runnable demonstration.

### Automatic termination on drop

A live `ProcessHandle` dropped without a successful terminal wait, `terminate()`, or `kill()` runs best-effort cleanup
and then **panics**. The panic is deliberate: a silent leaked child is much harder to detect than a loud panic, and
most "I forgot to wait" bugs would otherwise survive into production.

For long-lived processes that follow a service's own lifecycle, opt in to `.terminate_on_drop(...)` to swap the panic
for an async termination attempt during drop. The signature mirrors `terminate(...)` and takes a `GracefulShutdown`.
Drop runs the termination on Tokio's blocking pool, so `terminate_on_drop` requires a multithreaded Tokio runtime;
under `#[tokio::test]` opt in to `flavor = "multi_thread"`. If the attempt itself fails, the inner handle still falls
back to the panic path so the failure is not silent. See
[`examples/terminate_on_drop.rs`](examples/terminate_on_drop.rs).

When the child genuinely needs to outlive the handle (a daemon spawned intentionally), call `must_not_be_terminated()`.
That suppresses both the kill and the panic on drop. The library still drops its owned stdio, however: the child loses
access to its piped stdin and the parent loses its stdout/stderr at the same moment. To keep the child *and* its
`Stdin`/`Stdout`/`Stderr` together, use `ProcessHandle::into_inner()` to take ownership of all four.

## Graceful shutdown policy

For service-like children on either platform,
`GracefulShutdown::builder().unix_sigterm(Duration::from_secs(5)).windows_ctrl_break(Duration::from_secs(8)).build()`
is a safe default. The rest of this section explains when to depart from that.

`terminate(...)` always takes a single [`GracefulShutdown`] argument; the signature is identical on every supported
platform. `GracefulShutdown` itself carries platform-conditional fields, because the underlying graceful-shutdown model
is platform-conditional:

- On Unix it carries a [`UnixGracefulShutdown`] of one or more phases. Each phase pairs a signal (`SIGINT` or
  `SIGTERM`) with a per-phase timeout. After the configured phases run, `SIGKILL` is the implicit forceful fallback.
- On Windows it carries a [`WindowsGracefulShutdown`] with a single `timeout`, matching the 2-phase
  `CTRL_BREAK_EVENT` -> `TerminateProcess` shutdown.

### Timeouts are upper bounds, not delays

Each user-supplied graceful timeout bounds the post-signal wait of its phase. The wait future resolves the instant
Tokio's SIGCHLD reaper observes the child exit, so handler-less children typically die in microseconds via the kernel's
default disposition (`Term`) and the configured timeout never fires for them. The timeout only matters when the child
has installed a handler that takes time to complete.

| Scenario per phase                        | Time spent in this phase                |
|-------------------------------------------|-----------------------------------------|
| Signal send succeeds, child exits         | ≤ user timeout (typically much less)    |
| Signal send succeeds, child does NOT exit | exactly user timeout, then escalate     |
| Signal send fails                         | up to 100 ms fixed grace, then escalate |

The 100 ms grace covers the small window where the child has already exited but Tokio's SIGCHLD reaper has not yet
observed it; it replaces, never adds to, the user timeout for a failed phase. Real permission denials still surface as
a [`TerminationError`]. See its rustdoc for the platform error codes involved.

`Duration::from_secs(0)` disables the post-signal wait for that phase. The graceful signal is still sent; the call
collapses to "send the signal, do not wait, escalate immediately". On Unix that means a zero-duration phase rolls
straight on to the next phase (or to the implicit `SIGKILL` fallback). On Windows there is only the one phase, so
`timeout = 0` reduces `terminate(...)` to "send `CTRL_BREAK_EVENT`, then immediately `TerminateProcess`". Prefer small
non-zero values (100 ms to a few seconds) so well-behaved children can run their handlers.

### Which signal should I send?

A child can only be gracefully shut down by a signal it has a handler installed for. Without a handler, the kernel's
default disposition kills the child immediately on both `SIGINT` and `SIGTERM`. The library cannot pick a default
correct for every child, but the choice is well-trodden:

- **Default for unknown / service-like children: `.unix_sigterm(timeout)`.** `SIGTERM` is the standard Unix shutdown
  signal: what `kill <pid>` (no args) sends, and what systemd, K8s, Docker, runit, and supervisord all use.
- **Override to `.unix_sigint(timeout)` for the "I pressed Ctrl-C" semantics:** dev tools, REPLs, watch/reload runners,
  task runners. This matches tokio's `tokio::signal::ctrl_c()` and Python's `KeyboardInterrupt`.

Combining `SIGINT` and `SIGTERM` as escalation does NOT cover children with unknown signal handlers. Whichever signal
the child does not handle kills it via kernel default during phase 1, and the second phase never runs. `from_phases`
is for the narrow case where a single child handles BOTH signals as distinct cooperative stages (for example, `SIGINT`
"begin draining" and `SIGTERM` "abort drain, exit immediately"). See [`UnixGracefulShutdown::from_phases`] for the full
discussion.

### When termination fails

If `terminate()` returns `Err`, the panic-on-drop guard stays armed: the library cannot verify cleanup from the
outside, so dropping would leak a process. Recover by retrying `terminate()`, escalating to `kill()`, calling
`must_not_be_terminated()` to accept the failure, or propagating the error and letting the panic-on-drop surface the
leak.

## Platform support

Termination escalation is per-platform. Every spawned child is set up as the leader of its own process group, so
graceful and forceful steps reach the whole subtree rather than only the leaf.

- **Linux/macOS:** the child is spawned with `process_group(0)`, making its PID equal to its PGID. The configured
  `UnixGracefulShutdown` dispatches its signals to the group via `killpg`, then `SIGKILL` runs as the implicit forceful
  fallback. `send_interrupt_signal()`, `send_terminate_signal()`, `terminate()`, and `kill()` all target the group.
- **Windows:** the child is spawned with `CREATE_NEW_PROCESS_GROUP` and additionally assigned to an anonymous Job
  Object after spawn. `CTRL_BREAK_EVENT` is delivered to the console process group via `GenerateConsoleCtrlEvent`; the
  forceful-kill fallback uses `TerminateJobObject`, which reaches the leader and every descendant the OS has
  associated with the job. The Job Object is only assigned post-spawn (`tokio::process::Command` does not expose
  `CREATE_SUSPENDED`), so grandchildren forked in the small window before assignment are not captured by the job and
  behave like the explicitly-detached descendants below.
- **Other:** spawn, wait, output-collection, and `kill(...)` remain available; `kill(...)` forwards to Tokio's
  `Child::start_kill()`. The graceful-termination machinery (`terminate(...)`, `terminate_on_drop(...)`,
  `.or_terminate(...)`, `GracefulShutdown`, `send_*_signal(...)`) is gated out because there is no OS primitive to
  deliver a graceful signal against. Best-effort cleanup on drop still goes through `Child::start_kill()`, so un-awaited
  handles do not leak the child.

> **Windows interop note.** `tokio::signal::ctrl_c()` on Windows registers only for `CTRL_C_EVENT`; it does not catch
> `CTRL_BREAK_EVENT`. A child Rust binary that listens only on the cross-platform `tokio::signal::ctrl_c()` will not
> respond to this library's graceful step on Windows and will be terminated forcefully. Such a child should additionally
> listen on `tokio::signal::windows::ctrl_break()` (or use a stdin sentinel / IPC / command protocol).

A grandchild that explicitly detaches itself (`setsid`/`setpgid` to leave the group, double-fork into init, or its own
`JobObject` on Windows) is by definition outside the group and will not be signaled. That is correct: such children have
asked to outlive their parent. If you do not own the leaf, and it leaks descendants of its own, run it under a small
reaper (`tini` is the usual choice).

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
