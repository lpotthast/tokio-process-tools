# Benchmarks

The benchmark suite intentionally keeps the routine matrix small while measuring two stable performance stories:

- raw chunk delivery for `single_subscriber` and `broadcast`
- line delivery for `single_subscriber` and `broadcast`

Each target has allocation-light `count_only` cases and practical allocation-heavy `collect_vec` cases. The `count_only`
cases retain only byte/line counts and a checksum so backend delivery cost is easier to see. The `collect_vec` cases
preserve the common user-facing collection workflows and show whether allocation and parsing dominate the
channel/backend choice in practice.

Both `best_effort/no_replay` and `reliable/no_replay` are covered. This matters because broadcast uses its
`tokio::sync::broadcast` fast path only for `best_effort/no_replay`; reliable broadcast uses the fanout replay path.
The line-delivery target also includes a small replay-specific group for reliable replay and best-effort replay with a
slow subscriber.

The suite intentionally avoids a full Cartesian product. Late-subscription, subscriber-scaling, writer-helper,
wait-for-line, additional slow-consumer, or async-callback micro-benchmarks should only return as targeted
investigation benches for a concrete regression or design decision.

## Commands

- `just bench-smoke`: compile all benchmark targets without running Criterion samples
- `just bench-chunks`: run the chunk-delivery benchmark target
- `just bench-lines`: run the line-delivery benchmark target
- `just bench`: run the full benchmark suite

## Smoke Workflow

Use `cargo bench --no-run` for automation and local smoke checks. It validates the benchmark
targets compile in bench mode without turning debug benchmark execution into a CI signal.
