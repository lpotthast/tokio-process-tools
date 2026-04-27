//! Benchmarks for line delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    AsyncChunkCollector, BestEffortDelivery, BroadcastOutputStream, Chunk, CollectedLines,
    Collector, LineCollectionOptions, LineParsingOptions, Next, NoReplay, ReliableDelivery,
    ReplayEnabled, SingleSubscriberOutputStream,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineConsumerKind {
    CountOnly,
    CollectVec,
}

impl LineConsumerKind {
    fn label(self) -> &'static str {
        match self {
            Self::CountOnly => "count_only",
            Self::CollectVec => "collect_vec",
        }
    }
}

#[derive(Clone, Copy)]
enum LinePayloadKind {
    Ascii,
    Utf8,
}

#[derive(Clone, Copy)]
struct LinePayloadCase {
    label: &'static str,
    payload_kind: LinePayloadKind,
    line_len: usize,
    line_count: usize,
    read_chunk_size: usize,
    purpose: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct LineBenchCase {
    consumer: LineConsumerKind,
    backend: BackendKind,
    delivery: DeliveryKind,
    purpose: &'static str,
}

const LINE_COUNT_ONLY_PAYLOADS: [LinePayloadCase; 3] = [
    LinePayloadCase {
        label: "short_ascii_256B",
        payload_kind: LinePayloadKind::Ascii,
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
        purpose: "uses small chunks so per-event backend cost is visible during line parsing",
    },
    LinePayloadCase {
        label: "short_ascii_16KiB",
        payload_kind: LinePayloadKind::Ascii,
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
        purpose: "keeps a larger-chunk parser baseline with minimal retained allocation",
    },
    LinePayloadCase {
        label: "short_utf8_256B",
        payload_kind: LinePayloadKind::Utf8,
        line_len: support::UTF8_SHORT_LINE_LEN,
        line_count: support::UTF8_SHORT_LINE_COUNT,
        read_chunk_size: support::UTF8_SHORT_LINE_SMALL_READ_CHUNK_SIZE,
        purpose: "checks non-ASCII decoding cost without storing emitted lines",
    },
];

const LINE_COLLECT_PAYLOADS: [LinePayloadCase; 3] = [
    LinePayloadCase {
        label: "short_ascii_16KiB",
        payload_kind: LinePayloadKind::Ascii,
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
        purpose: "represents common short-line collection where allocations may dominate",
    },
    LinePayloadCase {
        label: "long_ascii_16KiB",
        payload_kind: LinePayloadKind::Ascii,
        line_len: support::LONG_LINE_LEN,
        line_count: support::LONG_LINE_COUNT,
        read_chunk_size: support::LONG_LINE_READ_CHUNK_SIZE,
        purpose: "checks long-line collection with fewer emitted lines but larger strings",
    },
    LinePayloadCase {
        label: "long_utf8_64KiB",
        payload_kind: LinePayloadKind::Utf8,
        line_len: support::UTF8_LONG_LINE_LEN,
        line_count: support::UTF8_LONG_LINE_COUNT,
        read_chunk_size: support::UTF8_LONG_LINE_READ_CHUNK_SIZE,
        purpose: "keeps the allocation-heavy UTF-8 line collection baseline",
    },
];

const LINE_BENCH_CASES: [LineBenchCase; 8] = [
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        purpose: "isolates single-subscriber best-effort delivery under line parsing",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        purpose: "isolates broadcast fast-path delivery under line parsing",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        purpose: "isolates single-subscriber reliable delivery under line parsing",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        purpose: "isolates broadcast fanout reliable delivery under line parsing",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
        purpose: "checks whether line allocation hides single best-effort delivery cost",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
        purpose: "checks whether line allocation hides broadcast fast-path delivery cost",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
        purpose: "keeps the practical single-subscriber reliable line collection baseline",
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
        purpose: "keeps the practical broadcast reliable line collection baseline",
    },
];

trait LineBenchStream {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats>;

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines>;
}

impl LineBenchStream for SingleSubscriberOutputStream {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
    }
}

impl LineBenchStream for BroadcastOutputStream<BestEffortDelivery, NoReplay> {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
    }
}

impl LineBenchStream for BroadcastOutputStream<ReliableDelivery, NoReplay> {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
    }
}

impl LineBenchStream for BroadcastOutputStream<BestEffortDelivery, ReplayEnabled> {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
    }
}

impl LineBenchStream for BroadcastOutputStream<ReliableDelivery, ReplayEnabled> {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Collector<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Collector<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
    }
}

struct HangingChunkCollector;

impl AsyncChunkCollector<()> for HangingChunkCollector {
    async fn collect<'a>(&'a mut self, _chunk: Chunk, _sink: &'a mut ()) -> Next {
        std::future::pending::<Next>().await
    }
}

