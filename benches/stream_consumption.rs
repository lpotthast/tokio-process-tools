//! Benchmarks for tokio-process-tools.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::borrow::Cow;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;
use tokio_process_tools::broadcast::BroadcastOutputStream;
use tokio_process_tools::single_subscriber::SingleSubscriberOutputStream;
use tokio_process_tools::{
    AsyncLineCollector, BackpressureControl, FromStreamOptions, LineParsingOptions, LineWriteMode,
    Next, NumBytesExt, WaitForLineResult,
};

const CHANNEL_CAPACITY: usize = 256;
const READY_PREFIX: &str = "ready:";

#[derive(Clone, Debug)]
struct ChunkedReader {
    chunks: Vec<Bytes>,
    chunk_index: usize,
    chunk_offset: usize,
}

impl ChunkedReader {
    fn new(chunks: &[Bytes]) -> Self {
        Self {
            chunks: chunks.to_vec(),
            chunk_index: 0,
            chunk_offset: 0,
        }
    }
}

impl AsyncRead for ChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        while self.chunk_index < self.chunks.len() {
            let chunk = self.chunks[self.chunk_index].clone();
            let chunk_len = chunk.len();
            let remaining = &chunk[self.chunk_offset..];

            if remaining.is_empty() {
                self.chunk_index += 1;
                self.chunk_offset = 0;
                continue;
            }

            let to_copy = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..to_copy]);
            self.chunk_offset += to_copy;

            if self.chunk_offset == chunk_len {
                self.chunk_index += 1;
                self.chunk_offset = 0;
            }

            return Poll::Ready(Ok(()));
        }

        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Default)]
struct CountingWrite {
    bytes_written: usize,
}

impl AsyncWrite for CountingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes_written += buf.len();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct CountLineBytes;

impl AsyncLineCollector<usize> for CountLineBytes {
    async fn collect<'a>(&'a mut self, line: Cow<'a, str>, byte_count: &'a mut usize) -> Next {
        *byte_count += line.len();
        Next::Continue
    }
}

#[derive(Clone, Copy)]
enum WaitCase {
    MatchAtStart,
    MatchAtEnd,
    Missing,
}

impl WaitCase {
    fn label(self) -> &'static str {
        match self {
            Self::MatchAtStart => "match_start",
            Self::MatchAtEnd => "match_end",
            Self::Missing => "missing",
        }
    }
}

fn build_chunk_payload(total_bytes: usize, read_chunk_size: usize) -> Vec<Bytes> {
    let pattern = b"tokio-process-tools:";
    let mut payload = vec![0_u8; total_bytes];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = pattern[index % pattern.len()];
    }
    split_bytes(&payload, read_chunk_size)
}

fn build_line_payload(
    line_len: usize,
    line_count: usize,
    read_chunk_size: usize,
    wait_case: WaitCase,
) -> Vec<Bytes> {
    let mut bytes = Vec::with_capacity((line_len + 1) * line_count);

    for index in 0..line_count {
        let prefix = match wait_case {
            WaitCase::MatchAtStart if index == 0 => READY_PREFIX,
            WaitCase::MatchAtEnd if index + 1 == line_count => READY_PREFIX,
            _ => "line:",
        };
        bytes.extend_from_slice(prefix.as_bytes());

        let fill_len = line_len.saturating_sub(prefix.len());
        let fill_byte = b'a' + u8::try_from(index % 26).unwrap();
        bytes.extend(std::iter::repeat_n(fill_byte, fill_len));
        bytes.push(b'\n');
    }

    split_bytes(&bytes, read_chunk_size)
}

fn split_bytes(bytes: &[u8], read_chunk_size: usize) -> Vec<Bytes> {
    bytes
        .chunks(read_chunk_size)
        .map(Bytes::copy_from_slice)
        .collect()
}

fn single_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
) -> SingleSubscriberOutputStream {
    SingleSubscriberOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        backpressure,
        FromStreamOptions {
            chunk_size: stream_chunk_size.bytes(),
            channel_capacity: CHANNEL_CAPACITY,
        },
    )
}

fn broadcast_stream(chunks: &[Bytes], stream_chunk_size: usize) -> BroadcastOutputStream {
    BroadcastOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        FromStreamOptions {
            chunk_size: stream_chunk_size.bytes(),
            channel_capacity: CHANNEL_CAPACITY,
        },
    )
}

