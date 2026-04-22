//! Benchmarks for tokio-process-tools.

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::borrow::Cow;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;
use tokio_process_tools::{
    AsyncLineCollector, BestEffortDelivery, DeliveryGuarantee, LineParsingOptions, LineWriteMode,
    Next, NoReplay, NumBytesExt, ReliableDelivery, ReplayEnabled, ReplayRetention,
    SealedReplayBehavior, StreamConfig, WaitForLineResult, WriteCollectionOptions,
    broadcast::BroadcastOutputStream, single_subscriber::SingleSubscriberOutputStream,
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

#[derive(Clone, Debug, Default)]
struct StartGate {
    started: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl StartGate {
    fn open(&self) {
        self.started.store(true, Ordering::Release);
        if let Some(waker) = self.waker.lock().expect("start gate poisoned").take() {
            waker.wake();
        }
    }

    fn is_open(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    fn store_waker(&self, waker: &Waker) {
        *self.waker.lock().expect("start gate poisoned") = Some(waker.clone());
    }
}

#[derive(Clone, Debug)]
struct GatedChunkedReader {
    inner: ChunkedReader,
    gate: StartGate,
}

impl GatedChunkedReader {
    fn new(chunks: &[Bytes]) -> (Self, StartGate) {
        let gate = StartGate::default();
        (
            Self {
                inner: ChunkedReader::new(chunks),
                gate: gate.clone(),
            },
            gate,
        )
    }
}

impl AsyncRead for GatedChunkedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.gate.is_open() {
            self.gate.store_waker(cx.waker());
            // Recheck after registering the waker to avoid losing a concurrent open().
            if !self.gate.is_open() {
                return Poll::Pending;
            }
        }

        Pin::new(&mut self.inner).poll_read(cx, buf)
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
    delivery: DeliveryGuarantee,
) -> SingleSubscriberOutputStream {
    match delivery {
        DeliveryGuarantee::BestEffort => SingleSubscriberOutputStream::from_stream(
            ChunkedReader::new(chunks),
            "bench",
            StreamConfig::builder()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(stream_chunk_size.bytes())
                .max_buffered_chunks(CHANNEL_CAPACITY)
                .build(),
        ),
        DeliveryGuarantee::ReliableForActiveSubscribers => {
            SingleSubscriberOutputStream::from_stream(
                ChunkedReader::new(chunks),
                "bench",
                StreamConfig::builder()
                    .reliable_for_active_subscribers()
                    .no_replay()
                    .read_chunk_size(stream_chunk_size.bytes())
                    .max_buffered_chunks(CHANNEL_CAPACITY)
                    .build(),
            )
        }
    }
}

fn broadcast_stream(chunks: &[Bytes], stream_chunk_size: usize) -> BroadcastOutputStream {
    BroadcastOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(stream_chunk_size.bytes())
            .max_buffered_chunks(CHANNEL_CAPACITY)
            .build(),
    )
}

fn reliable_broadcast_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
) -> BroadcastOutputStream<ReliableDelivery, NoReplay> {
    BroadcastOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(stream_chunk_size.bytes())
            .max_buffered_chunks(CHANNEL_CAPACITY)
            .build(),
    )
}

fn best_effort_replay_mode(
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> StreamConfig<BestEffortDelivery, ReplayEnabled> {
    let builder = StreamConfig::builder().best_effort_delivery();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
    .read_chunk_size(stream_chunk_size.bytes())
    .max_buffered_chunks(CHANNEL_CAPACITY)
    .build()
}

fn reliable_replay_mode(
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> StreamConfig<ReliableDelivery, ReplayEnabled> {
    let builder = StreamConfig::builder().reliable_for_active_subscribers();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
    .read_chunk_size(stream_chunk_size.bytes())
    .max_buffered_chunks(CHANNEL_CAPACITY)
    .build()
}

fn best_effort_broadcast_mode_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> BroadcastOutputStream<BestEffortDelivery, ReplayEnabled> {
    BroadcastOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        best_effort_replay_mode(stream_chunk_size, replay_retention),
    )
}

fn reliable_broadcast_mode_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> BroadcastOutputStream<ReliableDelivery, ReplayEnabled> {
    BroadcastOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        reliable_replay_mode(stream_chunk_size, replay_retention),
    )
}

