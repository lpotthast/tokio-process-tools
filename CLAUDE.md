# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`README.md` is the canonical user-facing reference for public behavior and is re-exported as crate-level Rustdoc by
`src/lib.rs`. Treat the `pub use` block in `src/lib.rs` as the canonical export map before reshaping public API.

## Commands

Use `just` for normal workflows; the recipes are in `Justfile`.

- `just verify`: full pre-PR validation (fmt-check, clippy, test, build, doc). Run this before declaring a change done.
- `just fmt`: apply rustfmt across the workspace.
- `just lint`: `cargo clippy --all-targets --all-features -- -D warnings`.
- `just test`: full unit test suite.
- `just bench-chunks` / `just bench-lines`: targeted criterion benchmarks (`benches/`).

Single test by name: `cargo test <test_name> -- --nocapture`. Some tests need a multi-thread runtime; see "Drop
semantics" below.

MSRV is `1.89.0`, edition `2024`.

## Architecture

The crate wraps `tokio::process::Child` with three layered concepts. Understanding the boundaries between them is the
fastest path to making correct changes.

1. **`Process` (spawn config) -> `ProcessHandle` (running child).** `src/process/builder.rs` is a staged builder that
   forces the caller to choose a name, then a stdout/stderr stream configuration, before `spawn()` returns a handle.
   Stream backend choice is encoded in the type system via `ProcessStreamBuilder`, `StreamConfig<D, R>`, and
   `ProcessHandle<Stdout, Stderr>`. Do not replace this with runtime backend dispatch; the compile-time typing is what
   gates which consumer-API surfaces are reachable on a given handle (e.g. the `.with_line_output(...)` /
   `.with_raw_output(...)` stages on `wait_for_completion(...)` are not in scope on a handle whose stream is discarded).

2. **Output streams** sit between the child's stdout/stderr and consumer code. Five orthogonal axes (backend, delivery,
   replay, collection, buffering) are configured at spawn time. The three backends live under
   `src/output_stream/backend/`:
   - `broadcast/`: multi-consumer. Has a fast `tokio::sync::broadcast` path for best-effort + no-replay, and a
     shared-state path when reliable delivery or replay retention is configured.
   - `single_subscriber/`: lower-overhead single-consumer. Must continue to reject a second active consumer while
     preserving replay behavior.
   - `discard/`: zero-cost marker for stdio configured as `Stdio::null()`. Implements `Subscribable` and `Consumable`
     with `Error = Infallible` for API uniformity, but its subscription emits a single EOF event and then closes.

   Policies are typed marker types in `src/output_stream/policy.rs`. Line-aware primitives
   (`LineParsingOptions`, `LineOverflowBehavior`, `LineParser`, `ParseLines`, `LineVisitor` / `AsyncLineVisitor`) live
   under `src/output_stream/line/`. Gap handling, EOF handling, and surfacing a final unterminated line are shared
   load-bearing behavior across inspectors, collectors, waiters, and output-collection helpers; changes there ripple.

3. **Consumers** drive a `StreamVisitor` / `AsyncStreamVisitor` over a subscription. `Consumer<S>` in
   `src/output_stream/consumer/` owns the spawned task; dropping it cancels the consumer. The bundled visitors
   (`collect`, `inspect`, `wait`, `write`) live in `src/output_stream/visitors/` and are user-facing types under
   `tokio_process_tools::visitors`. There is no per-backend factory glue: callers construct a visitor (e.g.
   `CollectChunks::fold(...)`, `ParseLines::inspect(...)`, `WriteLines::passthrough(...)`) and pass it to
   `stream.consume(...)` or `stream.consume_async(...)`. `Consumable::consume` / `consume_async` are default-impl
   methods (gated on `Self: OutputStream` so they can label the consumer task with the stream name); each backend's
   `Consumable` impl only specifies `type Error = ...;`. `wait_for_line` on each backend is a thin wrapper that adds a
   `tokio::time::timeout` around the `WaitForLine` visitor.

`src/process_handle/` owns the running-child lifecycle and is intentionally split by behavior: `spawn.rs` (stdio
capture and group setup), `mod.rs` (handle struct, stdin state, `RunningState`, `into_inner`), `wait.rs` (waiting
without output), `output_collection/` (waiting with collected stdout/stderr), `termination/` (graceful escalation),
`signal.rs` (single-shot user-facing signal shortcuts), `replay.rs` (macros that conditionally generate `seal_*_replay`
methods), `drop_guard.rs` (panic/kill-on-drop guards), and `group/` (the platform-specific process-group abstraction).
Keep cleanup and panic-on-drop logic centralized here.

The `process` -> `process_handle` boundary is bridged by exactly one trait,
`process::stream_config::ProcessStreamConfig`, consumed in `process_handle::spawn`. Think twice before widening that
interface; the layered design relies on it staying narrow.

