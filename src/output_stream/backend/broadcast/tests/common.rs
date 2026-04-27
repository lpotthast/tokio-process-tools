use crate::{
    BestEffortDelivery, CollectionOverflowBehavior, LineCollectionOptions, NumBytes, NumBytesExt,
    ReliableDelivery, ReplayEnabled, ReplayRetention, StreamConfig,
};
use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) fn line_collection_options() -> LineCollectionOptions {
    LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    }
}

pub(super) fn default_stream_sizing() -> (NumBytes, usize) {
    (8.bytes(), 2)
}

pub(super) fn best_effort_no_replay_options() -> StreamConfig<BestEffortDelivery, crate::NoReplay> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    best_effort_no_replay_options_with(read_chunk_size, max_buffered_chunks)
}

pub(super) fn best_effort_no_replay_options_with(
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig<BestEffortDelivery, crate::NoReplay> {
    StreamConfig::builder()
        .best_effort_delivery()
        .no_replay()
        .read_chunk_size(read_chunk_size)
        .max_buffered_chunks(max_buffered_chunks)
        .build()
}

pub(super) fn reliable_no_replay_options() -> StreamConfig<ReliableDelivery, crate::NoReplay> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    reliable_no_replay_options_with(read_chunk_size, max_buffered_chunks)
}

pub(super) fn reliable_no_replay_options_with(
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig<ReliableDelivery, crate::NoReplay> {
    StreamConfig::builder()
        .reliable_for_active_subscribers()
        .no_replay()
        .read_chunk_size(read_chunk_size)
        .max_buffered_chunks(max_buffered_chunks)
        .build()
}

pub(super) fn reliable_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<ReliableDelivery, ReplayEnabled> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    reliable_options_with(replay_retention, read_chunk_size, max_buffered_chunks)
}

pub(super) fn reliable_options_with(
    replay_retention: ReplayRetention,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig<ReliableDelivery, ReplayEnabled> {
    let builder = StreamConfig::builder().reliable_for_active_subscribers();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .read_chunk_size(read_chunk_size)
    .max_buffered_chunks(max_buffered_chunks)
    .build()
}

pub(super) fn best_effort_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<BestEffortDelivery, ReplayEnabled> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    let builder = StreamConfig::builder().best_effort_delivery();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .read_chunk_size(read_chunk_size)
    .max_buffered_chunks(max_buffered_chunks)
    .build()
}

#[derive(Clone, Debug)]
pub(super) struct ChunkedReader {
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
pub(super) struct StartGate {
    started: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl StartGate {
    pub(super) fn open(&self) {
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
pub(super) struct GatedChunkedReader {
    inner: ChunkedReader,
    gate: StartGate,
}

impl GatedChunkedReader {
    pub(super) fn new(chunks: &[Bytes]) -> (Self, StartGate) {
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

#[derive(Debug, Default)]
pub(super) struct CountingWrite {
    pub(super) bytes_written: usize,
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

pub(super) fn build_chunk_payload(total_bytes: usize, read_chunk_size: usize) -> Vec<Bytes> {
    let pattern = b"tokio-process-tools:";
    let mut payload = vec![0_u8; total_bytes];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = pattern[index % pattern.len()];
    }
    payload
        .chunks(read_chunk_size)
        .map(Bytes::copy_from_slice)
        .collect()
}
