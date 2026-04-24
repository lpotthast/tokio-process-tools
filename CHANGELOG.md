# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added `StreamConfig` and a required type-state `StreamConfig::builder()` API for direct stream
  construction and process stdout/stderr stream configuration. The builder requires selecting
  delivery, replay, read chunk size, and maximum buffered chunks.
- Added sealed typed delivery and replay policy markers: `BestEffortDelivery`, `ReliableDelivery`,
  `NoReplay`, `ReplayEnabled`, `Delivery`, `Replay`, `DeliveryGuarantee`, and
  `ReplayRetention`.
- Added typed process spawning with `.stdout_and_stderr(...)`, `.stdout(...)`, and `.stderr(...)`
  stream configuration stages. Stdout and stderr may now use different backends and, for broadcast
  streams, different delivery and replay mode types.
- Added broadcast stream delivery and replay configuration via `stream.broadcast()`, including
  `.best_effort_delivery()`, `.reliable_for_active_subscribers()`, `.no_replay()`,
  `.replay_last_chunks(...)`, `.replay_last_bytes(...)`, and `.replay_all()`.
- Added single-subscriber stream delivery and replay configuration via
  `stream.single_subscriber()` while keeping the lower-overhead single-consumer backend and
  second-consumer rejection behavior.
- Added replay-enabled stream APIs for retained output and replay sealing: `seal_replay` and
  `is_replay_sealed`. Replay-retention metadata uses `Option<ReplayRetention>` to represent
  disabled replay.
- Added handle-level replay sealing helpers. Broadcast handles expose `seal_stdout_replay` and
  `seal_stderr_replay` only when the relevant typed stream has replay enabled, and
  `seal_output_replay` only when both broadcast streams have replay enabled. Single-subscriber
  handles expose matching helpers for the single-subscriber backend.
- Added `BroadcastProcessHandle` as a readable alias for broadcast process handles with typed
  stdout and stderr streams.
- Added `inspect_chunks_async` for asynchronously inspecting raw output chunks without storing
  them.
- Added bounded in-memory output collection types and options: `RawCollectionOptions`,
  `LineCollectionOptions`, `CollectedBytes`, `CollectedLines`, and
  `CollectionOverflowBehavior`. Bounded collections expose truncation metadata, and trusted-output
  helpers preserve the previous unbounded collection behavior under explicit method names.
- Added required-field builders for `LineParsingOptions`, `LineCollectionOptions`, and process
  wait/output option structs.
- Added `WriteCollectionOptions` and public sink write error handler types for configuring whether
  writer collectors stop or continue after individual sink write failures. The options type remains
  generic so custom handlers are statically dispatched and allocation-free.
- Added `StreamReadError` so stream read failures can be surfaced by line waiters, collectors, and
  inspectors.
- Added `AutoName::program_only()`, `AutoName::program_with_args()`,
  `AutoName::program_with_env_and_args()`, and `AutoName::full()` as convenience constructors
  mirroring the built-in `AutoNameSettings` presets.
- Added `AutoNameSettings::builder()` and direct `.name(AutoNameSettings)` support for process
  names so callers can opt into combinations beyond the built-in naming presets.

### Changed

- **Breaking:** Changed `Process` into a staged builder: name the process with `.name(...)`,
  configure stdout and stderr with `.broadcast()` or `.single_subscriber()` stream builders, then
  call `.spawn()`.
- **Breaking:** Removed `Process::with_name(...)` and `Process::with_auto_name(...)`. Use
  `.name(...)` for explicit or automatic naming, including `.name(AutoName::program_only())` for
  the safe program-only default.
- **Breaking:** Changed direct `BroadcastOutputStream::from_stream` and
  `SingleSubscriberOutputStream::from_stream` construction to accept `StreamConfig<D, R>`.
  Single-subscriber delivery is now selected through delivery markers and `DeliveryGuarantee`
  instead of `BackpressureControl`.
- **Breaking:** Changed `ProcessHandle<O>` to `ProcessHandle<Stdout, Stderr = Stdout>`, with
  `stdout()` and `stderr()` returning their independently typed streams.
