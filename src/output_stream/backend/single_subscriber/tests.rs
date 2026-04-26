use super::{ActiveSubscriber, ConfiguredShared, read_chunked};
use crate::AsyncChunkCollector;
use crate::output_stream::Chunk;
use crate::output_stream::StreamEvent;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::backend::test_support::ReadErrorAfterBytes;
use crate::output_stream::{DeliveryGuarantee, Next, ReplayRetention, StreamConfig};
use crate::{
    CollectorCancelOutcome, CollectorError, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
    InspectorCancelOutcome, LineParsingOptions, NumBytes, NumBytesExt, RawCollectionOptions,
    WaitForLineResult, WriteCollectionOptions,
};
use assertr::prelude::*;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing_test::traced_test;

fn best_effort_no_replay_options() -> StreamConfig {
    best_effort_no_replay_options_with(4.bytes(), 4)
}

fn best_effort_no_replay_options_with(
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig {
    StreamConfig::builder()
        .best_effort_delivery()
        .no_replay()
        .read_chunk_size(read_chunk_size)
        .max_buffered_chunks(max_buffered_chunks)
        .build()
}

fn reliable_replay_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<crate::ReliableDelivery, crate::ReplayEnabled> {
    let builder = StreamConfig::builder().reliable_for_active_subscribers();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .read_chunk_size(4.bytes())
    .max_buffered_chunks(4)
    .build()
}

const ACTIVE_CONSUMER_PANIC: &str = "Cannot create multiple active consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one active inspector, collector, or line waiter can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.";

fn spawn_configured_reader<R>(
    read: R,
    delivery_guarantee: DeliveryGuarantee,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> (tokio::task::JoinHandle<()>, mpsc::Receiver<StreamEvent>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (sender, receiver) = mpsc::channel(max_buffered_chunks);
    let shared = Arc::new(ConfiguredShared::new());
    let active_rx = shared.subscribe_active();
    {
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        let id = state.attach_subscriber();
        shared
            .active_tx
            .send_replace(Some(Arc::new(ActiveSubscriber { id, sender })));
    }

    let stream_reader = tokio::spawn(read_chunked(
        read,
        shared,
        active_rx,
        read_chunk_size,
        delivery_guarantee,
        None,
        "custom",
    ));

    (stream_reader, receiver)
}

async fn wait_for_no_active_consumer(stream: &SingleSubscriberOutputStream) {
    let shared = Arc::clone(stream.configured_shared.as_ref().unwrap());
    for _ in 0..50 {
        {
            let state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");
            if state.active_id.is_none() {
                return;
            }
        }
        sleep(Duration::from_millis(10)).await;
    }

    assert_that!(()).fail("active consumer did not detach");
}

#[derive(Debug)]
struct AlwaysReadyBytes {
    remaining: usize,
    bytes_read: Arc<AtomicUsize>,
}

impl AlwaysReadyBytes {
    fn new(total_bytes: usize, bytes_read: Arc<AtomicUsize>) -> Self {
        Self {
            remaining: total_bytes,
            bytes_read,
        }
    }
}

impl AsyncRead for AlwaysReadyBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        const BYTES: [u8; 1024] = [b'x'; 1024];

        if self.remaining == 0 {
            return Poll::Ready(Ok(()));
        }

        let len = self.remaining.min(buf.remaining()).min(BYTES.len());
        buf.put_slice(&BYTES[..len]);
        self.remaining -= len;
        self.bytes_read.fetch_add(len, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

struct HangingChunkCollector {
    entered_tx: Option<oneshot::Sender<()>>,
}

impl HangingChunkCollector {
    fn new(entered_tx: oneshot::Sender<()>) -> Self {
        Self {
            entered_tx: Some(entered_tx),
        }
    }
}

impl AsyncChunkCollector<Vec<u8>> for HangingChunkCollector {
    async fn collect<'a>(&'a mut self, _chunk: Chunk, _seen: &'a mut Vec<u8>) -> Next {
        if let Some(entered_tx) = self.entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        std::future::pending::<Next>().await
    }
}

struct PendingWrite {
    entered_tx: Option<oneshot::Sender<()>>,
}

impl PendingWrite {
    fn new(entered_tx: oneshot::Sender<()>) -> Self {
        Self {
            entered_tx: Some(entered_tx),
        }
    }
}

impl AsyncWrite for PendingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(entered_tx) = self.entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
#[should_panic(expected = "read_chunk_size must be greater than zero bytes")]
fn from_stream_panics_on_zero_read_chunk_size() {
    let _stream = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        tokio::io::empty(),
        "custom",
        DeliveryGuarantee::BestEffort,
        NumBytes::zero(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
}

#[test]
#[should_panic(expected = "max_buffered_chunks must be greater than zero")]
fn from_stream_panics_on_zero_max_buffered_chunks() {
    let _stream = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        tokio::io::empty(),
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        0,
    );
}

#[tokio::test]
async fn typed_no_replay_starts_consumer_at_live_output() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    write_half.write_all(b"old").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"live".to_vec());
}

