# Keep in sync with `rust-version` in Cargo.toml and the `msrv` job in .github/workflows/ci.yml.
msrv_toolchain := "1.89.0"

# Lists all available commands.
list:
    @just --list

# Install tools and toolchains required by other recipes.
install-tools:
    rustup toolchain install {{ msrv_toolchain }}
    rustup target add x86_64-pc-windows-msvc
    cargo +stable install cargo-audit --locked
    cargo +stable install cargo-deny --locked
    cargo +stable install cargo-hack --locked
    cargo +stable install cargo-minimal-versions --locked
    cargo +stable install cargo-msrv --locked
    cargo +stable install cargo-semver-checks --locked
    cargo +stable install cargo-sort --locked

# Find the minimum supported rust version.
msrv:
    cargo msrv find

# Check if the current dependency version bounds are sufficient.
minimal-versions:
    cargo minimal-versions check --direct

# Check whether current changes require a breaking release.
semver-checks:
    cargo semver-checks

# Format all code.
fmt:
    cargo fmt --all

# Check that `fmt` would not change any code.
fmt-check:
    cargo fmt --all -- --check

# Check the code, including the Windows-specific library code.
check:
    cargo check --all-targets --all-features
    # Only `--lib`, as tests and benches use Unix-only shell helpers.
    cargo check --lib --target x86_64-pc-windows-msvc

# Check that the code still compiles on the minimum supported rust version.
check-msrv:
    cargo +{{ msrv_toolchain }} check --all-targets --locked

# Lint the code.
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run all tests, including doc tests.
test:
    cargo test

# Build the crate.
build:
    cargo build

# Build the crate documentation.
build-docs:
    RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Build and open the crate documentation for review.
open-docs:
    cargo doc --no-deps --open

# Run all benchmarks.
bench:
    cargo bench

# Compile all benchmarks without running them.
bench-smoke:
    cargo bench --no-run

# Run the chunk delivery benchmark.
bench-chunks:
    cargo bench --bench chunk_delivery

# Run the line delivery benchmark.
bench-lines:
    cargo bench --bench line_delivery

# Run security auditing.
audit:
    cargo audit --deny warnings
    cargo deny check advisories

# Update all deps, sort all Cargo.toml deps and format all code.
tidy:
    cargo update
    cargo sort
    just fmt

# Run the full non-mutating validation suite.
verify:
    just fmt-check
    just check
    just check-msrv
    just clippy
    just test
    just build
    just build-docs
    just audit
    just semver-checks
