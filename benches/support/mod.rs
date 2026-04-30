#![allow(dead_code)]

// `ChunkedReader`, `GatedChunkedReader`, `StartGate`, `CountingWrite`, and
// `build_chunk_payload` are intentionally duplicated in
// `src/output_stream/backend/broadcast/tests/common.rs`. The bench versions use
// `Arc<[Bytes]>` (the `Payload` alias) for cheap clones across criterion
// iterations and need `pub` visibility, while the test versions use
// `Vec<Bytes>` and `pub(super)`. Sharing them would require a `dev-utils`
// workspace crate or a `test-support` feature on the main crate; neither is
// justified by the size of the duplication. Keep the two copies in sync.

use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::runtime::Runtime;
use tokio_process_tools::{
    BestEffortDelivery, BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, NoReplay, NumBytesExt,
    ReliableDelivery, ReplayEnabled, SingleSubscriberOutputStream, StreamConfig,
};

pub const BENCH_STREAM_NAME: &str = "bench";
pub const THROUGHPUT_MAX_BUFFERED_CHUNKS: usize = 8 * DEFAULT_MAX_BUFFERED_CHUNKS;

pub const CHUNK_TOTAL_BYTES: usize = 8 * 1024 * 1024;
pub const COUNT_ONLY_RELIABLE_CHUNK_READ_CHUNK_SIZES: [usize; 3] = [256, 16 * 1024, 64 * 1024];
pub const COUNT_ONLY_BEST_EFFORT_CHUNK_READ_CHUNK_SIZES: [usize; 2] = [16 * 1024, 64 * 1024];
pub const COLLECT_CHUNK_READ_CHUNK_SIZES: [usize; 2] = [16 * 1024, 64 * 1024];

pub const SHORT_LINE_LEN: usize = 64;
pub const SHORT_LINE_COUNT: usize = 16_384;
pub const SHORT_LINE_SMALL_READ_CHUNK_SIZE: usize = 256;
pub const SHORT_LINE_LARGE_READ_CHUNK_SIZE: usize = 16 * 1024;

pub const LONG_LINE_LEN: usize = 4 * 1024;
pub const LONG_LINE_COUNT: usize = 2_048;
pub const LONG_LINE_SMALL_READ_CHUNK_SIZE: usize = 16 * 1024;
pub const LONG_LINE_LARGE_READ_CHUNK_SIZE: usize = 64 * 1024;

pub type Payload = Arc<[Bytes]>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Single,
    Broadcast,
}

impl BackendKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Broadcast => "broadcast",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    BestEffort,
    Reliable,
}

impl DeliveryKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BestEffort => "best_effort",
            Self::Reliable => "reliable",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChunkStats {
    pub bytes: usize,
    pub chunks: usize,
    pub checksum: u64,
}

#[derive(Debug, Default)]
pub struct CountingWrite {
    pub bytes: usize,
}

impl AsyncWrite for CountingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes += buf.len();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl ChunkStats {
    pub fn observe(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len();
        self.chunks += 1;
        self.checksum = sampled_checksum(self.checksum, bytes);
    }
}

#[derive(Clone, Debug, Default)]
pub struct LineStats {
    pub bytes: usize,
    pub lines: usize,
    pub checksum: u64,
}

impl LineStats {
    pub fn observe(&mut self, line: &str) {
        self.bytes += line.len();
        self.lines += 1;
        self.checksum = sampled_checksum(self.checksum, line.as_bytes());
    }
}

fn sampled_checksum(current: u64, bytes: &[u8]) -> u64 {
    current.rotate_left(5)
        ^ bytes.len() as u64
        ^ bytes.first().copied().map(u64::from).unwrap_or_default()
        ^ bytes.last().copied().map(u64::from).unwrap_or_default()
}

#[derive(Clone, Debug)]
pub struct ChunkedReader {
    chunks: Payload,
    chunk_index: usize,
    chunk_offset: usize,
}

