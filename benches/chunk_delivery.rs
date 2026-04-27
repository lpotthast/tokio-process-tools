//! Benchmarks for raw chunk delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    BestEffortDelivery, BroadcastOutputStream, CollectedBytes, Collector, NoReplay,
    RawCollectionOptions, ReliableDelivery, SingleSubscriberOutputStream,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkConsumerKind {
    CountOnly,
    CollectVec,
}

impl ChunkConsumerKind {
    fn label(self) -> &'static str {
        match self {
            Self::CountOnly => "count_only",
            Self::CollectVec => "collect_vec",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ChunkBenchCase {
    consumer: ChunkConsumerKind,
    backend: BackendKind,
    delivery: DeliveryKind,
    read_chunk_sizes: &'static [usize],
    purpose: &'static str,
}

const CHUNK_BENCH_CASES: [ChunkBenchCase; 8] = [
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates single-subscriber try_send overhead without retaining chunks",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates the broadcast fast path without retaining chunks",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates single-subscriber mpsc send overhead in reliable mode",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates broadcast fanout overhead in reliable mode",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "checks whether allocation-heavy chunk collection hides single best-effort cost",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "checks whether allocation-heavy chunk collection hides broadcast fast-path cost",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "keeps the practical single-subscriber reliable collection baseline",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "keeps the practical broadcast reliable collection baseline",
    },
];

trait ChunkBenchStream {
    fn collect_chunk_stats(&self) -> Collector<support::ChunkStats>;

    fn collect_chunk_bytes(&self) -> Collector<CollectedBytes>;
}

impl ChunkBenchStream for SingleSubscriberOutputStream {
    fn collect_chunk_stats(&self) -> Collector<support::ChunkStats> {
        self.collect_chunks(
            support::ChunkStats::default(),
            |chunk, stats: &mut support::ChunkStats| {
                stats.observe(chunk.as_ref());
            },
        )
    }

    fn collect_chunk_bytes(&self) -> Collector<CollectedBytes> {
        self.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
    }
}

impl ChunkBenchStream for BroadcastOutputStream<BestEffortDelivery, NoReplay> {
    fn collect_chunk_stats(&self) -> Collector<support::ChunkStats> {
        self.collect_chunks(
            support::ChunkStats::default(),
            |chunk, stats: &mut support::ChunkStats| {
                stats.observe(chunk.as_ref());
            },
        )
    }

    fn collect_chunk_bytes(&self) -> Collector<CollectedBytes> {
        self.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
    }
}

impl ChunkBenchStream for BroadcastOutputStream<ReliableDelivery, NoReplay> {
    fn collect_chunk_stats(&self) -> Collector<support::ChunkStats> {
        self.collect_chunks(
            support::ChunkStats::default(),
            |chunk, stats: &mut support::ChunkStats| {
                stats.observe(chunk.as_ref());
            },
        )
    }

    fn collect_chunk_bytes(&self) -> Collector<CollectedBytes> {
        self.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
    }
}

async fn run_chunk_case(
    backend: BackendKind,
    delivery: DeliveryKind,
    consumer: ChunkConsumerKind,
    chunks: &support::Payload,
    read_chunk_size: usize,
) -> (usize, usize, u64) {
    match (backend, delivery) {
        (BackendKind::Single, DeliveryKind::BestEffort) => {
            let stream = support::single_stream_best_effort(
                support::ChunkedReader::new(chunks),
                read_chunk_size,
            );
            run_chunk_consumer(&stream, consumer).await
        }
        (BackendKind::Single, DeliveryKind::Reliable) => {
            let stream = support::single_stream_reliable(
                support::ChunkedReader::new(chunks),
                read_chunk_size,
            );
            run_chunk_consumer(&stream, consumer).await
        }
        (BackendKind::Broadcast, DeliveryKind::BestEffort) => {
            let stream = support::broadcast_stream_best_effort(
                support::ChunkedReader::new(chunks),
                read_chunk_size,
            );
            run_chunk_consumer(&stream, consumer).await
        }
        (BackendKind::Broadcast, DeliveryKind::Reliable) => {
            let stream = support::broadcast_stream_reliable(
                support::ChunkedReader::new(chunks),
                read_chunk_size,
            );
            run_chunk_consumer(&stream, consumer).await
        }
    }
}

async fn run_chunk_consumer<S>(stream: &S, consumer: ChunkConsumerKind) -> (usize, usize, u64)
where
    S: ChunkBenchStream,
{
    match consumer {
        ChunkConsumerKind::CountOnly => {
            let stats = stream.collect_chunk_stats().wait().await.unwrap();
            (stats.bytes, stats.chunks, stats.checksum)
        }
        ChunkConsumerKind::CollectVec => {
            let collected = stream.collect_chunk_bytes().wait().await.unwrap();
            (collected.len(), 0, 0)
        }
    }
}

fn case_id(case: ChunkBenchCase) -> String {
    format!(
        "{}/{}/{}",
        case.consumer.label(),
        case.backend.label(),
        case.delivery.label()
    )
}

fn size_label(read_chunk_size: usize) -> String {
    if read_chunk_size < 1024 {
        format!("{read_chunk_size}B")
    } else {
        format!("{}KiB", read_chunk_size / 1024)
    }
}

fn bench_chunk_delivery(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("chunk_delivery");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for case in CHUNK_BENCH_CASES {
        for read_chunk_size in case.read_chunk_sizes {
            let chunks = support::build_chunk_payload(support::CHUNK_TOTAL_BYTES, *read_chunk_size);
            group.throughput(Throughput::Bytes(support::CHUNK_TOTAL_BYTES as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), size_label(*read_chunk_size)),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(case.purpose);
                        black_box(
                            run_chunk_case(
                                case.backend,
                                case.delivery,
                                case.consumer,
                                chunks,
                                *read_chunk_size,
                            )
                            .await,
                        )
                    });
                },
            );
        }
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1));
    targets = bench_chunk_delivery
);
criterion_main!(benches);