#[tokio::test]
async fn typed_replay_all_delivers_pre_consumer_output_before_live_output() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );

    write_half.write_all(b"old").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"oldlive".to_vec());
}

#[tokio::test]
async fn configured_subscription_snapshots_replay_buffer_from_shared_state() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );
    let shared = Arc::clone(stream.configured_shared.as_ref().unwrap());

    write_half.write_all(b"old").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    {
        let state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        assert_that!(state.events.len()).is_equal_to(1);
    }

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);

    {
        let state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        assert_that!(state.events.len()).is_equal_to(1);
    }

    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"oldlive".to_vec());
}

#[tokio::test]
async fn collector_drop_allows_later_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(collector);
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn collector_cancel_allows_later_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    assert_that!(collector.cancel().await.unwrap().bytes).is_empty();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn collector_cancel_waits_for_hanging_async_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let collector = stream.collect_chunks_async(Vec::new(), HangingChunkCollector::new(entered_tx));

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(25), collector.cancel()).await;
    assert_that!(result.is_err()).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn collector_abort_releases_single_subscriber_claim() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let collector = stream.collect_chunks_async(Vec::new(), HangingChunkCollector::new(entered_tx));

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    collector.abort().await;
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn collector_cancel_or_abort_after_returns_cancelled_when_cooperative() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    let outcome = collector
        .cancel_or_abort_after(Duration::from_secs(1))
        .await
        .unwrap();

    match outcome {
        CollectorCancelOutcome::Cancelled(bytes) => {
            assert_that!(bytes.bytes).is_empty();
        }
        CollectorCancelOutcome::Aborted => {
            assert_that!(()).fail("expected cooperative cancellation");
        }
    }
    wait_for_no_active_consumer(&stream).await;
}

#[tokio::test]
async fn collector_cancel_or_abort_after_aborts_hanging_async_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let collector = stream.collect_chunks_async(Vec::new(), HangingChunkCollector::new(entered_tx));

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let outcome = collector
        .cancel_or_abort_after(Duration::from_millis(25))
        .await
        .unwrap();

    assert_that!(matches!(outcome, CollectorCancelOutcome::Aborted)).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn inspector_drop_allows_later_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let inspector = stream.inspect_chunks(|_chunk| Next::Continue);
    drop(inspector);
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn inspector_wait_cancellation_releases_single_subscriber_claim() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let inspector = stream.inspect_lines_async(
        move |_line| {
            if let Some(entered_tx) = entered_tx.take() {
                entered_tx.send(()).unwrap();
            }
            async move { std::future::pending::<Next>().await }
        },
        LineParsingOptions::default(),
    );

    write_half.write_all(b"ready\n").await.unwrap();
    entered_rx.await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(25), inspector.wait()).await;
    assert_that!(result.is_err()).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later\n").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later\n".to_vec());
}

