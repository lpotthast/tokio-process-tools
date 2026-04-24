default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test

build:
    cargo build

doc:
    cargo doc

bench:
    cargo bench

bench-smoke:
    cargo bench --no-run

bench-chunks:
    cargo bench --bench chunk_delivery

bench-lines:
    cargo bench --bench line_delivery

# Run the full validation suite: check, clippy, test, build, doc
verify: fmt-check lint test build doc
