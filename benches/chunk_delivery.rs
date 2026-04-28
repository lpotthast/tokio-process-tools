//! Benchmarks for raw chunk delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    AsyncChunkCollector, BestEffortDelivery, BroadcastOutputStream, Chunk, CollectedBytes,
    Consumer, Next, NoReplay, RawCollectionOptions, ReliableDelivery,
    SingleSubscriberOutputStream, WriteCollectionOptions,
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
    purpose: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct ChunkBenchResult {
    bytes: usize,
    chunks: Option<usize>,
    checksum: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ExpectedChunkResult {
    bytes: usize,
    chunks: usize,
    checksum: u64,
}

impl ExpectedChunkResult {
    fn from_payload(chunks: &support::Payload) -> Self {
        let stats = support::chunk_stats(chunks);
        Self {
            bytes: support::total_bytes(chunks),
            chunks: stats.chunks,
            checksum: stats.checksum,
        }
    }
}

impl ChunkBenchResult {
    fn from_stats(stats: &support::ChunkStats) -> Self {
        Self {
            bytes: stats.bytes,
            chunks: Some(stats.chunks),
            checksum: Some(stats.checksum),
        }
    }

    fn bytes_only(bytes: usize) -> Self {
        Self {
            bytes,
            chunks: None,
            checksum: None,
        }
    }
}

const CHUNK_BENCH_CASES: [ChunkBenchCase; 16] = [
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_BEST_EFFORT_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates single-subscriber best-effort delivery without retaining chunks or forcing drops",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COUNT_ONLY_BEST_EFFORT_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates the broadcast fast path without retaining chunks or forcing drops",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_RELIABLE_CHUNK_READ_CHUNK_SIZES,
        purpose: "isolates single-subscriber mpsc send overhead in reliable mode",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COUNT_ONLY_RELIABLE_CHUNK_READ_CHUNK_SIZES,
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
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures single-subscriber async callback handoff overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures broadcast async callback handoff overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures reliable single-subscriber async callback handoff overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::AsyncCallback,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures reliable broadcast async callback handoff overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures single-subscriber chunk writer helper overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures broadcast chunk writer helper overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures reliable single-subscriber chunk writer helper overhead",
    },
    ChunkBenchCase {
        consumer: ChunkConsumerKind::Write,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        read_chunk_sizes: &support::COLLECT_CHUNK_READ_CHUNK_SIZES,
        purpose: "measures reliable broadcast chunk writer helper overhead",
    },
];

trait ChunkBenchStream {
    fn collect_chunk_stats(&self) -> Consumer<support::ChunkStats>;

    fn collect_chunk_bytes(&self) -> Consumer<CollectedBytes>;

    fn collect_chunk_stats_async(&self) -> Consumer<support::ChunkStats>;

    fn write_chunks(&self) -> Consumer<support::CountingWrite>;
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
    D: tokio_process_tools::Delivery,
    R: tokio_process_tools::Replay,
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

    fn write_chunks(&self) -> Consumer<support::CountingWrite> {
        self.collect_chunks_into_write(
            support::CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
        .expect("single-subscriber benchmark consumer should start")
    }
}

impl ChunkBenchStream for BroadcastOutputStream<BestEffortDelivery, NoReplay> {
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

    fn write_chunks(&self) -> Consumer<support::CountingWrite> {
        self.collect_chunks_into_write(
            support::CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
    }
}

impl ChunkBenchStream for BroadcastOutputStream<ReliableDelivery, NoReplay> {
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

    fn write_chunks(&self) -> Consumer<support::CountingWrite> {
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
) -> ChunkBenchResult {
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
) -> ChunkBenchResult
where
    S: ChunkBenchStream,
{
    match consumer {
        ChunkConsumerKind::CountOnly => {
            let collector = stream.collect_chunk_stats();
            open_gate_after_subscriber_is_polling(gate).await;
            let stats = collector.wait().await.unwrap();
            ChunkBenchResult::from_stats(&stats)
        }
        ChunkConsumerKind::CollectVec => {
            let collector = stream.collect_chunk_bytes();
            open_gate_after_subscriber_is_polling(gate).await;
            let collected = collector.wait().await.unwrap();
            ChunkBenchResult::bytes_only(collected.len())
        }
        ChunkConsumerKind::AsyncCallback => {
            let collector = stream.collect_chunk_stats_async();
            open_gate_after_subscriber_is_polling(gate).await;
            let stats = collector.wait().await.unwrap();
            ChunkBenchResult::from_stats(&stats)
        }
        ChunkConsumerKind::Write => {
            let collector = stream.write_chunks();
            open_gate_after_subscriber_is_polling(gate).await;
            let sink = collector.wait().await.unwrap();
            ChunkBenchResult::bytes_only(sink.bytes)
        }
    }
}

async fn open_gate_after_subscriber_is_polling(gate: support::StartGate) {
    tokio::task::yield_now().await;
    gate.open();
}

fn assert_chunk_result(
    case: ChunkBenchCase,
    expected: ExpectedChunkResult,
    result: ChunkBenchResult,
) {
    assert_eq!(
        result.bytes,
        expected.bytes,
        "{} dropped or duplicated bytes",
        case_id(case),
    );

    black_box((
        expected.chunks,
        expected.checksum,
        result.chunks,
        result.checksum,
    ));
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
            let expected = ExpectedChunkResult::from_payload(&chunks);
            group.throughput(Throughput::Bytes(support::CHUNK_TOTAL_BYTES as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), size_label(*read_chunk_size)),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(case.purpose);
                        let result = run_chunk_case(
                            case.backend,
                            case.delivery,
                            case.consumer,
                            chunks,
                            *read_chunk_size,
                        )
                        .await;
                        assert_chunk_result(case, expected, result);
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