#[tokio::test]
async fn inspector_cancel_waits_for_hanging_async_callback() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let inspector = stream.inspect_chunks_async(move |_chunk| {
        if let Some(entered_tx) = entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        async move { std::future::pending::<Next>().await }
    });

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(25), inspector.cancel()).await;
    assert_that!(result.is_err()).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn inspector_abort_releases_single_subscriber_claim() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let inspector = stream.inspect_chunks_async(move |_chunk| {
        if let Some(entered_tx) = entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        async move { std::future::pending::<Next>().await }
    });

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    inspector.abort().await;
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn inspector_cancel_or_abort_after_returns_cancelled_when_cooperative() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let inspector = stream.inspect_chunks(|_chunk| Next::Continue);
    let outcome = inspector
        .cancel_or_abort_after(Duration::from_secs(1))
        .await
        .unwrap();

    assert_that!(outcome).is_equal_to(InspectorCancelOutcome::Cancelled);
    wait_for_no_active_consumer(&stream).await;
}

#[tokio::test]
async fn inspector_cancel_or_abort_after_aborts_hanging_callback() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let mut entered_tx = Some(entered_tx);
    let inspector = stream.inspect_chunks_async(move |_chunk| {
        if let Some(entered_tx) = entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        async move { std::future::pending::<Next>().await }
    });

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let outcome = inspector
        .cancel_or_abort_after(Duration::from_millis(25))
        .await
        .unwrap();

    assert_that!(outcome).is_equal_to(InspectorCancelOutcome::Aborted);
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn wait_for_line_timeout_allows_later_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let result = stream
        .wait_for_line_with_timeout(
            |line| line == "ready",
            LineParsingOptions::default(),
            Duration::from_millis(25),
        )
        .await;
    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Timeout));
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"ready\n".to_vec());
}

#[tokio::test]
async fn reader_drains_after_consumer_drop() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(16.bytes())
            .max_buffered_chunks(1)
            .build(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(collector);
    wait_for_no_active_consumer(&stream).await;

    let idle_output = vec![b'x'; 4096];
    tokio::time::timeout(Duration::from_secs(1), write_half.write_all(&idle_output))
        .await
        .expect("reader should keep draining with no active consumer")
        .unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"tail").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"tail".to_vec());
}

#[tokio::test]
async fn no_replay_discards_output_between_consumers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    assert_that!(collector.cancel().await.unwrap().bytes).is_empty();
    wait_for_no_active_consumer(&stream).await;

    write_half.write_all(b"idle").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_empty();
}

#[tokio::test]
async fn replay_retains_output_between_consumers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    assert_that!(collector.cancel().await.unwrap().bytes).is_empty();
    wait_for_no_active_consumer(&stream).await;

    write_half.write_all(b"idle").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"idle".to_vec());
}

#[tokio::test]
async fn replay_retention_limits_later_consumer() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::LastChunks(1)),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    assert_that!(collector.cancel().await.unwrap().bytes).is_empty();
    wait_for_no_active_consumer(&stream).await;

    write_half.write_all(b"aabbcc").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"cc".to_vec());
}

#[tokio::test]
async fn replay_last_bytes_keeps_whole_chunks_covering_boundary() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .replay_last_bytes(3.bytes())
            .read_chunk_size(2.bytes())
            .max_buffered_chunks(4)
            .build(),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    assert_that!(collector.cancel().await.unwrap().bytes).is_empty();
    wait_for_no_active_consumer(&stream).await;

    write_half.write_all(b"aabbcc").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"bbcc".to_vec());
}

#[tokio::test]
async fn later_consumer_observes_eof() {
    let (read_half, write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    drop(write_half);
    sleep(Duration::from_millis(25)).await;

    let bytes = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .wait()
        .await
        .unwrap();
    assert_that!(bytes.bytes).is_empty();
}

#[tokio::test]
async fn later_consumer_observes_read_error() {
    let stream = SingleSubscriberOutputStream::from_stream(
        ReadErrorAfterBytes::new(b"before-error", io::ErrorKind::BrokenPipe),
        "custom",
        best_effort_no_replay_options(),
    );

    sleep(Duration::from_millis(25)).await;

    match stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .wait()
        .await
    {
        Err(CollectorError::StreamRead { source, .. }) => {
            assert_that!(source.stream_name()).is_equal_to("custom");
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected stream read error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn configured_subscription_after_seal_starts_live_with_empty_shared_replay() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );
    let shared = Arc::clone(stream.configured_shared.as_ref().unwrap());

    write_half.write_all(b"old").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    {
        let state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        assert_that!(state.events.len()).is_equal_to(1);
    }

    stream.seal_replay();
    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);

    {
        let state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        assert_that!(state.events.len()).is_equal_to(0);
    }

    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"live".to_vec());
}

