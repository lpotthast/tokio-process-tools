//! Benchmarks for chunk delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;

fn bench_chunk_delivery(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("chunk_delivery");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for read_chunk_size in support::CHUNK_READ_CHUNK_SIZES {
        let chunks = support::build_chunk_payload(support::CHUNK_TOTAL_BYTES, read_chunk_size);
        group.throughput(Throughput::Bytes(support::CHUNK_TOTAL_BYTES as u64));

        group.bench_with_input(
            BenchmarkId::new("single", format!("{}KiB", read_chunk_size / 1024)),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    let stream = support::single_stream(
                        support::ChunkedReader::new(chunks),
                        read_chunk_size,
                    );
                    let delivered_bytes = stream
                        .collect_all_chunks_into_vec_trusted()
                        .wait()
                        .await
                        .unwrap()
                        .len();
                    black_box(delivered_bytes)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast", format!("{}KiB", read_chunk_size / 1024)),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    let stream = support::broadcast_stream(
                        support::ChunkedReader::new(chunks),
                        read_chunk_size,
                    );
                    let delivered_bytes = stream
                        .collect_all_chunks_into_vec_trusted()
                        .wait()
                        .await
                        .unwrap()
                        .len();
                    black_box(delivered_bytes)
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1));
    targets = bench_chunk_delivery
);
criterion_main!(benches);