- **Breaking:** Changed `TerminateOnDrop<O>` to mirror the
  `ProcessHandle<Stdout, Stderr = Stdout>` generic shape.
- **Breaking:** Renamed stream sizing from chunk size/channel capacity to read chunk size/maximum
  buffered chunks, including `OutputStream::read_chunk_size()`,
  `OutputStream::max_buffered_chunks()`, `DEFAULT_READ_CHUNK_SIZE`, and
  `DEFAULT_MAX_BUFFERED_CHUNKS`.
- **Breaking:** Replaced `Output` and `RawOutput` with
  `ProcessOutput<Stdout, Stderr = Stdout>`. Existing helpers now return
  `ProcessOutput<CollectedLines>`, `ProcessOutput<CollectedBytes>`,
  `ProcessOutput<Vec<String>>`, or `ProcessOutput<Vec<u8>>`.
- **Breaking:** Replaced positional `wait_for_completion*` timeout and output arguments with
  required-field `TypedBuilder` option structs, including concrete line, raw, and trusted-line
  output wait options.
- **Breaking:** Changed `wait_for_line` and `wait_for_line_with_timeout` to return a `LineWaiter`
  future whose output is `Result<WaitForLineResult, StreamReadError>`. The stream subscription or
  single-subscriber receiver claim is created before the returned future is first polled.
- **Breaking:** Changed normal stream consumers to subscribe from the earliest output currently
  available. Replay-enabled unsealed streams may provide retained past output; no-replay or sealed
  streams start future consumers at live output.
- **Breaking:** Changed `wait_for_completion_or_terminate` to return the dedicated
  `WaitOrTerminateError` type, and changed output-collecting wait helpers to return dedicated
  compound error types instead of overloading `WaitError`.
- **Breaking:** Replaced per-phase `TerminationError::TerminationFailed` diagnostic fields with
  chronological `TerminationAttemptError` entries that preserve all recorded source errors.
- **Breaking:** Changed `collect_chunks_into_vec`, `collect_lines_into_vec`,
  `wait_for_completion_with_output`, and `wait_for_completion_with_raw_output` to require explicit
  collection limits. Trusted-output-only variants preserve the previous unbounded behavior under
  explicit `*_trusted` method names.
- **Breaking:** Changed `ProcessHandle::into_inner()` to return `(Child, Stdin, Stdout, Stderr)` so
  callers who extract the inner process retain manual control of piped stdin instead of implicitly
  closing it and sending EOF.
- **Breaking:** Changed `collect_chunks_into_write`, `collect_chunks_into_write_mapped`,
  `collect_lines_into_write`, and `collect_lines_into_write_mapped` to require
  `WriteCollectionOptions`; `WriteCollectionOptions::fail_fast()` stops on the first sink write
  failure.
- **Breaking:** Relaxed `Sink` to require only `Send`, allowing collectors and writer collectors
  to use sinks that are not `Debug` and cannot be shared concurrently.
- **Breaking:** Changed single-subscriber process spawning so delivery and replay must be selected
  explicitly. `.no_replay()` starts the sole consumer at live output instead of buffered startup
  output.
- Changed default automatic process names to include only the program name, avoiding accidental
  logging of command arguments that may contain secrets.
- Single-subscriber streams now allow one active consumer at a time instead of one consumer for the
  entire stream lifetime. After a collector, inspector, or line waiter completes, is canceled, is
  dropped, or times out, another consumer can attach. Concurrent consumers still panic.
- Replay-enabled single-subscriber streams retain configured replay history across sequential
  consumers, including output produced while no consumer is active. `.no_replay()` continues to
  discard output drained while no consumer is active.
- Simplified the Criterion benchmark suite to focused chunk-delivery and line-delivery targets for
  the single-subscriber and broadcast backends, added dedicated `just bench-smoke`,
  `just bench-chunks`, and `just bench-lines` commands, and made compile-only benchmark smoke the
  default workflow.

### Fixed

- Fixed `ProcessHandle::must_be_terminated()` so calling it on an already-armed handle is
  idempotent instead of dropping the existing panic-on-drop guard and panicking immediately.