#[tokio::test]
async fn configured_second_subscriber_panic_does_not_poison_state_or_stop_reader() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);

    assert_that_panic_by(|| {
        let _waiter = stream.wait_for_line_with_timeout(
            |_line| false,
            LineParsingOptions::default(),
            Duration::from_millis(25),
        );
    })
    .has_type::<String>()
    .is_equal_to(ACTIVE_CONSUMER_PANIC);

    assert_that!(stream.is_replay_sealed()).is_false();

    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"live".to_vec());
}

#[tokio::test]
async fn typed_replay_last_chunks_starts_at_retention_boundary() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::LastChunks(1)),
    );

    write_half.write_all(b"aaaabbbb").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;
    drop(write_half);

    let bytes = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .wait()
        .await
        .unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"bbbb".to_vec());
}

#[tokio::test]
async fn typed_wait_after_seal_starts_live() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let options = StreamConfig::builder()
        .reliable_for_active_subscribers()
        .replay_all()
        .read_chunk_size(4.bytes())
        .max_buffered_chunks(4)
        .build();
    let stream = SingleSubscriberOutputStream::from_stream(read_half, "custom", options);

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;
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
#[traced_test]
async fn configured_reader_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
{
    let (read_half, mut write_half) = tokio::io::duplex(64);

    // Let's preemptively write more data into the stream than our later selected chunk size (2)
    // can handle, forcing the initial read to completely fill our chunk buffer.
    // Our expectation is that we still receive all data written here through multiple
    // consecutive reads.
    // The behavior of bytes::BytesMut, potentially reaching zero capacity when splitting a
    // full buffer of, must not prevent this from happening but allocate more memory instead!
    write_half.write_all(b"hello world").await.unwrap();
    write_half.flush().await.unwrap();

    let (stream_reader, mut rx) =
        spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 64);

    drop(write_half); // This closes the stream and should let stream_reader terminate.
    stream_reader.await.unwrap();

    let mut chunks = Vec::<String>::new();
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Chunk(chunk) => {
                chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
            }
            StreamEvent::Gap => {}
            StreamEvent::Eof => break,
            StreamEvent::ReadError(err) => {
                assert_that!(&err).fail(format_args!("unexpected read error: {err}"));
            }
        }
    }
    assert_that!(chunks).contains_exactly(["he", "ll", "o ", "wo", "rl", "d"]);
}

#[tokio::test]
async fn configured_reader_sends_pending_gap_before_terminal_eof() {
    let read = Cursor::new(b"aabbcc".to_vec());
    let (stream_reader, mut rx) =
        spawn_configured_reader(read, DeliveryGuarantee::BestEffort, 2.bytes(), 1);

    match rx.recv().await.unwrap() {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"aa".as_slice());
        }
        other => {
            assert_that!(&other).fail(format_args!("expected first chunk, got {other:?}"));
        }
    }

    let mut previous = None;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Eof => {
                assert_that!(previous).is_equal_to(Some(StreamEvent::Gap));
                break;
            }
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).fail("dropped chunks should not be delivered");
            }
            event @ StreamEvent::Gap => {
                previous = Some(event);
            }
            StreamEvent::ReadError(err) => {
                assert_that!(&err).fail(format_args!("unexpected read error: {err}"));
            }
        }
    }

    stream_reader.await.unwrap();
    assert_that!(rx.recv().await).is_none();
}

#[tokio::test(flavor = "current_thread")]
async fn configured_best_effort_yields_when_pending_gap_channel_is_full() {
    let total_bytes = 1024;
    let bytes_read = Arc::new(AtomicUsize::new(0));
    let (stream_reader, mut rx) = spawn_configured_reader(
        AlwaysReadyBytes::new(total_bytes, Arc::clone(&bytes_read)),
        DeliveryGuarantee::BestEffort,
        1.bytes(),
        1,
    );

    match rx.recv().await.unwrap() {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"x".as_slice());
        }
        other => {
            assert_that!(&other).fail(format_args!("expected first chunk, got {other:?}"));
        }
    }

    let observed = bytes_read.load(Ordering::Relaxed);
    assert_that!(observed < total_bytes)
        .with_detail_message(format!(
            "reader consumed all {total_bytes} bytes before yielding"
        ))
        .is_true();

    drop(rx);
    stream_reader.await.unwrap();
}

