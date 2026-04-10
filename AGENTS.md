# Repository Guidelines

## Project Structure & Module Organization

This repository is a small Rust library crate. Core code lives in `src/`, with the public API exported from `src/lib.rs`. Process orchestration is centered around `src/process.rs` and `src/process_handle.rs`; error types live in `src/error.rs`; output handling is split across `src/output.rs`, `src/collector.rs`, `src/inspector.rs`, and `src/output_stream/`. Platform-specific termination logic is isolated in `src/signal.rs`.

The crate is a thin async wrapper around `tokio::process::Child` that adds structured output handling, graceful termination, and safe drop semantics. Keep these architectural constraints in mind when changing behavior:

- `Process` wraps a `tokio::process::Command` and exposes `.spawn_broadcast()` and `.spawn_single_subscriber()`. Each returns a typed `ProcessHandle<O>` where `O: OutputStream`, so the stream backend choice is compile-time, not runtime.
- `src/output_stream/broadcast.rs` is the multi-consumer backend built on `tokio::sync::broadcast`. `src/output_stream/single_subscriber.rs` is the lower-overhead single-consumer backend and must continue to reject a second subscriber.
- Shared chunk and line parsing behavior lives in `src/output_stream/mod.rs` and `src/output_stream/impls.rs`. `LineParsingOptions`, `LineOverflowBehavior`, EOF handling, and surfacing a final unterminated line are load-bearing behavior.
- `Inspector` and `Collector` each spawn Tokio tasks around stream consumption. Their handles must continue to support clean `wait()` and `cancel()` behavior.
- `ProcessHandle` drop semantics are intentionally strict. Dropping a live handle without explicit cleanup triggers best-effort cleanup and a panic via `PanicOnDrop`. The opt-out paths are `wait_for_completion*`, `terminate*`, `.terminate_on_drop(...)`, or `must_not_be_terminated()`.
- `TerminateOnDrop` requires a multi-threaded Tokio runtime.
- Termination escalates across platform-specific signals in `src/signal.rs`. Preserve the current Unix and Windows behavior when modifying shutdown logic.
- `ProcessHandle::stdin()` exposes piped stdin via the `Stdin` enum. Closing stdin must continue to signal EOF to the child process.

The `pub use` block at the top of `src/lib.rs` is the canonical list of public types and the fastest way to orient yourself before making API changes. Use `README.md` as the primary user-facing behavior and example reference; it is also re-exported as crate-level Rustdoc.

## Build, Test, and Development Commands

- `cargo check`: fast validation during iteration.
- `cargo build`: compile the library in debug mode.
- `cargo test`: run the full unit test suite.
- `cargo test <test_name> -- --nocapture`: run a single test by name and show println or tracing output when needed.
- `cargo clippy --all-targets --all-features -- -D warnings`: enforce lint cleanliness before opening a PR.
- `cargo fmt --all`: apply standard Rust formatting.
- `cargo doc --no-deps`: rebuild local API docs when changing public behavior or docs.

MSRV is `1.89.0` and the crate uses Rust edition `2024` as declared in `Cargo.toml`.

## Coding Style & Naming Conventions

Follow standard Rust style with `rustfmt` defaults: 4-space indentation, trailing commas where rustfmt inserts them, and module/file names in `snake_case`. Types and traits use `UpperCamelCase`; functions, methods, and local variables use `snake_case`.

Keep public API additions documented: `src/lib.rs` enables `#![warn(missing_docs)]`, so new exported items should include concise Rustdoc comments. Prefer small, focused modules over large cross-cutting edits.

## Testing Guidelines

There is no top-level `tests/` directory; most tests are inline `#[cfg(test)]` modules beside the code they verify. Add tests next to the affected module.

This crate uses `#[tokio::test]` heavily. Any test that exercises `TerminateOnDrop` must use `#[tokio::test(flavor = "multi_thread")]`, because the single-threaded runtime cannot drive the async drop path correctly.

Assertions use the `assertr` crate consistently via `assert_that!(...)`. Other relevant test dependencies include `tracing-test`, `tempfile`, `mockall`, and `tokio-test`.

Name tests after observable behavior, for example `wait_with_output` or `single_subscriber_panics_on_multiple_consumers`. Run `cargo test` locally before pushing, and add regression coverage for bug fixes.

## Commit & Pull Request Guidelines

Recent history favors short, imperative commit subjects such as `Allow programmatic stdin handling` or `Update documentation`. Keep commits focused and descriptive. For pull requests, include a clear summary, note any API or behavior changes, update `README.md` or `CHANGELOG.md` when relevant, and confirm `cargo fmt`, `cargo clippy`, and `cargo test` all pass.
