use super::{Backend, BroadcastOutputStream, SharedBackend};
use crate::output_stream::StreamEvent;
use crate::output_stream::backend::test_support::{FailingWrite, ReadErrorAfterBytes};
use crate::{
    BestEffortDelivery, CollectionOverflowBehavior, CollectorError, Delivery, InspectorError,
    LineCollectionOptions, LineParsingOptions, Next, NumBytes, NumBytesExt, RawCollectionOptions,
    ReliableDelivery, Replay, ReplayEnabled, ReplayRetention, SinkWriteErrorAction, StreamConfig,
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
    LineCollectionOptions::builder()
        .max_bytes(1.megabytes())
        .max_lines(1024)
        .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
        .build()
}

fn raw_collection_options() -> RawCollectionOptions {
    RawCollectionOptions::new(1.megabytes())
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

fn shared_backend<D, R>(stream: &BroadcastOutputStream<D, R>) -> &SharedBackend<D, R>
where
    D: Delivery,
    R: Replay,
{
    let backend = match &stream.backend {
        Backend::Shared(backend) => Some(backend),
        Backend::Fast(_) => None,
    };
    assert_that!(backend.is_some()).is_true();
    backend.expect("broadcast backend assertion should have failed")
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
#[should_panic(expected = "max_buffered_chunks must be greater than zero")]
async fn from_stream_panics_on_zero_max_buffered_chunks() {
    let _stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_no_replay_options_with(8.bytes(), 0),
    );
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
async fn wait_for_line_returns_read_error_when_stream_read_fails() {
    let stream = BroadcastOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"booting\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        reliable_no_replay_options(),
    );

    let result = stream
        .wait_for_line(|line| line == "partial", LineParsingOptions::default())
        .await;

    let err = result.expect_err("read failure should be surfaced");
    assert_that!(err.stream_name()).is_equal_to("custom");
    assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn late_wait_for_line_returns_read_error_after_stream_read_fails() {
    let stream = BroadcastOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"booting\n", io::ErrorKind::BrokenPipe),
        "custom",
        reliable_no_replay_options(),
    );

    while shared_backend(&stream)
        .shared
        .state
        .lock()
        .expect("broadcast state poisoned")
        .read_error
        .is_none()
    {
        tokio::task::yield_now().await;
    }

    let result = stream
        .wait_for_line(|line| line == "ready", LineParsingOptions::default())
        .await;

    let err = result.expect_err("late subscriber should receive terminal read error");
    assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn replay_wait_for_line_returns_read_error_after_stream_read_fails() {
    let stream = BroadcastOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"booting\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        reliable_options(ReplayRetention::All),
    );

    while shared_backend(&stream)
        .shared
        .state
        .lock()
        .expect("broadcast state poisoned")
        .read_error
        .is_none()
    {
        tokio::task::yield_now().await;
    }

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "partial",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );

    let err = waiter
        .await
        .expect_err("replay subscriber should receive terminal read error");
    assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn collector_and_inspector_return_read_error_when_stream_read_fails() {
    let collector_stream = BroadcastOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"complete\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        reliable_no_replay_options(),
    );
    let collector = collector_stream
        .collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());
    match collector.wait().await {
        Err(CollectorError::StreamRead { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!(
                "expected collector stream read error, got {other:?}"
            ));
        }
    }

    let inspector_stream = BroadcastOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"complete\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        reliable_no_replay_options(),
    );
    let inspector =
        inspector_stream.inspect_lines(|_line| Next::Continue, LineParsingOptions::default());
    match inspector.wait().await {
        Err(InspectorError::StreamRead { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!(
                "expected inspector stream read error, got {other:?}"
            ));
        }
    }
}

#[tokio::test]
async fn subscriber_after_seal_starts_live() {
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
    drop(write_half);

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

#[tokio::test]
async fn block_until_subscribers_catch_up_waits_before_active_lag_exceeds_capacity() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_no_replay_options_with(1.bytes(), 1),
    );
    let mut subscription = stream.subscribe();

    write_half.write_all(b"ab").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    {
        let state = shared_backend(&stream)
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        assert_that!(state.next_seq).is_equal_to(1);
        assert_that!(state.events.len()).is_equal_to(1);
        assert_that!(state.retained_chunk_count).is_equal_to(1);
        assert_that!(state.retained_byte_count).is_equal_to(1);
    }

    match subscription.recv().await {
        Some(StreamEvent::Chunk(chunk)) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"a");
        }
        other => {
            assert_that!(&other).fail(format_args!("expected first chunk, got {other:?}"));
        }
    }
    sleep(Duration::from_millis(20)).await;

    {
        let state = shared_backend(&stream)
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        assert_that!(state.next_seq).is_equal_to(2);
        assert_that!(state.events.len()).is_equal_to(1);
        assert_that!(state.retained_chunk_count).is_equal_to(1);
        assert_that!(state.retained_byte_count).is_equal_to(1);
    }

    match subscription.recv().await {
        Some(StreamEvent::Chunk(chunk)) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"b");
        }
        other => {
            assert_that!(&other).fail(format_args!("expected second chunk, got {other:?}"));
        }
    }
    drop(write_half);
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
async fn collect_chunks_into_write_returns_sink_write_error() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_no_replay_options());
    let collector = stream.collect_chunks_into_write(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"abc").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite {
            stream_name,
            source,
        }) => {
            assert_that!(stream_name).is_equal_to("custom");
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn collect_chunks_into_write_can_continue_after_sink_write_error() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_no_replay_options());
    let handled_count = Arc::new(Mutex::new(0_usize));
    let count_for_handler = Arc::clone(&handled_count);
    let collector = stream.collect_chunks_into_write(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        WriteCollectionOptions::with_error_handler(move |err| {
            assert_that!(err.stream_name()).is_equal_to("custom");
            assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            *count_for_handler.lock().unwrap() += 1;
            SinkWriteErrorAction::Continue
        }),
    );

    write_half.write_all(b"abc").await.unwrap();
    drop(write_half);

    let write = collector.wait().await.unwrap();
    assert_that!(write.bytes_written).is_equal_to(0);
    assert_that!(*handled_count.lock().unwrap()).is_greater_or_equal_to(1);
}