#[tokio::test]
async fn configured_reader_sends_pending_gap_before_resumed_chunk_delivery() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let (stream_reader, mut rx) =
        spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 2);

    write_half.write_all(b"aabbcc").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    for expected in [b"aa".as_slice(), b"bb".as_slice()] {
        match rx.recv().await.unwrap() {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(expected);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected buffered chunk, got {other:?}"));
            }
        }
    }

    write_half.write_all(b"dd").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Gap);
    match rx.recv().await.unwrap() {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"dd".as_slice());
        }
        other => {
            assert_that!(&other).fail(format_args!("expected resumed chunk, got {other:?}"));
        }
    }
    assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Eof);

    stream_reader.await.unwrap();
    assert_that!(rx.recv().await).is_none();
}

#[tokio::test]
async fn configured_reader_publishes_read_error_without_panicking() {
    let (stream_reader, mut rx) = spawn_configured_reader(
        ReadErrorAfterBytes::new(b"ready\n", io::ErrorKind::BrokenPipe),
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        64,
    );

    stream_reader.await.unwrap();

    let mut saw_error = false;
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Chunk(_) | StreamEvent::Gap => {}
            StreamEvent::Eof => {
                assert_that!(()).fail("read failure must not be reported as EOF");
            }
            StreamEvent::ReadError(err) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
                saw_error = true;
                break;
            }
        }
    }

    assert_that!(saw_error).is_true();
}

#[tokio::test]
async fn dropping_stream_closes_waiting_subscribers() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let waiter = stream.wait_for_line(|line| line == "never", LineParsingOptions::default());
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn wait_for_line_claims_receiver_before_polling() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let _waiter = stream.wait_for_line(|_line| false, LineParsingOptions::default());

    assert_that_panic_by(|| {
        let _second_waiter = stream.wait_for_line(|_line| false, LineParsingOptions::default());
    })
    .has_type::<String>()
    .is_equal_to(ACTIVE_CONSUMER_PANIC);
}

#[tokio::test]
#[traced_test]
async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        2,
    );

    let inspector = os.inspect_lines_async(
        |_line| async move {
            // Mimic a slow consumer.
            sleep(Duration::from_millis(100)).await;
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    #[rustfmt::skip]
        let producer = tokio::spawn(async move {
            for count in 1..=15 {
                write_half
                    .write_all(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                sleep(Duration::from_millis(25)).await;
            }
        });

    producer.await.unwrap();
    inspector.wait().await.unwrap();
    drop(os);

    logs_assert(|lines: &[&str]| {
        let lagged_logs = lines
            .iter()
            .filter(|line| line.contains("Stream reader is lagging behind lagged="))
            .count();
        if lagged_logs == 0 {
            return Err("Expected at least one lagged log".to_string());
        }
        Ok(())
    });
}

#[tokio::test]
async fn writer_collector_cancel_or_abort_after_aborts_pending_write() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let collector = stream.collect_chunks_into_write(
        PendingWrite::new(entered_tx),
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let outcome = collector
        .cancel_or_abort_after(Duration::from_millis(25))
        .await
        .unwrap();

    assert_that!(matches!(outcome, CollectorCancelOutcome::Aborted)).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
#[traced_test]
async fn multiple_subscribers_are_not_possible() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let _inspector = os.inspect_lines(|_line| Next::Continue, LineParsingOptions::default());

    // Doesn't matter if we call `inspect_lines` or some other "consuming" function instead.
    assert_that_panic_by(move || {
        os.inspect_lines(|_line| Next::Continue, LineParsingOptions::default())
    })
    .has_type::<String>()
    .is_equal_to(ACTIVE_CONSUMER_PANIC);
}
