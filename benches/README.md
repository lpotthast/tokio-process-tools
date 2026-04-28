# Benchmarks

The benchmark suite intentionally keeps the routine matrix small while measuring two stable performance stories:

- raw chunk delivery for `single_subscriber` and `broadcast`
- line delivery for `single_subscriber` and `broadcast`

Each target has allocation-light `count_only` cases and practical allocation-heavy `collect_vec` cases. The normal
throughput cases assert that all expected output was observed, so accidental best-effort drops fail the benchmark
instead of being counted as delivered output. The `count_only` cases retain only byte/line counts and a checksum so
backend delivery cost is easier to see. The `collect_vec` cases preserve the common user-facing collection workflows
and show whether allocation and parsing dominate the channel/backend choice in practice. In many no-drop throughput
cases, single-subscriber and broadcast are in the same range because allocation-heavy collection, line parsing, and sink
writes can dominate backend delivery overhead. The chunk target also includes async-callback and writer-helper cases for
single-subscriber helper comparisons.

Both `best_effort/no_replay` and `reliable/no_replay` are covered. Best-effort small-chunk overflow stress is kept out
of the main no-drop throughput matrix because it measures drop behavior unless the live queue is sized to retain the
whole run. This matters for broadcast in particular: `best_effort/no_replay` uses Tokio's broadcast ring, whose setup
cost is sensitive to very large capacities, while reliable broadcast uses the shared fanout/mpsc path. That can let
reliable broadcast match or beat broadcast best-effort in one-fast-subscriber benchmarks. The line-delivery target also
includes a small replay-specific group for reliable replay and best-effort replay with a slow subscriber.

The suite intentionally avoids a full Cartesian product. Late-subscription, subscriber-scaling, wait-for-line, or
additional slow-consumer micro-benchmarks should only return as targeted investigation benches for a concrete
regression or design decision.

## Commands

- `just bench-smoke`: compile all benchmark targets without running Criterion samples
- `just bench-chunks`: run the chunk-delivery benchmark target
- `just bench-lines`: run the line-delivery benchmark target
- `just bench`: run the full benchmark suite

## Smoke Workflow

Use `cargo bench --no-run` for automation and local smoke checks. It validates the benchmark
targets compile in bench mode without turning debug benchmark execution into a CI signal.