fn gated_broadcast_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
) -> (BroadcastOutputStream, StartGate) {
    let (reader, gate) = GatedChunkedReader::new(chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "bench",
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(stream_chunk_size.bytes())
            .max_buffered_chunks(CHANNEL_CAPACITY)
            .build(),
    );
    (stream, gate)
}

fn gated_reliable_broadcast_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
) -> (BroadcastOutputStream<ReliableDelivery, NoReplay>, StartGate) {
    let (reader, gate) = GatedChunkedReader::new(chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "bench",
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(stream_chunk_size.bytes())
            .max_buffered_chunks(CHANNEL_CAPACITY)
            .build(),
    );
    (stream, gate)
}

fn gated_best_effort_broadcast_mode_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> (
    BroadcastOutputStream<BestEffortDelivery, ReplayEnabled>,
    StartGate,
) {
    let (reader, gate) = GatedChunkedReader::new(chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "bench",
        best_effort_replay_mode(stream_chunk_size, replay_retention),
    );
    (stream, gate)
}

fn gated_reliable_broadcast_mode_stream(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    replay_retention: ReplayRetention,
) -> (
    BroadcastOutputStream<ReliableDelivery, ReplayEnabled>,
    StartGate,
) {
    let (reader, gate) = GatedChunkedReader::new(chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "bench",
        reliable_replay_mode(stream_chunk_size, replay_retention),
    );
    (stream, gate)
}

