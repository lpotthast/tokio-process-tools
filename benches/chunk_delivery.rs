//! Benchmarks for raw chunk delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    AsyncChunkCollector, BroadcastOutputStream, Chunk, CollectedBytes, Consumer, Delivery, Next,
    RawCollectionOptions, Replay, SingleSubscriberOutputStream, SinkWriteError,
    WriteCollectionOptions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkConsumerKind {
    CountOnly,
    CollectVec,
    AsyncCallback,
    Write,
}

impl ChunkConsumerKind {
    fn label(self) -> &'static str {
        match self {
            Self::CountOnly => "count_only",
            Self::CollectVec => "collect_vec",
            Self::AsyncCallback => "async_callback",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ChunkBenchCase {
    consumer: ChunkConsumerKind,
    backend: BackendKind,
    delivery: DeliveryKind,
    read_chunk_sizes: &'static [usize],
}

const CHUNK_BENCH_CASES: [ChunkBenchCase; 16] = [
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_BEST_EFFORT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_BEST_EFFORT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_RELIABLE_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_RELIABLE_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
    },
];

trait ChunkBenchStream {
    fn collect_chunk_stats(&self) -> Consumer<support::ChunkStats>;

    fn collect_chunk_bytes(&self) -> Consumer<CollectedBytes>;

    fn collect_chunk_stats_async(&self) -> Consumer<support::ChunkStats>;

    fn write_chunks(&self) -> Consumer<Result<support::CountingWrite, SinkWriteError>>;
}

struct AsyncChunkStatsCollector;

impl AsyncChunkCollector<support::ChunkStats> for AsyncChunkStatsCollector {
    async fn collect<'a>(&'a mut self, chunk: Chunk, stats: &'a mut support::ChunkStats) -> Next {
        stats.observe(chunk.as_ref());
        Next::Continue
    }
}

impl<D, R> ChunkBenchStream for SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn collect_chunk_stats(&self) -> Consumer<support::ChunkStats> {
        self.collect_chunks(
            support::ChunkStats::default(),
            |chunk, stats: &mut support::ChunkStats| {
                stats.observe(chunk.as_ref());
            },
        )
        .expect("single-subscriber benchmark consumer should start")
    }

    fn collect_chunk_bytes(&self) -> Consumer<CollectedBytes> {
        self.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
            .expect("single-subscriber benchmark consumer should start")
    }

    fn collect_chunk_stats_async(&self) -> Consumer<support::ChunkStats> {
        self.collect_chunks_async(support::ChunkStats::default(), AsyncChunkStatsCollector)
            .expect("single-subscriber benchmark consumer should start")
    }

    fn write_chunks(&self) -> Consumer<Result<support::CountingWrite, SinkWriteError>> {
        self.collect_chunks_into_write(
            support::CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
        .expect("single-subscriber benchmark consumer should start")
    }
}

impl<D, R> ChunkBenchStream for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn collect_chunk_stats(&self) -> Consumer<support::ChunkStats> {
        self.collect_chunks(
            support::ChunkStats::default(),
            |chunk, stats: &mut support::ChunkStats| {
                stats.observe(chunk.as_ref());
            },
        )
    }

    fn collect_chunk_bytes(&self) -> Consumer<CollectedBytes> {
        self.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
    }

    fn collect_chunk_stats_async(&self) -> Consumer<support::ChunkStats> {
        self.collect_chunks_async(support::ChunkStats::default(), AsyncChunkStatsCollector)
    }

    fn write_chunks(&self) -> Consumer<Result<support::CountingWrite, SinkWriteError>> {
        self.collect_chunks_into_write(
            support::CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
    }
}