async fn collect_single_chunks(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
) -> usize {
    single_stream(chunks, stream_chunk_size, backpressure)
        .collect_chunks_into_vec()
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_broadcast_chunks(chunks: &[Bytes], stream_chunk_size: usize) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_chunks_into_vec()
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_single_lines(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
    options: LineParsingOptions,
) -> usize {
    single_stream(chunks, stream_chunk_size, backpressure)
        .collect_lines_into_vec(options)
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_broadcast_lines(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_lines_into_vec(options)
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_single_chunks_into_write(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
) -> usize {
    single_stream(chunks, stream_chunk_size, backpressure)
        .collect_chunks_into_write(CountingWrite::default())
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_broadcast_chunks_into_write(chunks: &[Bytes], stream_chunk_size: usize) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_chunks_into_write(CountingWrite::default())
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_single_chunks_into_write_mapped(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
) -> usize {
    single_stream(chunks, stream_chunk_size, backpressure)
        .collect_chunks_into_write_mapped(CountingWrite::default(), |chunk| chunk)
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_broadcast_chunks_into_write_mapped(
    chunks: &[Bytes],
    stream_chunk_size: usize,
) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_chunks_into_write_mapped(CountingWrite::default(), |chunk| chunk)
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_single_lines_into_write(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
    mode: LineWriteMode,
) -> usize {
    single_stream(
        chunks,
        stream_chunk_size,
        BackpressureControl::BlockUntilBufferHasSpace,
    )
    .collect_lines_into_write(CountingWrite::default(), options, mode)
    .wait()
    .await
    .unwrap()
    .bytes_written
}

async fn collect_broadcast_lines_into_write(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
    mode: LineWriteMode,
) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_lines_into_write(CountingWrite::default(), options, mode)
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_single_lines_into_write_mapped(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
    mode: LineWriteMode,
) -> usize {
    single_stream(
        chunks,
        stream_chunk_size,
        BackpressureControl::BlockUntilBufferHasSpace,
    )
    .collect_lines_into_write_mapped(
        CountingWrite::default(),
        |line| line.into_owned().into_bytes(),
        options,
        mode,
    )
    .wait()
    .await
    .unwrap()
    .bytes_written
}

async fn collect_broadcast_lines_into_write_mapped(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
    mode: LineWriteMode,
) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_lines_into_write_mapped(
            CountingWrite::default(),
            |line| line.into_owned().into_bytes(),
            options,
            mode,
        )
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_single_lines_async(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> usize {
    single_stream(
        chunks,
        stream_chunk_size,
        BackpressureControl::BlockUntilBufferHasSpace,
    )
    .collect_lines_async(0_usize, CountLineBytes, options)
    .wait()
    .await
    .unwrap()
}

async fn collect_broadcast_lines_async(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_lines_async(0_usize, CountLineBytes, options)
        .wait()
        .await
        .unwrap()
}

async fn inspect_single_lines_async(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> usize {
    let stream = single_stream(
        chunks,
        stream_chunk_size,
        BackpressureControl::BlockUntilBufferHasSpace,
    );
    let byte_count = Arc::new(AtomicUsize::new(0));
    let byte_count_in_callback = Arc::clone(&byte_count);

    stream
        .inspect_lines_async(
            move |line| {
                let line_len = line.len();
                let byte_count = Arc::clone(&byte_count_in_callback);
                async move {
                    byte_count.fetch_add(line_len, Ordering::Relaxed);
                    Next::Continue
                }
            },
            options,
        )
        .wait()
        .await
        .unwrap();

    byte_count.load(Ordering::Relaxed)
}

async fn inspect_broadcast_lines_async(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> usize {
    let stream = broadcast_stream(chunks, stream_chunk_size);
    let byte_count = Arc::new(AtomicUsize::new(0));
    let byte_count_in_callback = Arc::clone(&byte_count);

    stream
        .inspect_lines_async(
            move |line| {
                let line_len = line.len();
                let byte_count = Arc::clone(&byte_count_in_callback);
                async move {
                    byte_count.fetch_add(line_len, Ordering::Relaxed);
                    Next::Continue
                }
            },
            options,
        )
        .wait()
        .await
        .unwrap();

    byte_count.load(Ordering::Relaxed)
}

async fn wait_for_line_single(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    backpressure: BackpressureControl,
    options: LineParsingOptions,
) -> WaitForLineResult {
    single_stream(chunks, stream_chunk_size, backpressure)
        .wait_for_line(|line| line.starts_with(READY_PREFIX), options)
        .await
}

async fn wait_for_line_broadcast(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> WaitForLineResult {
    broadcast_stream(chunks, stream_chunk_size)
        .wait_for_line(|line| line.starts_with(READY_PREFIX), options)
        .await
}

async fn wait_for_line_single_gap(chunks: &[Bytes], stream_chunk_size: usize) -> WaitForLineResult {
    let stream = SingleSubscriberOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        BackpressureControl::DropLatestIncomingIfBufferFull,
        FromStreamOptions {
            chunk_size: stream_chunk_size.bytes(),
            channel_capacity: 1,
        },
    );

    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    stream
        .wait_for_line(
            |line| line.starts_with(READY_PREFIX),
            LineParsingOptions::default(),
        )
        .await
}

fn bench_chunk_collection(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("chunk_collection");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for stream_chunk_size in [4 * 1024, 16 * 1024, 64 * 1024] {
        let total_bytes = 8 * 1024 * 1024;
        let chunks = build_chunk_payload(total_bytes, stream_chunk_size);
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new("single_drop_latest", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_chunks(
                            chunks,
                            stream_chunk_size,
                            BackpressureControl::DropLatestIncomingIfBufferFull,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("single_block_until_space", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_chunks(
                            chunks,
                            stream_chunk_size,
                            BackpressureControl::BlockUntilBufferHasSpace,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(collect_broadcast_chunks(chunks, stream_chunk_size).await)
                });
            },
        );
    }

    group.finish();
}

fn bench_line_collection(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("line_collection");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for (label, line_len, line_count, stream_chunk_size) in [
        ("short_lines", 64, 16_384, 4 * 1024),
        ("short_lines", 64, 16_384, 16 * 1024),
        ("long_lines", 4 * 1024, 2_048, 16 * 1024),
        ("long_lines", 4 * 1024, 2_048, 64 * 1024),
    ] {
        let chunks = build_line_payload(line_len, line_count, stream_chunk_size, WaitCase::Missing);
        let total_bytes = (line_len + 1) * line_count;
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new(format!("single_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_lines(
                            chunks,
                            stream_chunk_size,
                            BackpressureControl::BlockUntilBufferHasSpace,
                            options,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("broadcast_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(collect_broadcast_lines(chunks, stream_chunk_size, options).await)
                });
            },
        );
    }

    group.finish();
}

#[expect(
    clippy::too_many_lines,
    reason = "criterion benchmark registration is intentionally grouped by helper family"
)]
fn bench_writer_helpers(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("writer_helpers");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for stream_chunk_size in [16 * 1024, 64 * 1024] {
        let total_bytes = 8 * 1024 * 1024;
        let chunks = build_chunk_payload(total_bytes, stream_chunk_size);
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new("single_chunks", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_chunks_into_write(
                            chunks,
                            stream_chunk_size,
                            BackpressureControl::BlockUntilBufferHasSpace,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("single_chunks_mapped", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_chunks_into_write_mapped(
                            chunks,
                            stream_chunk_size,
                            BackpressureControl::BlockUntilBufferHasSpace,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast_chunks", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(collect_broadcast_chunks_into_write(chunks, stream_chunk_size).await)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast_chunks_mapped", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_chunks_into_write_mapped(chunks, stream_chunk_size).await,
                    )
                });
            },
        );
    }

    let options = LineParsingOptions::default();
    let mode = LineWriteMode::AppendLf;

    for (label, line_len, line_count, stream_chunk_size) in [
        ("short_lines", 64, 16_384, 16 * 1024),
        ("long_lines", 4 * 1024, 2_048, 64 * 1024),
    ] {
        let chunks = build_line_payload(line_len, line_count, stream_chunk_size, WaitCase::Missing);
        let total_bytes = (line_len + 1) * line_count;
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new(format!("single_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_lines_into_write(chunks, stream_chunk_size, options, mode)
                            .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("single_{label}_mapped"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_single_lines_into_write_mapped(
                            chunks,
                            stream_chunk_size,
                            options,
                            mode,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("broadcast_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_lines_into_write(
                            chunks,
                            stream_chunk_size,
                            options,
                            mode,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("broadcast_{label}_mapped"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_lines_into_write_mapped(
                            chunks,
                            stream_chunk_size,
                            options,
                            mode,
                        )
                        .await,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_async_line_processing(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("async_line_processing");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for (label, line_len, line_count, stream_chunk_size) in [
        ("short_lines", 64, 16_384, 16 * 1024),
        ("long_lines", 4 * 1024, 2_048, 16 * 1024),
    ] {
        let chunks = build_line_payload(line_len, line_count, stream_chunk_size, WaitCase::Missing);
        let total_bytes = (line_len + 1) * line_count;
        group.throughput(Throughput::Bytes(total_bytes as u64));

        group.bench_with_input(
            BenchmarkId::new(format!("collect_single_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(collect_single_lines_async(chunks, stream_chunk_size, options).await)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("collect_broadcast_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_lines_async(chunks, stream_chunk_size, options).await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("inspect_single_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(inspect_single_lines_async(chunks, stream_chunk_size, options).await)
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new(format!("inspect_broadcast_{label}"), stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        inspect_broadcast_lines_async(chunks, stream_chunk_size, options).await,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_wait_for_line(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("wait_for_line");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));
    let options = LineParsingOptions::default();

    for wait_case in [
        WaitCase::MatchAtStart,
        WaitCase::MatchAtEnd,
        WaitCase::Missing,
    ] {
        let chunks = build_line_payload(128, 8_192, 4 * 1024, wait_case);

        group.bench_with_input(
            BenchmarkId::new("single", wait_case.label()),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        wait_for_line_single(
                            chunks,
                            4 * 1024,
                            BackpressureControl::BlockUntilBufferHasSpace,
                            options,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast", wait_case.label()),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(wait_for_line_broadcast(chunks, 4 * 1024, options).await)
                });
            },
        );
    }

    let gap_chunks = build_line_payload(128, 32_768, 256, WaitCase::MatchAtEnd);
    group.bench_with_input(
        BenchmarkId::new("single_gap_drop_latest", "match_end"),
        &gap_chunks,
        |b, chunks| {
            b.to_async(&rt)
                .iter(|| async { black_box(wait_for_line_single_gap(chunks, 256).await) });
        },
    );

    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1));
    targets = bench_chunk_collection, bench_line_collection, bench_writer_helpers, bench_async_line_processing, bench_wait_for_line
);
criterion_main!(benches);