impl ChunkedReader {
    #[must_use]
    pub fn new(chunks: &Payload) -> Self {
        Self {
            chunks: Arc::clone(chunks),
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
pub struct StartGate {
    started: Arc<AtomicBool>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
}

impl StartGate {
    pub fn open(&self) {
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
pub struct GatedChunkedReader {
    inner: ChunkedReader,
    gate: StartGate,
}

impl GatedChunkedReader {
    #[must_use]
    pub fn new(chunks: &Payload) -> (Self, StartGate) {
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
            if !self.gate.is_open() {
                return Poll::Pending;
            }
        }

        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[must_use]
pub fn runtime() -> Runtime {
    Runtime::new().expect("benchmark runtime should initialize")
}

#[must_use]
pub fn build_chunk_payload(total_bytes: usize, read_chunk_size: usize) -> Payload {
    let pattern = b"tokio-process-tools:";
    let mut payload = vec![0_u8; total_bytes];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = pattern[index % pattern.len()];
    }
    split_bytes(&payload, read_chunk_size)
}

#[must_use]
pub fn build_line_payload(line_len: usize, line_count: usize, read_chunk_size: usize) -> Payload {
    let pattern = b"tokio-process-tools-line:";
    let mut payload = Vec::with_capacity((line_len + 1) * line_count);

    for line_index in 0..line_count {
        for byte_index in 0..line_len {
            let pattern_index = (line_index + byte_index) % pattern.len();
            payload.push(pattern[pattern_index]);
        }
        payload.push(b'\n');
    }

    split_bytes(&payload, read_chunk_size)
}

#[must_use]
pub fn total_bytes(chunks: &Payload) -> usize {
    chunks.iter().map(Bytes::len).sum()
}

#[must_use]
pub fn chunk_stats(chunks: &Payload) -> ChunkStats {
    let mut stats = ChunkStats::default();
    for chunk in chunks.iter() {
        stats.observe(chunk);
    }
    stats
}

#[must_use]
pub fn line_stats(chunks: &Payload) -> LineStats {
    let bytes = chunks.iter().map(Bytes::len).sum();
    let mut buffer = Vec::with_capacity(bytes);
    for chunk in chunks.iter() {
        buffer.extend_from_slice(chunk);
    }

    let text = std::str::from_utf8(&buffer).expect("benchmark line payload should be UTF-8");
    let mut stats = LineStats::default();
    for line in text.lines() {
        stats.observe(line);
    }
    stats
}

#[must_use]
pub fn single_stream_best_effort<R>(
    reader: R,
    read_chunk_size: usize,
) -> SingleSubscriberOutputStream<BestEffortDelivery, NoReplay>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    SingleSubscriberOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn single_stream_reliable<R>(
    reader: R,
    read_chunk_size: usize,
) -> SingleSubscriberOutputStream<ReliableDelivery, NoReplay>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    SingleSubscriberOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn broadcast_stream_best_effort<R>(
    reader: R,
    read_chunk_size: usize,
) -> BroadcastOutputStream<BestEffortDelivery, NoReplay>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    BroadcastOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn broadcast_stream_reliable<R>(
    reader: R,
    read_chunk_size: usize,
) -> BroadcastOutputStream<ReliableDelivery, NoReplay>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    BroadcastOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn broadcast_stream_best_effort_replay<R>(
    reader: R,
    read_chunk_size: usize,
) -> BroadcastOutputStream<BestEffortDelivery, ReplayEnabled>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    BroadcastOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .best_effort_delivery()
            .replay_all()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn broadcast_stream_reliable_replay<R>(
    reader: R,
    read_chunk_size: usize,
) -> BroadcastOutputStream<ReliableDelivery, ReplayEnabled>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    BroadcastOutputStream::from_stream(
        reader,
        BENCH_STREAM_NAME,
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .replay_all()
            .read_chunk_size(read_chunk_size.bytes())
            .max_buffered_chunks(THROUGHPUT_MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

fn split_bytes(bytes: &[u8], read_chunk_size: usize) -> Payload {
    bytes
        .chunks(read_chunk_size)
        .map(Bytes::copy_from_slice)
        .collect::<Vec<_>>()
        .into()
}