#[tokio::test]
async fn drop_oldest_emits_gap_for_slow_subscriber() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_options_with(ReplayRetention::LastChunks(1), 3.bytes(), 1),
    );
    let mut subscription = stream.subscribe();

    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;

    assert_that!(subscription.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Gap);
    match subscription.recv().await {
        Some(StreamEvent::Chunk(chunk)) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"dy\n");
        }
        other => {
            assert_that!(&other).fail(format_args!("expected tail chunk, got {other:?}"));
        }
    }
    assert_that!(subscription.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Eof);
}

#[tokio::test]
async fn eof_is_replayed_to_late_subscribers_before_seal() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"tail").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;

    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());
    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["tail"]);
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
async fn dropping_slow_subscriber_after_seal_frees_retained_history() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options_with(ReplayRetention::All, 8.bytes(), 8),
    );
    let slow = stream.subscribe();

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    stream.seal_replay();
    {
        let state = shared_backend(&stream)
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        assert_that!(state.events.len()).is_equal_to(1);
        assert_that!(state.retained_chunk_count).is_equal_to(1);
        assert_that!(state.retained_byte_count).is_equal_to(4);
        assert_that!(state.replay_byte_count).is_equal_to(0);
    }

    drop(slow);

    let state = shared_backend(&stream)
        .shared
        .state
        .lock()
        .expect("broadcast state poisoned");
    assert_that!(state.events.len()).is_equal_to(0);
    assert_that!(state.retained_chunk_count).is_equal_to(0);
    assert_that!(state.retained_byte_count).is_equal_to(0);
}

#[tokio::test]
async fn dropping_slow_reliable_subscriber_unpins_buffer() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_no_replay_options_with(1.bytes(), 1),
    );
    let slow = stream.subscribe();
    let mut fast = stream.subscribe();

    write_half.write_all(b"a").await.unwrap();
    write_half.flush().await.unwrap();
    match fast.recv().await {
        Some(StreamEvent::Chunk(chunk)) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"a");
        }
        other => {
            assert_that!(&other).fail(format_args!("expected chunk, got {other:?}"));
        }
    }

    {
        let state = shared_backend(&stream)
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        assert_that!(state.events.len()).is_equal_to(1);
        assert_that!(state.retained_chunk_count).is_equal_to(1);
        assert_that!(state.retained_byte_count).is_equal_to(1);
    }

    drop(slow);

    let state = shared_backend(&stream)
        .shared
        .state
        .lock()
        .expect("broadcast state poisoned");
    assert_that!(state.events.len()).is_equal_to(0);
    assert_that!(state.retained_chunk_count).is_equal_to(0);
    assert_that!(state.retained_byte_count).is_equal_to(0);
}

#[tokio::test]
async fn dropping_stream_closes_waiting_subscribers() {
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
async fn replay_last_chunks_starts_at_retention_boundary() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_options_with(ReplayRetention::LastChunks(2), 2.bytes(), 4),
    );

    write_half.write_all(b"a\nb\nc\n").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;

    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());
    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["b", "c"]);
}

#[tokio::test]
async fn wait_starts_at_bounded_replay_window_when_start_evicted() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options_with(ReplayRetention::LastChunks(1), 1.bytes(), 4),
    );
    let _pinned_subscription = stream.subscribe();

    write_half.write_all(b"ab").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    {
        let state = shared_backend(&stream)
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");
        assert_that!(state.buffer_start_seq).is_equal_to(0);
        assert_that!(state.replay_start_seq).is_equal_to(1);
    }

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "b",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    drop(write_half);

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn replay_last_bytes_keeps_whole_chunks_covering_boundary() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_options_with(ReplayRetention::LastBytes(3.bytes()), 2.bytes(), 4),
    );

    write_half.write_all(b"aabbcc").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;

    let collector = stream.collect_chunks_into_vec(raw_collection_options());
    assert_that!(collector.wait().await.unwrap().bytes).is_equal_to(b"bbcc".to_vec());
}

#[tokio::test]
async fn dropping_slow_subscriber_unblocks_stream_consumption() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_no_replay_options_with(2.bytes(), 1),
    );
    let subscription = stream.subscribe();

    write_half.write_all(b"a\nb\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    drop(subscription);

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "c",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    write_half.write_all(b"c\n").await.unwrap();
    drop(write_half);

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn wait_for_line_with_timeout_subscribes_before_polling() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::LastBytes(1.megabytes())),
    );

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "ready",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn replay_last_bytes_bounds_replay_and_clears_on_seal() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options_with(ReplayRetention::LastBytes(4.bytes()), 2.bytes(), 8),
    );

    write_half.write_all(b"aa\nbb\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "bb",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    drop(write_half);

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));

    stream.seal_replay();

    let state = shared_backend(&stream)
        .shared
        .state
        .lock()
        .expect("broadcast state poisoned");
    assert_that!(state.events.len()).is_equal_to(0);
    assert_that!(state.retained_chunk_count).is_equal_to(0);
    assert_that!(state.retained_byte_count).is_equal_to(0);
    assert_that!(state.replay_byte_count).is_equal_to(0);
}

#[tokio::test]
async fn leptos_browser_test_shaped_startup_race() {
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
