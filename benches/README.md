# Benchmarks

The benchmark suite intentionally measures only two stable performance stories:

- raw chunk delivery for `single_subscriber` and `broadcast`
- line delivery for `single_subscriber` and `broadcast`

The suite no longer includes subscriber-scaling, replay, late-subscription, writer-helper,
wait-for-line, or async-callback microbenchmarks. Those scenarios should only return as targeted
investigation benches for a concrete regression.

## Commands

- `just bench-smoke`: compile all benchmark targets without running Criterion samples
- `just bench-chunks`: run the chunk-delivery benchmark target
- `just bench-lines`: run the line-delivery benchmark target
- `just bench`: run the full benchmark suite

## Smoke Workflow

Use `cargo bench --no-run` for automation and local smoke checks. It validates the benchmark
targets compile in bench mode without turning debug benchmark execution into a CI signal.