- Fixed README process-naming docs to state that the staged builder requires an explicit naming
  call before stream configuration and to use `.name(AutoName::program_only())` for the safe
  program-only automatic naming case.
- **Breaking:** Fixed `terminate()` so failed or canceled termination attempts no longer disarm the
  drop cleanup and panic guards before the process has successfully terminated.
- Fixed `send_interrupt_signal()` and `send_terminate_signal()` to reap children that exited
  before signalling or during a failed signal attempt, avoiding stale PID/process-group targeting
  and spurious signal failures.
- Fixed `ProcessHandle::kill()` so a successful kill-and-wait disarms the drop cleanup and panic
  guards, allowing the handle to be dropped safely afterward.
- Fixed `wait_for_completion*()` helpers and `ProcessHandle::kill()` to close any still-open stdin
  handle before waiting for process exit, restoring Tokio-compatible deadlock avoidance after
  piped stdin ownership was split out of `Child`.
- Rejected zero broadcast and single-subscriber maximum buffered chunk counts before spawning a
  child process, avoiding Tokio channel-construction panics after process creation.
- Preserved non-timeout wait errors from `wait_for_completion_or_terminate` while still attempting
  cleanup termination after every wait failure.
- Continued termination escalation when a graceful signal cannot be sent, and reported diagnostics
  from all attempted shutdown phases.
- Fixed writer collectors so sink write errors are no longer silently suppressed unless explicitly
  accepted by the configured write error handler.
- Relaxed single-subscriber `inspect_chunks`, `collect_chunks`, and `collect_lines` callback bounds
  to accept stateful `FnMut` closures.
- Fixed Windows graceful interrupt delivery to use targeted `CTRL_BREAK_EVENT` for child process
  groups and enabled the `Win32_System_Threading` feature required for process-group creation.
- **Breaking:** Fixed timed `wait_for_completion_with_output*` and
  `wait_for_completion_with_raw_output*` so configured timeouts bound both process completion and
  stdout/stderr collection using the full effective wait-or-terminate budget, including inherited
  output pipes held open by descendants and the fixed 3-second post-kill confirmation wait when
  force-kill fallback is required. This adds `OutputCollectionTimeout` variants to the output wait
  error types, surfaces real collector failures promptly even when the sibling stream remains
  open, and keeps timed-out single-subscriber collectors from blocking later consumers.
- Fixed `Inspector::wait()` so dropping an in-flight wait future aborts the inspector task instead
  of detaching it, releasing single-subscriber claims held by stuck async inspectors.
- Fixed dropped single-subscriber streams so active line waiters, collectors, and inspectors are
  unblocked instead of waiting forever.

### Removed

- **Breaking:** Removed `Process::spawn_broadcast()` and `Process::spawn_single_subscriber()` in
  favor of the staged `.broadcast()`/`.single_subscriber()` stream builders.
- **Breaking:** Removed `Process::auto_name()`. Use `.name(AutoName::program_only())` for the
  safe program-only automatic naming preset.
- **Breaking:** Removed the old `Process` stream-sizing and single-subscriber backpressure setter
  methods. Read chunk size, maximum buffered chunks, delivery, and replay are now configured on the
  per-stream builder.
- **Breaking:** Removed `FromStreamOptions`, `DEFAULT_CHUNK_SIZE`, and `DEFAULT_CHANNEL_CAPACITY`;
  direct stream construction now uses `StreamConfig`.
- **Breaking:** Removed `BackpressureControl` and
  `SingleSubscriberOutputStream::backpressure_control()`. Single-subscriber buffering behavior is
  now represented directly by `DeliveryGuarantee`.
- **Breaking:** Removed `SealedReplayBehavior`, `ReplaySubscribeError`, and the explicit
  replay-from-start line wait APIs. Replay retention and sealing now control what output is
  available to the normal consumer APIs.
- Removed the atomic-take dependency.

## [0.8.1] - 2026-04-11

### Changed

- Windows constants `CTRL_C_EVENT` and `CTRL_BREAK_EVENT` are now imported from
  `windows_sys::Win32::System::Console`.

