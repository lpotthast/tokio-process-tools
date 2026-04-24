#![allow(dead_code)]

use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::runtime::Runtime;
use tokio_process_tools::{
    NoReplay, NumBytesExt, ReliableDelivery, StreamConfig, broadcast::BroadcastOutputStream,
    single_subscriber::SingleSubscriberOutputStream,
};

pub const BENCH_STREAM_NAME: &str = "bench";
pub const MAX_BUFFERED_CHUNKS: usize = 256;

pub const CHUNK_TOTAL_BYTES: usize = 8 * 1024 * 1024;
pub const CHUNK_READ_CHUNK_SIZES: [usize; 2] = [16 * 1024, 64 * 1024];

pub const SHORT_LINE_LEN: usize = 64;
pub const SHORT_LINE_COUNT: usize = 16_384;
pub const SHORT_LINE_READ_CHUNK_SIZE: usize = 16 * 1024;

pub const LONG_LINE_LEN: usize = 4 * 1024;
pub const LONG_LINE_COUNT: usize = 2_048;
pub const LONG_LINE_READ_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ChunkedReader {
    chunks: Vec<Bytes>,
    chunk_index: usize,
    chunk_offset: usize,
}

impl ChunkedReader {
    #[must_use]
    pub fn new(chunks: &[Bytes]) -> Self {
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

#[must_use]
pub fn runtime() -> Runtime {
    Runtime::new().expect("benchmark runtime should initialize")
}

#[must_use]
pub fn build_chunk_payload(total_bytes: usize, read_chunk_size: usize) -> Vec<Bytes> {
    let pattern = b"tokio-process-tools:";
    let mut payload = vec![0_u8; total_bytes];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = pattern[index % pattern.len()];
    }
    split_bytes(&payload, read_chunk_size)
}

#[must_use]
pub fn build_line_payload(
    line_len: usize,
    line_count: usize,
    read_chunk_size: usize,
) -> Vec<Bytes> {
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
pub fn total_bytes(chunks: &[Bytes]) -> usize {
    chunks.iter().map(Bytes::len).sum()
}

#[must_use]
pub fn single_stream<R>(reader: R, read_chunk_size: usize) -> SingleSubscriberOutputStream
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
            .max_buffered_chunks(MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

#[must_use]
pub fn broadcast_stream<R>(
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
            .max_buffered_chunks(MAX_BUFFERED_CHUNKS)
            .build(),
    )
}

fn split_bytes(bytes: &[u8], read_chunk_size: usize) -> Vec<Bytes> {
    bytes
        .chunks(read_chunk_size)
        .map(Bytes::copy_from_slice)
        .collect()
}
