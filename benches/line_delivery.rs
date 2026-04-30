//! Benchmarks for line delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    AsyncChunkCollector, BroadcastOutputStream, Chunk, CollectedLines, Consumer,
    LineCollectionOptions, LineParsingOptions, Next, SingleSubscriberOutputStream,
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
struct LinePayloadCase {
    label: &'static str,
    line_len: usize,
    line_count: usize,
    read_chunk_size: usize,
}

#[derive(Debug, Clone, Copy)]
struct LineBenchCase {
    consumer: LineConsumerKind,
    backend: BackendKind,
    delivery: DeliveryKind,
}

#[derive(Debug, Clone, Copy)]
struct LineBenchResult {
    bytes: usize,
    lines: usize,
}

impl LineBenchResult {
    fn from_stats(stats: &support::LineStats) -> Self {
        // Keep the per-line checksum read alive so the parser cannot elide line content.
        black_box(stats.checksum);
        Self {
            bytes: stats.bytes,
            lines: stats.lines,
        }
    }

    fn from_lines(lines: &CollectedLines) -> Self {
        Self {
            bytes: lines.iter().map(String::len).sum(),
            lines: lines.len(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ExpectedLineResult {
    bytes: usize,
    lines: usize,
}

impl ExpectedLineResult {
    fn from_payload(payload: &support::Payload) -> Self {
        let stats = support::line_stats(payload);
        Self {
            bytes: stats.bytes,
            lines: stats.lines,
        }
    }
}

const LINE_COUNT_ONLY_PAYLOADS: [LinePayloadCase; 2] = [
    LinePayloadCase {
        label: "short_256B",
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
    },
    LinePayloadCase {
        label: "short_16KiB",
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
    },
];

const LINE_COUNT_ONLY_BEST_EFFORT_PAYLOADS: [LinePayloadCase; 1] = [LinePayloadCase {
    label: "short_16KiB",
    line_len: support::SHORT_LINE_LEN,
    line_count: support::SHORT_LINE_COUNT,
    read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
}];

const LINE_COLLECT_PAYLOADS: [LinePayloadCase; 3] = [
    LinePayloadCase {
        label: "short_16KiB",
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
    },
    LinePayloadCase {
        label: "long_16KiB",
        line_len: support::LONG_LINE_LEN,
        line_count: support::LONG_LINE_COUNT,
        read_chunk_size: support::LONG_LINE_SMALL_READ_CHUNK_SIZE,
    },
    LinePayloadCase {
        label: "long_64KiB",
        line_len: support::LONG_LINE_LEN,
        line_count: support::LONG_LINE_COUNT,
        read_chunk_size: support::LONG_LINE_LARGE_READ_CHUNK_SIZE,
    },
];

const LINE_BENCH_CASES: [LineBenchCase; 8] = [
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CountOnly,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::BestEffort,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::BestEffort,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Single,
        delivery: DeliveryKind::Reliable,
    },
    LineBenchCase {
        consumer: LineConsumerKind::CollectVec,
        backend: BackendKind::Broadcast,
        delivery: DeliveryKind::Reliable,
    },
];

trait LineBenchStream {
    fn collect_line_stats(&self, options: LineParsingOptions) -> Consumer<support::LineStats>;

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Consumer<CollectedLines>;
}

impl<D, R> LineBenchStream for SingleSubscriberOutputStream<D, R>
where
    D: tokio_process_tools::Delivery,
    R: tokio_process_tools::Replay,
{
    fn collect_line_stats(&self, options: LineParsingOptions) -> Consumer<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
        .expect("single-subscriber benchmark consumer should start")
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Consumer<CollectedLines> {
        self.collect_lines_into_vec(options, LineCollectionOptions::TrustedUnbounded)
            .expect("single-subscriber benchmark consumer should start")
    }
}

impl<D, R> LineBenchStream for BroadcastOutputStream<D, R>
where
    D: tokio_process_tools::Delivery,
    R: tokio_process_tools::Replay,
{
    fn collect_line_stats(&self, options: LineParsingOptions) -> Consumer<support::LineStats> {
        self.collect_lines(
            support::LineStats::default(),
            |line, stats: &mut support::LineStats| {
                stats.observe(line.as_ref());
                Next::Continue
            },
            options,
        )
    }

    fn collect_line_buffer(&self, options: LineParsingOptions) -> Consumer<CollectedLines> {
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
) -> LineBenchResult {
    match (case.backend, case.delivery) {
        (BackendKind::Single, DeliveryKind::BestEffort) => {
            let (reader, gate) = support::GatedChunkedReader::new(payload);
            let stream = support::single_stream_best_effort(reader, read_chunk_size);
            run_line_consumer(&stream, case.consumer, options, gate).await
        }
        (BackendKind::Single, DeliveryKind::Reliable) => {
            let (reader, gate) = support::GatedChunkedReader::new(payload);
            let stream = support::single_stream_reliable(reader, read_chunk_size);
            run_line_consumer(&stream, case.consumer, options, gate).await
        }
        (BackendKind::Broadcast, DeliveryKind::BestEffort) => {
            let (reader, gate) = support::GatedChunkedReader::new(payload);
            let stream = support::broadcast_stream_best_effort(reader, read_chunk_size);
            run_line_consumer(&stream, case.consumer, options, gate).await
        }
        (BackendKind::Broadcast, DeliveryKind::Reliable) => {
            let (reader, gate) = support::GatedChunkedReader::new(payload);
            let stream = support::broadcast_stream_reliable(reader, read_chunk_size);
            run_line_consumer(&stream, case.consumer, options, gate).await
        }
    }
}

async fn run_line_consumer<S>(
    stream: &S,
    consumer: LineConsumerKind,
    options: LineParsingOptions,
    gate: support::StartGate,
) -> LineBenchResult
where
    S: LineBenchStream,
{
    match consumer {
        LineConsumerKind::CountOnly => {
            let collector = stream.collect_line_stats(options);
            open_gate_after_subscriber_is_polling(gate).await;
            let stats = collector.wait().await.unwrap();
            LineBenchResult::from_stats(&stats)
        }
        LineConsumerKind::CollectVec => {
            let collector = stream.collect_line_buffer(options);
            open_gate_after_subscriber_is_polling(gate).await;
            let collected = collector.wait().await.unwrap();
            LineBenchResult::from_lines(&collected)
        }
    }
}

async fn open_gate_after_subscriber_is_polling(gate: support::StartGate) {
    tokio::task::yield_now().await;
    gate.open();
}

fn assert_line_result(case: LineBenchCase, expected: ExpectedLineResult, result: LineBenchResult) {
    assert_eq!(
        result.bytes,
        expected.bytes,
        "{} dropped or duplicated line bytes",
        case_id(case),
    );
    assert_eq!(
        result.lines,
        expected.lines,
        "{} delivered an unexpected line count",
        case_id(case),
    );
}

fn assert_best_effort_slow_subscriber_result(
    expected: ExpectedLineResult,
    result: LineBenchResult,
) {
    assert!(
        result.bytes <= expected.bytes,
        "best-effort slow-subscriber case delivered more bytes than the source payload"
    );
    assert!(
        result.lines <= expected.lines,
        "best-effort slow-subscriber case delivered more lines than the source payload"
    );
}

fn payloads_for(case: LineBenchCase) -> &'static [LinePayloadCase] {
    match (case.consumer, case.delivery) {
        (LineConsumerKind::CountOnly, DeliveryKind::BestEffort) => {
            &LINE_COUNT_ONLY_BEST_EFFORT_PAYLOADS
        }
        (LineConsumerKind::CountOnly, DeliveryKind::Reliable) => &LINE_COUNT_ONLY_PAYLOADS,
        (LineConsumerKind::CollectVec, _) => &LINE_COLLECT_PAYLOADS,
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

    let mut payloads: HashMap<(usize, usize, usize), (support::Payload, ExpectedLineResult)> =
        HashMap::new();
    for payload_case in LINE_COUNT_ONLY_PAYLOADS
        .iter()
        .chain(LINE_COUNT_ONLY_BEST_EFFORT_PAYLOADS.iter())
        .chain(LINE_COLLECT_PAYLOADS.iter())
    {
        let key = (
            payload_case.line_len,
            payload_case.line_count,
            payload_case.read_chunk_size,
        );
        payloads.entry(key).or_insert_with(|| {
            let payload = support::build_line_payload(
                payload_case.line_len,
                payload_case.line_count,
                payload_case.read_chunk_size,
            );
            let expected = ExpectedLineResult::from_payload(&payload);
            (payload, expected)
        });
    }

    for case in LINE_BENCH_CASES {
        for payload_case in payloads_for(case) {
            let key = (
                payload_case.line_len,
                payload_case.line_count,
                payload_case.read_chunk_size,
            );
            let (payload, expected) = payloads.get(&key).expect("payload precomputed");
            let expected = *expected;
            group.throughput(Throughput::Bytes(support::total_bytes(payload) as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), payload_case.label),
                payload,
                |b, payload| {
                    b.to_async(&rt).iter(|| async {
                        let result =
                            run_line_case(case, payload, payload_case.read_chunk_size, options)
                                .await;
                        assert_line_result(case, expected, result);
                        black_box(result)
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
) -> LineBenchResult {
    let (reader, gate) = support::GatedChunkedReader::new(payload);
    let stream = support::broadcast_stream_reliable_replay(reader, read_chunk_size);
    run_line_consumer(&stream, LineConsumerKind::CountOnly, options, gate).await
}

async fn run_best_effort_replay_slow_subscriber_case(
    payload: &support::Payload,
    read_chunk_size: usize,
    options: LineParsingOptions,
) -> LineBenchResult {
    let (reader, gate) = support::GatedChunkedReader::new(payload);
    let stream = support::broadcast_stream_best_effort_replay(reader, read_chunk_size);
    let _slow_subscriber = stream.collect_chunks_async((), HangingChunkCollector);
    let collector = stream.collect_line_stats(options);

    open_gate_after_subscriber_is_polling(gate).await;
    let stats = collector.wait().await.unwrap();
    LineBenchResult::from_stats(&stats)
}

fn bench_line_delivery_replay(c: &mut Criterion) {
    let rt = support::runtime();
    let mut group = c.benchmark_group("line_delivery_replay");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for read_chunk_size in [
        support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
        support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
    ] {
        let payload = support::build_line_payload(
            support::SHORT_LINE_LEN,
            support::SHORT_LINE_COUNT,
            read_chunk_size,
        );
        let expected = ExpectedLineResult::from_payload(&payload);
        group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));
        group.bench_with_input(
            BenchmarkId::new(
                "count_only/broadcast/reliable_replay",
                format!("short_{read_chunk_size}B"),
            ),
            &payload,
            |b, payload| {
                b.to_async(&rt).iter(|| async {
                    let result =
                        run_reliable_replay_line_case(payload, read_chunk_size, options).await;
                    assert_line_result(
                        LineBenchCase {
                            consumer: LineConsumerKind::CountOnly,
                            backend: BackendKind::Broadcast,
                            delivery: DeliveryKind::Reliable,
                        },
                        expected,
                        result,
                    );
                    black_box(result)
                });
            },
        );
    }

    let payload = support::build_line_payload(
        support::SHORT_LINE_LEN,
        support::SHORT_LINE_COUNT,
        support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
    );
    let expected = ExpectedLineResult::from_payload(&payload);
    group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));
    group.bench_with_input(
        BenchmarkId::new(
            "count_only/broadcast/best_effort_replay_slow_subscriber",
            "short_256B",
        ),
        &payload,
        |b, payload| {
            b.to_async(&rt).iter(|| async {
                let result = run_best_effort_replay_slow_subscriber_case(
                    payload,
                    support::SHORT_LINE_SMALL_READ_CHUNK_SIZE,
                    options,
                )
                .await;
                assert_best_effort_slow_subscriber_result(expected, result);
                black_box(result)
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