## [0.8.0] - 2026-04-11

### Added

- Added `WaitForLineResult` with `Matched`, `StreamClosed`, and `Timeout` outcomes for line-wait
  operations.
- Added `LineWriteMode` so line-writing helpers require an explicit choice between preserving
  mapped output as-is and appending `\n` delimiters.
- Added single-subscriber backpressure configuration via `Process::stdout_backpressure_control`,
  `Process::stderr_backpressure_control`, and `Process::backpressure_control`, plus
  `SingleSubscriberOutputStream::backpressure_control()` to inspect the configured policy.
- Added `AsyncChunkCollector` and `AsyncLineCollector` for wiring up custom async collectors not
  requiring a per-item allocation.
- Added `RawOutput` plus `wait_for_completion_with_raw_output` and
  `wait_for_completion_with_raw_output_or_terminate` on both process handle backends for collecting
  stdout and stderr as raw bytes.
- Made `BroadcastOutputStream::from_stream` public and re-exported `FromStreamOptions` for custom
  stream construction.
- Re-exported `BackpressureControl`, `Chunk`, `FromStreamOptions`, `LineWriteMode`, and
  `RawOutput` from the crate root.
- Added this `CHANGELOG.md` in Keep a Changelog format.

### Changed

- Changed `wait_for_line` and `wait_for_line_with_timeout` on both output stream
  implementations to return `WaitForLineResult` so callers can distinguish why the wait
  completed.
- Changed `collect_lines_into_write` and `collect_lines_into_write_mapped` on both stream
  backends to require an explicit `LineWriteMode`.
- **Breaking:** Changed `collect_chunks_async` and `collect_lines_async` on both stream backends
  to accept the new collector traits instead of callbacks returning boxed futures. This removes
  the per-item allocation previously required by `Pin<Box<dyn Future<Output = Next> + Send + '_>>`.
- Changed `ProcessHandle` to perform its own cleanup-on-drop instead of relying on Tokio's
  `kill_on_drop(true)`, which restores `must_not_be_terminated()` as a real opt-out from
  implicit termination.
- Changed chunk-size configuration to reject `NumBytes::zero()` in `Process` builder methods and
  stream `from_stream` constructors.
- Now enforcing pedantic clippy lints.
- Raised the MSRV from `1.85.0` to `1.89.0`.
- Updated the README and crate-level docs to cover the new line-wait outcomes, EOF behavior,
  backpressure trade-offs, stdin examples, and drop semantics.

### Fixed

- Fixed `ProcessHandle` drop semantics so dropping a live, still-armed handle first attempts
  best-effort cleanup and then lets the panic-on-drop guard report the misuse.
- Fixed `must_not_be_terminated()` so it once again disables implicit cleanup-on-drop behavior.
- Fixed `terminate()` to return the real exit status when the child exits just before or during
  the first signaling step instead of reporting a spurious signaling failure.
- Fixed `TerminateOnDrop` to attempt best-effort termination when process state probing is
  uncertain instead of treating that case as already terminated.
- Fixed a broadcast-stream late-subscriber race at EOF so subscribers that attach before closure
  cannot receive a synthetic terminal `None` ahead of real tail data.
- Fixed line-oriented output handling to preserve the final unterminated line at EOF in
  `wait_for_completion_with_output`, line collectors (sync and async), line waiters, and line
  inspectors (sync and async).
- Fixed line-oriented consumers to resynchronize after lossy gaps instead of joining bytes across
  dropped chunks.
- Fixed `LineOverflowBehavior::DropAdditionalData` so discarding now persists across chunk
  boundaries until the next newline is observed.
- Now requiring `bytes` in v1.11.1 to enforce a RUSTSEC-2026-0007 fixed version.

### Removed

- Removed `OutputError` from the public API because line-wait timeouts are now modeled as a
  normal result instead of an error.
- Removed the boxed async collector callback API.

## [0.7.2] - 2025-11-11

### Added

- Added programmatic stdin handling via `ProcessHandle::stdin()` and the `Stdin` enum.

### Changed