async fn run_line_case(
    case: LineBenchCase,
    payload: &support::Payload,
    read_chunk_size: usize,
    options: LineParsingOptions,
) -> (usize, usize, u64) {
    match (case.backend, case.delivery) {
        (BackendKind::Single, DeliveryKind::BestEffort) => {
            let stream = support::single_stream_best_effort(
                support::ChunkedReader::new(payload),
                read_chunk_size,
            );
            run_line_consumer(&stream, case.consumer, options).await
        }
        (BackendKind::Single, DeliveryKind::Reliable) => {
            let stream = support::single_stream_reliable(
                support::ChunkedReader::new(payload),
                read_chunk_size,
            );
            run_line_consumer(&stream, case.consumer, options).await
        }
        (BackendKind::Broadcast, DeliveryKind::BestEffort) => {
            let stream = support::broadcast_stream_best_effort(
                support::ChunkedReader::new(payload),
                read_chunk_size,
            );
            run_line_consumer(&stream, case.consumer, options).await
        }
        (BackendKind::Broadcast, DeliveryKind::Reliable) => {
            let stream = support::broadcast_stream_reliable(
                support::ChunkedReader::new(payload),
                read_chunk_size,
            );
            run_line_consumer(&stream, case.consumer, options).await
        }
    }
}

async fn run_line_consumer<S>(
    stream: &S,
    consumer: LineConsumerKind,
    options: LineParsingOptions,
) -> (usize, usize, u64)
where
    S: LineBenchStream,
{
    match consumer {
        LineConsumerKind::CountOnly => {
            let stats = stream.collect_line_stats(options).wait().await.unwrap();
            (stats.bytes, stats.lines, stats.checksum)
        }
        LineConsumerKind::CollectVec => {
            let collected = stream.collect_line_buffer(options).wait().await.unwrap();
            (collected.iter().map(String::len).sum(), collected.len(), 0)
        }
    }
}

fn build_payload(case: LinePayloadCase) -> support::Payload {
    match case.payload_kind {
        LinePayloadKind::Ascii => {
            support::build_line_payload(case.line_len, case.line_count, case.read_chunk_size)
        }
        LinePayloadKind::Utf8 => {
            support::build_utf8_line_payload(case.line_len, case.line_count, case.read_chunk_size)
        }
    }
}

fn payloads_for(consumer: LineConsumerKind) -> &'static [LinePayloadCase] {
    match consumer {
        LineConsumerKind::CountOnly => &LINE_COUNT_ONLY_PAYLOADS,
        LineConsumerKind::CollectVec => &LINE_COLLECT_PAYLOADS,
    }
}

fn case_id(case: LineBenchCase) -> String {
    format!(
        "{}/{}/{}",
        case.consumer.label(),
        case.backend.label(),
        case.delivery.label()
    )
}

fn bench_line_delivery(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("line_delivery");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for case in LINE_BENCH_CASES {
        for payload_case in payloads_for(case.consumer) {
            let payload = build_payload(*payload_case);
            group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), payload_case.label),
                &payload,
                |b, payload| {
                    b.to_async(&rt).iter(|| async {
                        black_box(case.purpose);
                        black_box(payload_case.purpose);
                        black_box(
                            run_line_case(case, payload, payload_case.read_chunk_size, options)
                                .await,
                        )
                    });
                },
            );
        }
    }

    group.finish();
}

async fn run_reliable_replay_line_case(
    payload: &support::Payload,
    read_chunk_size: usize,
    options: LineParsingOptions,
) -> (usize, usize, u64) {
    let stream = support::broadcast_stream_reliable_replay(
        support::ChunkedReader::new(payload),
        read_chunk_size,
    );
    run_line_consumer(&stream, LineConsumerKind::CountOnly, options).await
}

async fn run_best_effort_replay_slow_subscriber_case(
    payload: &support::Payload,
    read_chunk_size: usize,
    options: LineParsingOptions,
) -> (usize, usize, u64) {
    let (reader, gate) = support::GatedChunkedReader::new(payload);
    let stream = support::broadcast_stream_best_effort_replay(reader, read_chunk_size);
    let _slow_subscriber = stream.collect_chunks_async((), HangingChunkCollector);
    let collector = stream.collect_line_stats(options);

    gate.open();
    let stats = collector.wait().await.unwrap();
    (stats.bytes, stats.lines, stats.checksum)
}

fn bench_line_delivery_replay(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("line_delivery_replay");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for read_chunk_size in [
        support::UTF8_SHORT_LINE_SMALL_READ_CHUNK_SIZE,
        support::UTF8_SHORT_LINE_LARGE_READ_CHUNK_SIZE,
    ] {
        let payload = support::build_utf8_line_payload(
            support::UTF8_SHORT_LINE_LEN,
            support::UTF8_SHORT_LINE_COUNT,
            read_chunk_size,
        );
        group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));
        group.bench_with_input(
            BenchmarkId::new(
                "count_only/broadcast/reliable_replay",
                format!("short_utf8_{read_chunk_size}B"),
            ),
            &payload,
            |b, payload| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        run_reliable_replay_line_case(payload, read_chunk_size, options).await,
                    )
                });
            },
        );
    }

    let payload = support::build_line_payload(
        support::SHORT_LINE_LEN,
        support::SHORT_LINE_COUNT,
        support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
    );
    group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));
    group.bench_with_input(
        BenchmarkId::new(
            "count_only/broadcast/best_effort_replay_slow_subscriber",
            "short_ascii_256B",
        ),
        &payload,
        |b, payload| {
            b.to_async(&rt).iter(|| async {
                black_box(
                    run_best_effort_replay_slow_subscriber_case(
                        payload,
                        support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
                        options,
                    )
                    .await,
                )
            });
        },
    );

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1));
    targets = bench_line_delivery, bench_line_delivery_replay
);
criterion_main!(benches);