async fn collect_single_chunks(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    delivery: DeliveryGuarantee,
) -> usize {
    single_stream(chunks, stream_chunk_size, delivery)
        .collect_all_chunks_into_vec_trusted()
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_broadcast_chunks(chunks: &[Bytes], stream_chunk_size: usize) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_all_chunks_into_vec_trusted()
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_broadcast_mode_chunks(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    replay_retention: Option<ReplayRetention>,
    delivery: DeliveryGuarantee,
) -> usize {
    match (delivery, replay_retention) {
        (DeliveryGuarantee::BestEffort, Some(replay_retention)) => {
            best_effort_broadcast_mode_stream(chunks, stream_chunk_size, replay_retention)
                .collect_all_chunks_into_vec_trusted()
                .wait()
                .await
                .unwrap()
                .len()
        }
        (DeliveryGuarantee::BestEffort, None) => broadcast_stream(chunks, stream_chunk_size)
            .collect_all_chunks_into_vec_trusted()
            .wait()
            .await
            .unwrap()
            .len(),
        (DeliveryGuarantee::ReliableForActiveSubscribers, Some(replay_retention)) => {
            reliable_broadcast_mode_stream(chunks, stream_chunk_size, replay_retention)
                .collect_all_chunks_into_vec_trusted()
                .wait()
                .await
                .unwrap()
                .len()
        }
        (DeliveryGuarantee::ReliableForActiveSubscribers, None) => {
            reliable_broadcast_stream(chunks, stream_chunk_size)
                .collect_all_chunks_into_vec_trusted()
                .wait()
                .await
                .unwrap()
                .len()
        }
    }
}

async fn collect_broadcast_chunks_for_subscribers(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    subscriber_count: usize,
) -> usize {
    let (stream, gate) = gated_broadcast_stream(chunks, stream_chunk_size);
    let collectors = (0..subscriber_count)
        .map(|_| {
            stream.collect_chunks_into_write(
                CountingWrite::default(),
                WriteCollectionOptions::fail_fast(),
            )
        })
        .collect::<Vec<_>>();

    gate.open();

    let mut bytes_written = 0;
    for collector in collectors {
        bytes_written += collector.wait().await.unwrap().bytes_written;
    }

    bytes_written
}

async fn collect_broadcast_mode_chunks_for_subscribers(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    subscriber_count: usize,
    replay_retention: Option<ReplayRetention>,
    delivery: DeliveryGuarantee,
) -> usize {
    let expected_bytes = chunks.iter().map(Bytes::len).sum::<usize>() * subscriber_count;
    let bytes_written = match (delivery, replay_retention) {
        (DeliveryGuarantee::BestEffort, Some(replay_retention)) => {
            let (stream, gate) = gated_best_effort_broadcast_mode_stream(
                chunks,
                stream_chunk_size,
                replay_retention,
            );
            let collectors = (0..subscriber_count)
                .map(|_| {
                    stream.collect_chunks_into_write(
                        CountingWrite::default(),
                        WriteCollectionOptions::fail_fast(),
                    )
                })
                .collect::<Vec<_>>();

            gate.open();

            let mut bytes_written = 0;
            for collector in collectors {
                bytes_written += collector.wait().await.unwrap().bytes_written;
            }
            bytes_written
        }
        (DeliveryGuarantee::BestEffort, None) => {
            let (stream, gate) = gated_broadcast_stream(chunks, stream_chunk_size);
            let collectors = (0..subscriber_count)
                .map(|_| {
                    stream.collect_chunks_into_write(
                        CountingWrite::default(),
                        WriteCollectionOptions::fail_fast(),
                    )
                })
                .collect::<Vec<_>>();

            gate.open();

            let mut bytes_written = 0;
            for collector in collectors {
                bytes_written += collector.wait().await.unwrap().bytes_written;
            }
            bytes_written
        }
        (DeliveryGuarantee::ReliableForActiveSubscribers, Some(replay_retention)) => {
            let (stream, gate) =
                gated_reliable_broadcast_mode_stream(chunks, stream_chunk_size, replay_retention);
            let collectors = (0..subscriber_count)
                .map(|_| {
                    stream.collect_chunks_into_write(
                        CountingWrite::default(),
                        WriteCollectionOptions::fail_fast(),
                    )
                })
                .collect::<Vec<_>>();

            gate.open();

            let mut bytes_written = 0;
            for collector in collectors {
                bytes_written += collector.wait().await.unwrap().bytes_written;
            }
            bytes_written
        }
        (DeliveryGuarantee::ReliableForActiveSubscribers, None) => {
            let (stream, gate) = gated_reliable_broadcast_stream(chunks, stream_chunk_size);
            let collectors = (0..subscriber_count)
                .map(|_| {
                    stream.collect_chunks_into_write(
                        CountingWrite::default(),
                        WriteCollectionOptions::fail_fast(),
                    )
                })
                .collect::<Vec<_>>();

            gate.open();

            let mut bytes_written = 0;
            for collector in collectors {
                bytes_written += collector.wait().await.unwrap().bytes_written;
            }
            bytes_written
        }
    };

    if matches!(delivery, DeliveryGuarantee::ReliableForActiveSubscribers) {
        assert_eq!(bytes_written, expected_bytes);
    }

    bytes_written
}

async fn collect_single_lines(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    delivery: DeliveryGuarantee,
    options: LineParsingOptions,
) -> usize {
    single_stream(chunks, stream_chunk_size, delivery)
        .collect_all_lines_into_vec_trusted(options)
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
        .collect_all_lines_into_vec_trusted(options)
        .wait()
        .await
        .unwrap()
        .len()
}

async fn collect_single_chunks_into_write(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    delivery: DeliveryGuarantee,
) -> usize {
    single_stream(chunks, stream_chunk_size, delivery)
        .collect_chunks_into_write(
            CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_broadcast_chunks_into_write(chunks: &[Bytes], stream_chunk_size: usize) -> usize {
    broadcast_stream(chunks, stream_chunk_size)
        .collect_chunks_into_write(
            CountingWrite::default(),
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await
        .unwrap()
        .bytes_written
}

async fn collect_single_chunks_into_write_mapped(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    delivery: DeliveryGuarantee,
) -> usize {
    single_stream(chunks, stream_chunk_size, delivery)
        .collect_chunks_into_write_mapped(
            CountingWrite::default(),
            |chunk| chunk,
            WriteCollectionOptions::fail_fast(),
        )
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
        .collect_chunks_into_write_mapped(
            CountingWrite::default(),
            |chunk| chunk,
            WriteCollectionOptions::fail_fast(),
        )
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
        DeliveryGuarantee::ReliableForActiveSubscribers,
    )
    .collect_lines_into_write(
        CountingWrite::default(),
        options,
        mode,
        WriteCollectionOptions::fail_fast(),
    )
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
        .collect_lines_into_write(
            CountingWrite::default(),
            options,
            mode,
            WriteCollectionOptions::fail_fast(),
        )
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
        DeliveryGuarantee::ReliableForActiveSubscribers,
    )
    .collect_lines_into_write_mapped(
        CountingWrite::default(),
        |line| line.into_owned().into_bytes(),
        options,
        mode,
        WriteCollectionOptions::fail_fast(),
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
            WriteCollectionOptions::fail_fast(),
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
        DeliveryGuarantee::ReliableForActiveSubscribers,
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
        DeliveryGuarantee::ReliableForActiveSubscribers,
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
    delivery: DeliveryGuarantee,
    options: LineParsingOptions,
) -> WaitForLineResult {
    single_stream(chunks, stream_chunk_size, delivery)
        .wait_for_line(|line| line.starts_with(READY_PREFIX), options)
        .await
        .expect("benchmark stream should not fail")
}

async fn wait_for_line_broadcast(
    chunks: &[Bytes],
    stream_chunk_size: usize,
    options: LineParsingOptions,
) -> WaitForLineResult {
    broadcast_stream(chunks, stream_chunk_size)
        .wait_for_line(|line| line.starts_with(READY_PREFIX), options)
        .await
        .expect("benchmark stream should not fail")
}

async fn wait_for_line_single_gap(chunks: &[Bytes], stream_chunk_size: usize) -> WaitForLineResult {
    let stream = SingleSubscriberOutputStream::from_stream(
        ChunkedReader::new(chunks),
        "bench",
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(stream_chunk_size.bytes())
            .max_buffered_chunks(1)
            .build(),
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
        .expect("benchmark stream should not fail")
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
                            DeliveryGuarantee::BestEffort,
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
                            DeliveryGuarantee::ReliableForActiveSubscribers,
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

        group.bench_with_input(
            BenchmarkId::new("broadcast_best_effort", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_mode_chunks(
                            chunks,
                            stream_chunk_size,
                            None,
                            DeliveryGuarantee::BestEffort,
                        )
                        .await,
                    )
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("broadcast_reliable_active", stream_chunk_size),
            &chunks,
            |b, chunks| {
                b.to_async(&rt).iter(|| async {
                    black_box(
                        collect_broadcast_mode_chunks(
                            chunks,
                            stream_chunk_size,
                            None,
                            DeliveryGuarantee::ReliableForActiveSubscribers,
                        )
                        .await,
                    )
                });
            },
        );
    }

    group.finish();
}

fn bench_broadcast_mode_subscriber_throughput(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let mut group = c.benchmark_group("broadcast_mode_subscriber_throughput");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    for stream_chunk_size in [16 * 1024, 64 * 1024] {
        let total_bytes = 512 * 1024;
        let chunks = build_chunk_payload(total_bytes, stream_chunk_size);

        for subscriber_count in [1, 2, 4] {
            group.throughput(Throughput::Bytes((total_bytes * subscriber_count) as u64));
            let parameter = format!("{stream_chunk_size}_bytes/{subscriber_count}_subscribers");

            group.bench_with_input(
                BenchmarkId::new("broadcast_lossy", &parameter),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(
                            collect_broadcast_chunks_for_subscribers(
                                chunks,
                                stream_chunk_size,
                                subscriber_count,
                            )
                            .await,
                        )
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new("broadcast_best_effort", &parameter),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(
                            collect_broadcast_mode_chunks_for_subscribers(
                                chunks,
                                stream_chunk_size,
                                subscriber_count,
                                None,
                                DeliveryGuarantee::BestEffort,
                            )
                            .await,
                        )
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new("broadcast_reliable_active", &parameter),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(
                            collect_broadcast_mode_chunks_for_subscribers(
                                chunks,
                                stream_chunk_size,
                                subscriber_count,
                                None,
                                DeliveryGuarantee::ReliableForActiveSubscribers,
                            )
                            .await,
                        )
                    });
                },
            );

            group.bench_with_input(
                BenchmarkId::new("broadcast_reliable_replay_all", &parameter),
                &chunks,
                |b, chunks| {
                    b.to_async(&rt).iter(|| async {
                        black_box(
                            collect_broadcast_mode_chunks_for_subscribers(
                                chunks,
                                stream_chunk_size,
                                subscriber_count,
                                Some(ReplayRetention::All),
                                DeliveryGuarantee::ReliableForActiveSubscribers,
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
                            DeliveryGuarantee::ReliableForActiveSubscribers,
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
                            DeliveryGuarantee::ReliableForActiveSubscribers,
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
                            DeliveryGuarantee::ReliableForActiveSubscribers,
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
                            DeliveryGuarantee::ReliableForActiveSubscribers,
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
    targets = bench_chunk_collection, bench_broadcast_mode_subscriber_throughput, bench_line_collection, bench_writer_helpers, bench_async_line_processing, bench_wait_for_line
);
criterion_main!(benches);