- Expanded the README with stdin examples and clarified collector behavior after process
  termination.

## [0.7.1] - 2025-10-15

### Fixed

- Ensured `ProcessHandle`, `BroadcastOutputStream`, and `SingleSubscriberOutputStream` are
  `Send + Sync`.

## [0.7.0] - 2025-10-15

### Added

- Added the `Process` builder with explicit `.spawn_broadcast()` and
  `.spawn_single_subscriber()` entry points.
- Added process naming configuration via `ProcessName`, `AutoName`, and `AutoNameSettings`.
- Added centralized error types via `SpawnError` and `OutputError`.
- Exported `DEFAULT_CHUNK_SIZE` and `DEFAULT_CHANNEL_CAPACITY`.

### Changed

- Replaced direct public `ProcessHandle::spawn*` constructors with `Process::new(...)` as the
  public spawning API.
- Allowed inspectors and collectors to be created from shared `stdout()` and `stderr()`
  references even for single-subscriber handles.
- Switched line callbacks to `Cow<'_, str>` where possible to reduce unnecessary allocations.
- Streams, collectors, and inspectors now carry stream names in their diagnostics.
- Always create a Windows process group for spawned children.

## [0.6.0] - 2025-10-14

### Added

- Added the `Output` struct as a structured return type for collected process output.
- Added `wait_for_completion_with_output_or_terminate`.
- Re-exported `WaitError`.

### Changed

- Renamed output-related wait helpers around the `wait_for_completion_*` naming scheme for
  better discoverability.
- Started using `README.md` as crate-level Rustdoc and significantly expanded the documentation.
- Switched dependencies to explicit feature lists to reduce the dependency tree.

## [0.5.7] - 2025-10-02

### Changed

- Refreshed dependencies.
- Updated README examples and documentation.

## [0.5.6] - 2025-06-07

### Fixed

- Disarmed the termination-on-drop safeguard when `try_wait()` had already observed process exit,
  avoiding incorrect follow-up termination behavior on completed processes.

## [0.5.5] - 2025-06-06

### Added

- Implemented `DerefMut` for `TerminateOnDrop`.

## [0.5.4] - 2025-05-31

### Changed

- Refactored and expanded line-parser regression coverage, including preserved-whitespace tests.
  No intended public API change was identified in this release.

## [0.5.3] - 2025-05-25

### Fixed

- Treated `LineParsingOptions { max_line_length: 0, .. }` as "no limit" instead of immediately
  tripping overflow handling.

## [0.5.2] - 2025-05-23

### Added

- Exported `LineOverflowBehavior`.

### Fixed

- Corrected `LineOverflowBehavior::DropAdditionalData`.

## [0.5.1] - 2025-05-19

### Changed

- Documentation-only release to clean up the new `0.5.0` README and installation instructions.

## [0.5.0] - 2025-05-18

### Added

- Added dedicated `broadcast` and `single_subscriber` output stream backends.
- Added chunk-based processing APIs and the `Chunk` type.
- Added `Next`, `LineParsingOptions`, `LineOverflowBehavior`, and line-to-writer collection
  helpers.
- Added panic-on-drop leak detection, explicit `kill()`, and stricter process lifecycle
  safeguards.

### Changed

- Reworked process waiting APIs around `wait_for_completion*`.
- Replaced `IsRunning` with `RunningState`.
- Switched internal chunk and line buffering to `bytes::BytesMut` to reduce allocation pressure.
- Bumped the MSRV to `1.85.0` and moved to Rust edition 2024.

## [0.4.0] - 2025-02-04

### Changed

- Simplified async inspector and collector bounds so callers can pass ordinary futures instead of
  boxed pinned futures.

### Fixed

- Reduced expected shutdown-noise logging when inspector or collector termination races occur.

## [0.3.1] - 2025-01-19

### Fixed

- Added the missing `windows-sys` feature needed for Windows builds.

## [0.3.0] - 2025-01-19

### Added

- Added `ProcessHandle::spawn_with_capacity(...)`.

### Changed

- Made `ProcessHandle::spawn(...)` the normal public construction path and internalized
  `new_from_child_with_piped_io(...)`.
