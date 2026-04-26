use super::{Backend, BroadcastOutputStream};
use crate::{
    BestEffortDelivery, CollectionOverflowBehavior, LineCollectionOptions, LineParsingOptions,
    Next, NumBytes, NumBytesExt, ReliableDelivery, ReplayEnabled, ReplayRetention, StreamConfig,
    WaitForLineResult, WriteCollectionOptions,
};
use assertr::prelude::*;
use bytes::Bytes;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::time::sleep;

fn line_collection_options() -> LineCollectionOptions {
    LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
    }
}

fn default_stream_sizing() -> (NumBytes, usize) {
    (8.bytes(), 2)
}

fn best_effort_no_replay_options() -> StreamConfig<BestEffortDelivery, crate::NoReplay> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    best_effort_no_replay_options_with(read_chunk_size, max_buffered_chunks)
}

fn best_effort_no_replay_options_with(
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

fn reliable_no_replay_options() -> StreamConfig<ReliableDelivery, crate::NoReplay> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    reliable_no_replay_options_with(read_chunk_size, max_buffered_chunks)
}

fn reliable_no_replay_options_with(
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

fn reliable_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<ReliableDelivery, ReplayEnabled> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    reliable_options_with(replay_retention, read_chunk_size, max_buffered_chunks)
}

fn reliable_options_with(
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

fn best_effort_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<BestEffortDelivery, ReplayEnabled> {
    let (read_chunk_size, max_buffered_chunks) = default_stream_sizing();
    best_effort_options_with(replay_retention, read_chunk_size, max_buffered_chunks)
}

fn best_effort_options_with(
    replay_retention: ReplayRetention,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig<BestEffortDelivery, ReplayEnabled> {
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

#[tokio::test]
async fn default_broadcast_uses_tokio_broadcast_fast_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_no_replay_options_with(
            crate::DEFAULT_READ_CHUNK_SIZE,
            crate::DEFAULT_MAX_BUFFERED_CHUNKS,
        ),
    );

    assert_that!(matches!(&stream.backend, Backend::Fast(_))).is_true();
}

#[tokio::test]
async fn typed_best_effort_no_replay_uses_tokio_broadcast_fast_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_no_replay_options(),
    );

    assert_that!(matches!(&stream.backend, Backend::Fast(_))).is_true();
}

#[tokio::test]
async fn reliable_no_replay_uses_shared_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        reliable_no_replay_options(),
    );

    assert_that!(matches!(&stream.backend, Backend::Shared(_))).is_true();
}

#[tokio::test]
async fn replay_enabled_best_effort_uses_shared_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_options(ReplayRetention::LastChunks(1)),
    );

    assert_that!(matches!(&stream.backend, Backend::Shared(_))).is_true();
    stream.seal_replay();
    assert_that!(stream.is_replay_sealed()).is_true();
}

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

fn build_chunk_payload(total_bytes: usize, read_chunk_size: usize) -> Vec<Bytes> {
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

#[tokio::test]
async fn late_subscriber_receives_startup_line_with_replay_all() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let result = stream
        .wait_for_line_with_timeout(
            |line| line == "ready",
            LineParsingOptions::default(),
            Duration::from_secs(1),
        )
        .await;

    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn seal_replay_drops_old_history_for_future_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "old",
        LineParsingOptions::default(),
        Duration::from_millis(50),
    );

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Timeout));
}

#[tokio::test]
async fn subscriber_created_after_seal_starts_live_by_default() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "live",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    write_half.write_all(b"live\n").await.unwrap();
    write_half.flush().await.unwrap();

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn explicit_replay_after_seal_ignores_history_retained_for_active_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );
    let _active = stream.subscribe();

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "old",
        LineParsingOptions::default(),
        Duration::from_millis(50),
    );

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Timeout));
}

#[tokio::test]
async fn block_until_subscribers_catch_up_preserves_all_output_for_active_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_no_replay_options_with(2.bytes(), 1),
    );
    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

    write_half.write_all(b"a\nb\nc\n").await.unwrap();
    drop(write_half);

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_until_subscribers_catch_up_gated_multi_subscriber_collection_completes() {
    let total_bytes = 512 * 1024;
    let stream_chunk_size = 16 * 1024;
    let subscriber_count = 2;
    let chunks = build_chunk_payload(total_bytes, stream_chunk_size);
    let (reader, gate) = GatedChunkedReader::new(&chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "custom",
        reliable_no_replay_options_with(stream_chunk_size.bytes(), 256),
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

    let result = tokio::time::timeout(Duration::from_secs(1), async {
        let mut bytes_written = 0;
        for collector in collectors {
            bytes_written += collector.wait().await.unwrap().bytes_written;
        }
        bytes_written
    })
    .await;

    assert_that!(result)
        .is_ok()
        .is_equal_to(total_bytes * subscriber_count);
}

#[tokio::test]
async fn active_subscribers_still_receive_unread_tail_data_after_seal() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );
    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

    write_half.write_all(b"tail\n").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["tail"]);
}

#[tokio::test]
async fn dropping_fast_stream_closes_waiting_subscribers() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_no_replay_options());

    let waiter = stream.wait_for_line(|line| line == "never", LineParsingOptions::default());
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn dropping_shared_stream_closes_waiting_subscribers() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    let waiter = stream.wait_for_line(|line| line == "never", LineParsingOptions::default());
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn waiter_created_before_seal_can_match_replayed_startup_line() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::LastBytes(1.megabytes())),
    );
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let logs_in_task = Arc::clone(&logs);
    let _logger = stream.inspect_lines(
        move |line| {
            logs_in_task.lock().unwrap().push(line.into_owned());
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let ready = stream.wait_for_line_with_timeout(
        |line| line == "ready",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    stream.seal_replay();

    assert_that!(ready.await).is_equal_to(Ok(WaitForLineResult::Matched));
    assert_that!(logs.lock().unwrap().clone()).contains_exactly(["ready"]);
}