`src/process_handle/group/` isolates platform-specific group/job management. Termination escalates `SIGINT -> SIGTERM
-> SIGKILL` via `killpg` on Unix and `CTRL_BREAK_EVENT -> TerminateJobObject` on Windows. Children are spawned as
process-group leaders (`process_group(0)` on Unix, `CREATE_NEW_PROCESS_GROUP` on Windows) so signals reach the whole
subtree; on Windows the spawned child is also assigned to a Job Object so `TerminateJobObject` reaches grandchildren
the leader has fork-execed instead of orphaning them the way `Child::start_kill` would. Preserve this when touching
shutdown logic.

## Drop semantics (critical)

A live armed `ProcessHandle` that is dropped without a successful terminal wait, `terminate*`, or `kill` performs
best-effort cleanup and then **panics**. The opt-out paths are explicit: `wait_for_completion*`, `terminate*`, `kill`,
`.terminate_on_drop(...)`, `into_inner()`, or `must_not_be_terminated()`. This is deliberate. Do not soften it to a
silent leak.

`TerminateOnDrop` (in `src/terminate_on_drop.rs` + `src/async_drop.rs`) drives async termination from synchronous
`Drop` and **requires a multithreaded Tokio runtime**. Tests that exercise it must use
`#[tokio::test(flavor = "multi_thread")]`. The single-threaded runtime cannot drive that path.

`PanicOnDrop` is the misuse guard for the armed-handle case. If `terminate()` returns `Err`, the guard stays armed on
purpose so the failure is loud rather than silent. Recover by retrying, escalating to `kill()`, or calling
`must_not_be_terminated()` to acknowledge. One exception: if the thread is already unwinding from another panic when
the guard drops, it logs via `tracing::warn!` instead of raising a second panic

## Stdin invariants

Stdin is always piped (no opt-out). `process.stdin()` returns a `&mut Stdin` enum with `Open(ChildStdin)` / `Closed`
variants; `Stdin::as_mut()` returns `None` once the slot has been closed (explicit `stdin().close()`, or implicitly by
a wait helper or `kill()`). Closing must continue to drop the pipe and signal EOF to the child.

## Testing conventions

- Two-tier layout, picked by what is being tested:
  - **Behavior tests against the public API live in the top-level `tests/` integration suite.**
    Helpers (process scripts, builder shortcuts, common assertions) are shared via
    `tests/common/mod.rs`. Spawn-and-wait, termination, output-collection, replay, and
    drop-guard contracts belong here; integration tests prove the surface a downstream user
    actually sees.
  - **Unit tests of internal-only helpers stay inline as `#[cfg(test)] mod tests` beside the
    production code.** When an inline module gets unwieldy, promote it to a sibling
    `{module_name}_tests.rs`. A single `mod tests` per file with inner `mod <subject>`
    sub-modules is the layout used here; do not repeat the subject prefix in test names.
- Default to the integration tier: prefer asserting an externally observable contract over
  poking at private state. Only add an inline unit test when the helper genuinely cannot be
  exercised through the public API, or when a public-API test would be far less precise about
  the property under test.
- Do not duplicate coverage between tiers. If a behavior is already exercised by a unit test on
  the helper that implements it, a second integration test that re-runs the same scenario
  through the public API earns its keep only if it catches a real wiring bug the unit test
  cannot.
- Assertions: `use assertr::prelude::*;` and `assert_that!(...)`. Do not introduce a parallel
  assertion style.
- Name tests after observable behavior (the contract being protected), not after the
  implementation detail or the fact that the test was added as a regression.
- When fixing behavior, prefer extending or broadening an existing nearby behavior-focused test
  over adding a parallel regression test that duplicates the scenario.
- Do not use a test file as a dumping ground for unrelated regression cases. A bloated test
  module is usually a signal that the production module has too many responsibilities and
  should be split.

## Required change workflow

1. Record the net release-notable effect under `## [Unreleased]` in `CHANGELOG.md`. Before adding a new item, check
   for an existing entry in the same area and update or merge instead of stacking iterations.
2. Mark SemVer-breaking entries with a leading `- **Breaking:** ...`.
3. Do not bump the crate version or README install snippets for in-progress changes; that happens at release time.
4. Update `README.md` when a change affects an example shown there.
5. Run `just verify` for final validation.

At release time: determine the next version from accumulated `## [Unreleased]` entries (any `**Breaking:**` entry forces
a breaking bump), move them into a new `## [x.y.z] - YYYY-MM-DD` section, bump the crate version, update README install
snippets to the new version, extend the comparison link list at the bottom of `CHANGELOG.md`, and re-point `[Unreleased]`
to compare from the new tag. Run `just verify` as the final gate.

## Coding notes specific to this crate

- `src/lib.rs` enables `#![warn(missing_docs)]`. New exported items need concise Rustdoc.
- `clippy::pedantic` is set to `warn` crate-wide via `Cargo.toml`. Address pedantic findings on touched code rather
  than blanket-allowing them.
- Markdown line length convention: 120 columns. Do not pre-wrap shorter than that.
