//! Benchmarks for line delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use tokio_process_tools::{LineCollectionOptions, LineParsingOptions};

#[derive(Clone, Copy)]
enum LinePayloadKind {
    Ascii,
    Utf8,
}

#[derive(Clone, Copy)]
struct LineWorkload {
    label: &'static str,
    payload_kind: LinePayloadKind,
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
            payload_kind: LinePayloadKind::Ascii,
            line_len: support::SHORT_LINE_LEN,
            line_count: support::SHORT_LINE_COUNT,
            read_chunk_size: support::SHORT_LINE_READ_CHUNK_SIZE,
        },
        LineWorkload {
            label: "long_lines",
            payload_kind: LinePayloadKind::Ascii,
            line_len: support::LONG_LINE_LEN,
            line_count: support::LONG_LINE_COUNT,
            read_chunk_size: support::LONG_LINE_READ_CHUNK_SIZE,
        },
        LineWorkload {
            label: "utf8_short_lines",
            payload_kind: LinePayloadKind::Utf8,
            line_len: support::UTF8_SHORT_LINE_LEN,
            line_count: support::UTF8_SHORT_LINE_COUNT,
            read_chunk_size: support::UTF8_SHORT_LINE_READ_CHUNK_SIZE,
        },
        LineWorkload {
            label: "utf8_long_lines",
            payload_kind: LinePayloadKind::Utf8,
            line_len: support::UTF8_LONG_LINE_LEN,
            line_count: support::UTF8_LONG_LINE_COUNT,
            read_chunk_size: support::UTF8_LONG_LINE_READ_CHUNK_SIZE,
        },
    ] {
        let chunks = match workload.payload_kind {
            LinePayloadKind::Ascii => support::build_line_payload(
                workload.line_len,
                workload.line_count,
                workload.read_chunk_size,
            ),
            LinePayloadKind::Utf8 => support::build_utf8_line_payload(
                workload.line_len,
                workload.line_count,
                workload.read_chunk_size,
            ),
        };
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
                        .collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
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
                        .collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
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
