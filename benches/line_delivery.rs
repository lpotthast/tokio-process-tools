//! Benchmarks for line delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tokio_process_tools::LineParsingOptions;

#[derive(Clone, Copy)]
struct LineWorkload {
    label: &'static str,
    line_len: usize,
    line_count: usize,
    read_chunk_size: usize,
}

fn bench_line_delivery(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("line_delivery");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for workload in [
        LineWorkload {
            label: "short_lines",
            line_len: support::SHORT_LINE_LEN,
            line_count: support::SHORT_LINE_COUNT,
            read_chunk_size: support::SHORT_LINE_READ_CHUNK_SIZE,
        },
        LineWorkload {
            label: "long_lines",
            line_len: support::LONG_LINE_LEN,
            line_count: support::LONG_LINE_COUNT,
            read_chunk_size: support::LONG_LINE_READ_CHUNK_SIZE,
        },
    ] {
        let chunks = support::build_line_payload(
            workload.line_len,
            workload.line_count,
            workload.read_chunk_size,
        );
        group.throughput(Throughput::Bytes(support::total_bytes(&chunks) as u64));

        group.bench_with_input(
            BenchmarkId::new("single", workload.label),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    let stream = support::single_stream(
                        support::ChunkedReader::new(chunks),
                        workload.read_chunk_size,
                    );
                    let delivered_lines = stream
                        .collect_all_lines_into_vec_trusted(options)
                        .wait()
                        .await
                        .unwrap()
                        .len();
                    black_box(delivered_lines)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast", workload.label),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    let stream = support::broadcast_stream(
                        support::ChunkedReader::new(chunks),
                        workload.read_chunk_size,
                    );
                    let delivered_lines = stream
                        .collect_all_lines_into_vec_trusted(options)
                        .wait()
                        .await
                        .unwrap()
                        .len();
                    black_box(delivered_lines)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1));
    targets = bench_line_delivery
);
criterion_main!(benches);
