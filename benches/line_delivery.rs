//! Benchmarks for line delivery throughput.

mod support;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use std::time::Duration;
use support::{BackendKind, DeliveryKind};
use tokio_process_tools::{
    AsyncChunkCollector, BestEffortDelivery, BroadcastOutputStream, Chunk, CollectedLines,
    Consumer, LineCollectionOptions, LineParsingOptions, Next, NoReplay, ReliableDelivery,
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

#[derive(Debug, Clone, Copy)]
struct LineBenchResult {
    bytes: usize,
    lines: usize,
    checksum: Option<u64>,
}

impl LineBenchResult {
    fn from_stats(stats: &support::LineStats) -> Self {
        Self {
            bytes: stats.bytes,
            lines: stats.lines,
            checksum: Some(stats.checksum),
        }
    }

    fn from_lines(lines: &CollectedLines) -> Self {
        Self {
            bytes: lines.iter().map(String::len).sum(),
            lines: lines.len(),
            checksum: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ExpectedLineResult {
    bytes: usize,
    lines: usize,
    checksum: u64,
}

impl ExpectedLineResult {
    fn from_payload(payload: &support::Payload) -> Self {
        let stats = support::line_stats(payload);
        Self {
            bytes: stats.bytes,
            lines: stats.lines,
            checksum: stats.checksum,
        }
    }
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

const LINE_COUNT_ONLY_BEST_EFFORT_PAYLOADS: [LinePayloadCase; 2] = [
    LinePayloadCase {
        label: "short_ascii_16KiB",
        payload_kind: LinePayloadKind::Ascii,
        line_len: support::SHORT_LINE_LEN,
        line_count: support::SHORT_LINE_COUNT,
        read_chunk_size: support::SHORT_LINE_LARGE_READ_CHUNK_SIZE,
        purpose: "keeps a larger-chunk parser baseline with minimal retained allocation",
    },
    LinePayloadCase {
        label: "short_utf8_16KiB",
        payload_kind: LinePayloadKind::Utf8,
        line_len: support::UTF8_SHORT_LINE_LEN,
        line_count: support::UTF8_SHORT_LINE_COUNT,
        read_chunk_size: support::UTF8_SHORT_LINE_LARGE_READ_CHUNK_SIZE,
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

impl LineBenchStream for BroadcastOutputStream<BestEffortDelivery, NoReplay> {
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

impl LineBenchStream for BroadcastOutputStream<ReliableDelivery, NoReplay> {
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

impl LineBenchStream for BroadcastOutputStream<BestEffortDelivery, ReplayEnabled> {
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

impl LineBenchStream for BroadcastOutputStream<ReliableDelivery, ReplayEnabled> {
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

    black_box((expected.checksum, result.checksum));
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

    black_box((
        expected.bytes,
        expected.lines,
        expected.checksum,
        result.bytes,
        result.lines,
        result.checksum,
    ));
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

    for case in LINE_BENCH_CASES {
        for payload_case in payloads_for(case) {
            let payload = build_payload(*payload_case);
            let expected = ExpectedLineResult::from_payload(&payload);
            group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));

            group.bench_with_input(
                BenchmarkId::new(case_id(case), payload_case.label),
                &payload,
                |b, payload| {
                    b.to_async(&rt).iter(|| async {
                        black_box(case.purpose);
                        black_box(payload_case.purpose);
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
        support::UTF8_SHORT_LINE_SMALL_READ_CHUNK_SIZE,
        support::UTF8_SHORT_LINE_LARGE_READ_CHUNK_SIZE,
    ] {
        let payload = support::build_utf8_line_payload(
            support::UTF8_SHORT_LINE_LEN,
            support::UTF8_SHORT_LINE_COUNT,
            read_chunk_size,
        );
        let expected = ExpectedLineResult::from_payload(&payload);
        group.throughput(Throughput::Bytes(support::total_bytes(&payload) as u64));
        group.bench_with_input(
            BenchmarkId::new(
                "count_only/broadcast/reliable_replay",
                format!("short_utf8_{read_chunk_size}B"),
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
                            purpose: "checks reliable replay count-only line delivery",
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
            "short_ascii_256B",
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