async fn run_chunk_case(
    backend: BackendKind,
    delivery: DeliveryKind,
    consumer: ChunkConsumerKind,
    chunks: &support::Payload,
    read_chunk_size: usize,
) -> usize {
    match (backend, delivery) {
        (BackendKind::Single, DeliveryKind::BestEffort) => {
            let (reader, gate) = support::GatedChunkedReader::new(chunks);
            let stream = support::single_stream_best_effort(reader, read_chunk_size);
            run_chunk_consumer(&stream, consumer, gate).await
        }
        (BackendKind::Single, DeliveryKind::Reliable) => {
            let (reader, gate) = support::GatedChunkedReader::new(chunks);
            let stream = support::single_stream_reliable(reader, read_chunk_size);
            run_chunk_consumer(&stream, consumer, gate).await
        }
        (BackendKind::Broadcast, DeliveryKind::BestEffort) => {
            let (reader, gate) = support::GatedChunkedReader::new(chunks);
            let stream = support::broadcast_stream_best_effort(reader, read_chunk_size);
            run_chunk_consumer(&stream, consumer, gate).await
        }
        (BackendKind::Broadcast, DeliveryKind::Reliable) => {
            let (reader, gate) = support::GatedChunkedReader::new(chunks);
            let stream = support::broadcast_stream_reliable(reader, read_chunk_size);
            run_chunk_consumer(&stream, consumer, gate).await
        }
    }
}

async fn run_chunk_consumer<S>(
    stream: &S,
    consumer: ChunkConsumerKind,
    gate: support::StartGate,
) -> usize
where
    S: ChunkBenchStream,
{
    match consumer {
        ChunkConsumerKind::CountOnly => {
            let collector = stream.collect_chunk_stats();
            open_gate_after_subscriber_is_polling(gate).await;
            let stats = collector.wait().await.unwrap();
            // Keep the per-chunk content reads alive so the observer cannot be elided.
            black_box((stats.chunks, stats.checksum));
            stats.bytes
        }
        ChunkConsumerKind::CollectVec => {
            let collector = stream.collect_chunk_bytes();
            open_gate_after_subscriber_is_polling(gate).await;
            let collected = collector.wait().await.unwrap();
            collected.len()
        }
        ChunkConsumerKind::AsyncCallback => {
            let collector = stream.collect_chunk_stats_async();
            open_gate_after_subscriber_is_polling(gate).await;
            let stats = collector.wait().await.unwrap();
            black_box((stats.chunks, stats.checksum));
            stats.bytes
        }
        ChunkConsumerKind::Write => {
            let collector = stream.write_chunks();
            open_gate_after_subscriber_is_polling(gate).await;
            let sink = collector.wait().await.unwrap().unwrap();
            sink.bytes
        }
    }
}

async fn open_gate_after_subscriber_is_polling(gate: support::StartGate) {
    tokio::task::yield_now().await;
    gate.open();
}

fn assert_chunk_result(case: ChunkBenchCase, expected_bytes: usize, result_bytes: usize) {
    assert_eq!(
        result_bytes,
        expected_bytes,
        "{} dropped or duplicated bytes",
        case_id(case),
    );
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

    let mut payloads: HashMap<usize, support::Payload> = HashMap::new();
    for case in CHUNK_BENCH_CASES {
        for read_chunk_size in case.read_chunk_sizes {
            payloads.entry(*read_chunk_size).or_insert_with(|| {
                support::build_chunk_payload(support::CHUNK_TOTAL_BYTES, *read_chunk_size)
            });
        }
    }
    let expected_bytes = support::CHUNK_TOTAL_BYTES;

    for case in CHUNK_BENCH_CASES {
        for read_chunk_size in case.read_chunk_sizes {
            let chunks = payloads.get(read_chunk_size).expect("payload precomputed");
            group.throughput(Throughput::Bytes(support::CHUNK_TOTAL_BYTES as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), size_label(*read_chunk_size)),
                chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        let result = run_chunk_case(
                            case.backend,
                            case.delivery,
                            case.consumer,
                            chunks,
                            *read_chunk_size,
                        )
                        .await;
                        assert_chunk_result(case, expected_bytes, result);
                        black_box(result)
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