- Let the library prepare captured stdio and related command settings itself so termination stays
  reliable.
- Created a new process group on Windows so control-event-based shutdown works correctly.

## [0.2.1] - 2025-01-19

### Changed

- Documentation-only release to update examples for `ProcessHandle::spawn(...)` and the new
  termination-timeout behavior.

## [0.2.0] - 2025-01-19

### Added

- Added `ProcessHandle::spawn(...)`.
- Added `send_interrupt_signal()` and `send_terminate_signal()`.

### Changed

- Reworked termination to escalate `SIGINT` to `SIGTERM` to `SIGKILL` or the platform
  equivalents.
- Made termination timeouts explicit and enforced in both `terminate()` and `TerminateOnDrop`.
- Replaced the older `interrupt` module with cross-platform signal handling.

## [0.1.1] - 2025-01-17

### Changed

- Documentation-only release that updated installation instructions for crates.io usage.

## [0.1.0] - 2025-01-17

### Added

- Initial published release.
- Added `ProcessHandle` for stdout/stderr inspection, collection, wait-for-output, and
  termination support.
- Added `TerminateOnDrop` for best-effort async cleanup on drop.
- Added process state helpers such as `id()` and `is_running()`.
- Added `collect_into_*` helpers on `OutputStream`.

[Unreleased]: https://github.com/lpotthast/tokio-process-tools/compare/v0.8.1...HEAD

[0.8.1]: https://github.com/lpotthast/tokio-process-tools/compare/v0.8.0...v0.8.1

[0.8.0]: https://github.com/lpotthast/tokio-process-tools/compare/v0.7.2...v0.8.0

[0.7.2]: https://github.com/lpotthast/tokio-process-tools/compare/v0.7.1...v0.7.2

[0.7.1]: https://github.com/lpotthast/tokio-process-tools/compare/v0.7.0...v0.7.1

[0.7.0]: https://github.com/lpotthast/tokio-process-tools/compare/v0.6.0...v0.7.0

[0.6.0]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.7...v0.6.0

[0.5.7]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.6...v0.5.7

[0.5.6]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.5...v0.5.6

[0.5.5]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.4...v0.5.5

[0.5.4]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.3...v0.5.4

[0.5.3]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.2...v0.5.3

[0.5.2]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.1...v0.5.2

[0.5.1]: https://github.com/lpotthast/tokio-process-tools/compare/v0.5.0...v0.5.1

[0.5.0]: https://github.com/lpotthast/tokio-process-tools/compare/443d629b4fb193f36cba48e9431eec6b38e4823f...v0.5.0

[0.4.0]: https://github.com/lpotthast/tokio-process-tools/compare/5741ad89be9c75764b417c0d2f76200d70ed9655...443d629b4fb193f36cba48e9431eec6b38e4823f

[0.3.1]: https://github.com/lpotthast/tokio-process-tools/compare/41d4017b3c5c1eb566d733944588701411fa39fe...5741ad89be9c75764b417c0d2f76200d70ed9655

[0.3.0]: https://github.com/lpotthast/tokio-process-tools/compare/3f59bdd3759327ce23745205cef432f0b83c69c7...41d4017b3c5c1eb566d733944588701411fa39fe

[0.2.1]: https://github.com/lpotthast/tokio-process-tools/compare/ef7d1bd3814e1dae30e9e63c8d9c0992af192440...3f59bdd3759327ce23745205cef432f0b83c69c7

[0.2.0]: https://github.com/lpotthast/tokio-process-tools/compare/8657219ba05f3645ab617a5a2ffe9ef58d92b28b...ef7d1bd3814e1dae30e9e63c8d9c0992af192440

[0.1.1]: https://github.com/lpotthast/tokio-process-tools/compare/cfeba4890a8b5f6ffd74042017eaf38e623f5b57...8657219ba05f3645ab617a5a2ffe9ef58d92b28b

[0.1.0]: https://github.com/lpotthast/tokio-process-tools/tree/cfeba4890a8b5f6ffd74042017eaf38e623f5b57
