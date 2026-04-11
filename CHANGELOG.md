# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.8.0] - 2026-04-11

### Added

- Added `WaitForLineResult` with `Matched`, `StreamClosed`, and `Timeout` outcomes for line-wait
  operations.
- Added `LineWriteMode` so line-writing helpers require an explicit choice between preserving
  mapped output as-is and appending `\n` delimiters.
- Added `Process` builder methods for single-subscriber backpressure control:
  `stdout_backpressure_control`, `stderr_backpressure_control`, and `backpressure_control`.
- Added `AsyncChunkCollector` and `AsyncLineCollector` for wiring up custom async collectors not
  requiring a per-item allocation.
- Re-exported `BackpressureControl`, `Chunk`, and `LineWriteMode` from the crate root.
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
  `wait_for_completion_with_output`, line collectors (sync and async), line waiters, and
  `inspect_lines_async`.
- Fixed line-oriented consumers to resynchronize after lossy gaps instead of joining bytes across
  dropped chunks.
- Fixed `LineOverflowBehavior::DropAdditionalData` so discarding now persists across chunk
  boundaries until the next newline is observed.
- Now requiring `bytes` in v0.11.1 to enforce a RUSTSEC-2026-0007 fixed version.

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

[Unreleased]: https://github.com/lpotthast/tokio-process-tools/compare/v0.8.0...HEAD
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
