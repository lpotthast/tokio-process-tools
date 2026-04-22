use super::{ConfiguredShared, read_chunked};
use crate::ReplaySubscribeError;
use crate::output_stream::Chunk;
use crate::output_stream::StreamEvent;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::backend::test_support::{
    FailingWrite, ReadErrorAfterBytes, write_test_data,
};
use crate::output_stream::{
    DeliveryGuarantee, LineWriteMode, Next, ReplayRetention, SealedReplayBehavior, StreamConfig,
};
use crate::{AsyncChunkCollector, AsyncLineCollector};
use crate::{
    CollectionOverflowBehavior, CollectorError, DEFAULT_MAX_BUFFERED_CHUNKS,
    DEFAULT_READ_CHUNK_SIZE, InspectorError, LineCollectionOptions, LineParsingOptions, NumBytes,
    NumBytesExt, SinkWriteErrorAction, SinkWriteOperation, WaitForLineResult,
    WriteCollectionOptions,
};
use assertr::prelude::*;
use atomic_take::AtomicTake;
use bytes::Bytes;
use mockall::{automock, predicate};
use std::borrow::Cow;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing_test::traced_test;

fn line_collection_options() -> LineCollectionOptions {
    LineCollectionOptions::builder()
        .max_bytes(1.megabytes())
        .max_lines(1024)
        .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
        .build()
}

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
    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
    .read_chunk_size(4.bytes())
    .max_buffered_chunks(4)
    .build()
}

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
    {
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.consumer_attached = true;
    }

    let stream_reader = tokio::spawn(read_chunked(
        read,
        shared,
        sender,
        read_chunk_size,
        delivery_guarantee,
        None,
        "custom",
    ));

    (stream_reader, receiver)
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

struct BreakOnLine;

impl AsyncLineCollector<Vec<String>> for BreakOnLine {
    async fn collect<'a>(&'a mut self, line: Cow<'a, str>, seen: &'a mut Vec<String>) -> Next {
        if line == "break" {
            seen.push(line.into_owned());
            Next::Break
        } else {
            seen.push(line.into_owned());
            Next::Continue
        }
    }
}

struct WriteLine;

impl AsyncLineCollector<std::fs::File> for WriteLine {
    async fn collect<'a>(
        &'a mut self,
        line: Cow<'a, str>,
        temp_file: &'a mut std::fs::File,
    ) -> Next {
        writeln!(temp_file, "{line}").unwrap();
        Next::Continue
    }
}

struct ExtendChunks;

impl AsyncChunkCollector<Vec<u8>> for ExtendChunks {
    async fn collect<'a>(&'a mut self, chunk: Chunk, seen: &'a mut Vec<u8>) -> Next {
        seen.extend_from_slice(chunk.as_ref());
        Next::Continue
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

    let collector = stream.collect_all_chunks_into_vec_trusted();
    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes).is_equal_to(b"live".to_vec());
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

    let collector = stream.collect_all_chunks_into_vec_trusted();
    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes).is_equal_to(b"oldlive".to_vec());
}

#[tokio::test]
async fn configured_subscription_takes_replay_buffer_from_shared_state() {
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

    let collector = stream.collect_all_chunks_into_vec_trusted();

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
    assert_that!(bytes).is_equal_to(b"oldlive".to_vec());
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
    let collector = stream.collect_all_chunks_into_vec_trusted();

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
    assert_that!(bytes).is_equal_to(b"live".to_vec());
}

#[tokio::test]
async fn configured_second_subscriber_panic_does_not_poison_state_or_stop_reader() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );

    let collector = stream.collect_all_chunks_into_vec_trusted();

    assert_that_panic_by(|| {
            let _waiter = stream.try_wait_for_line_from_start_with_timeout(
                |_line| false,
                LineParsingOptions::default(),
                Duration::from_millis(25),
            );
        })
        .has_type::<String>()
        .is_equal_to("Cannot create multiple consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one inspector or collector can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.");

    assert_that!(stream.is_replay_sealed()).is_false();

    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes).is_equal_to(b"live".to_vec());
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
        .collect_all_chunks_into_vec_trusted()
        .wait()
        .await
        .unwrap();
    assert_that!(bytes).is_equal_to(b"bbbb".to_vec());
}

#[tokio::test]
async fn typed_replay_from_start_after_seal_can_be_rejected() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let options = StreamConfig::builder()
        .reliable_for_active_subscribers()
        .replay_all()
        .sealed_replay_behavior(SealedReplayBehavior::RejectReplaySubscribers)
        .read_chunk_size(4.bytes())
        .max_buffered_chunks(4)
        .build();
    let stream = SingleSubscriberOutputStream::from_stream(read_half, "custom", options);

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;
    stream.seal_replay();

    let result = stream.try_wait_for_line_from_start_with_timeout(
        |line| line == "ready",
        LineParsingOptions::default(),
        Duration::from_millis(25),
    );

    match result {
        Err(err) => {
            assert_that!(err).is_equal_to(ReplaySubscribeError::ReplaySealed);
        }
        Ok(_waiter) => {
            assert_that!(()).fail("replay should be rejected");
        }
    }
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
async fn wait_for_line_returns_matched_when_line_appears_before_eof() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let waiter = os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default());

    write_half.write_all(b"booting\nready\n").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let result = waiter.await;
    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn wait_for_line_returns_stream_closed_when_stream_ends_before_match() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let waiter = os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default());

    write_half
        .write_all(b"booting\nstill starting\n")
        .await
        .unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let result = waiter.await;
    assert_that!(result).is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn wait_for_line_returns_matched_for_partial_final_line_at_eof() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let waiter = os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default());

    write_half.write_all(b"booting\nready").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let result = waiter.await;
    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn wait_for_line_with_timeout_returns_timeout_while_stream_stays_open() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let result = os
        .wait_for_line_with_timeout(
            |line| line.contains("ready"),
            LineParsingOptions::default(),
            Duration::from_millis(25),
        )
        .await;

    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Timeout));
}

#[tokio::test]
async fn wait_for_line_subscribes_before_polling() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let waiter = stream.wait_for_line(|line| line == "ready", LineParsingOptions::default());
    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn wait_for_line_with_timeout_subscribes_before_polling() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
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
async fn wait_for_line_claims_receiver_before_polling() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let _waiter = stream.wait_for_line(|_line| false, LineParsingOptions::default());

    assert_that_panic_by(|| {
            let _second_waiter =
                stream.wait_for_line(|_line| false, LineParsingOptions::default());
        })
        .has_type::<String>()
        .is_equal_to("Cannot create multiple consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one inspector or collector can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.");
}

#[tokio::test]
async fn wait_for_line_returns_read_error_when_stream_read_fails() {
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        ReadErrorAfterBytes::new(b"booting\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let result = os
        .wait_for_line(|line| line == "partial", LineParsingOptions::default())
        .await;

    let err = result.expect_err("read failure should be surfaced");
    assert_that!(err.stream_name()).is_equal_to("custom");
    assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn collector_and_inspector_return_read_error_when_stream_read_fails() {
    let collector_stream = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        ReadErrorAfterBytes::new(b"complete\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
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

    let inspector_stream = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        ReadErrorAfterBytes::new(b"complete\npartial", io::ErrorKind::BrokenPipe),
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
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
async fn wait_for_line_returns_stream_closed_when_stream_ends_after_writes_without_match() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    write_half.write_all(b"booting\n").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    // No yield needed: `SingleSubscriberOutputStream` is built on an mpsc channel
    // that buffers every chunk and the terminal EOF event regardless of when the
    // consumer attaches, so this is race-free by construction.
    let result = os
        .wait_for_line(|line| line.contains("ready"), LineParsingOptions::default())
        .await;

    assert_that!(result).is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn wait_for_line_does_not_match_across_explicit_gap_event() {
    let (tx, rx) = mpsc::channel::<StreamEvent>(4);
    let os = SingleSubscriberOutputStream {
        stream_reader: tokio::spawn(async {}),
        receiver: AtomicTake::new(rx),
        read_chunk_size: 4.bytes(),
        max_buffered_chunks: 4,
        configured_shared: None,
        replay_retention: None,
        sealed_replay_behavior: None,
        replay_enabled: false,
        name: "custom",
    };

    tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"rea"))))
        .await
        .unwrap();
    tx.send(StreamEvent::Gap).await.unwrap();
    tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"dy\n"))))
        .await
        .unwrap();
    tx.send(StreamEvent::Eof).await.unwrap();
    drop(tx);

    let result = os
        .wait_for_line(|line| line == "ready", LineParsingOptions::default())
        .await;

    assert_that!(result).is_equal_to(Ok(WaitForLineResult::StreamClosed));
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
async fn inspect_lines() {
    #[automock]
    trait LineVisitor {
        fn visit(&self, line: String);
    }

    #[rustfmt::skip]
        fn configure(mock: &mut MockLineVisitor) {
            mock.expect_visit().with(predicate::eq("Cargo.lock".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("Cargo.toml".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("README.md".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("src".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("target".to_string())).times(1).return_const(());
        }

    let (read_half, write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let mut mock = MockLineVisitor::new();
    configure(&mut mock);

    let inspector = os.inspect_lines(
        move |line| {
            mock.visit(line.into_owned());
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    tokio::spawn(write_test_data(write_half)).await.unwrap();

    inspector.cancel().await.unwrap();
    drop(os);
}

#[tokio::test]
async fn inspect_chunks_accepts_stateful_callback() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let (count_tx, count_rx) = oneshot::channel();
    let mut chunk_count = 0;
    let mut count_tx = Some(count_tx);
    let inspector = os.inspect_chunks(move |_chunk| {
        chunk_count += 1;
        if chunk_count == 3 {
            count_tx.take().unwrap().send(chunk_count).unwrap();
            Next::Break
        } else {
            Next::Continue
        }
    });

    write_half.write_all(b"abcdef").await.unwrap();
    drop(write_half);

    inspector.wait().await.unwrap();
    let chunk_count = count_rx.await.unwrap();
    assert_that!(chunk_count).is_equal_to(3);
    drop(os);
}

/// This tests that our impl macros properly `break 'outer`, as they might be in an inner loop!
/// With `break` instead of `break 'outer`, this test would never complete, as the `Next::Break`
/// would not terminate the collector!
#[tokio::test]
#[traced_test]
async fn inspect_lines_async() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        32.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let seen: Vec<String> = Vec::new();
    let collector = os.collect_lines_async(seen, BreakOnLine, LineParsingOptions::default());

    let _writer = tokio::spawn(async move {
        write_half.write_all("start\n".as_bytes()).await.unwrap();
        write_half.write_all("break\n".as_bytes()).await.unwrap();
        write_half.write_all("end\n".as_bytes()).await.unwrap();

        loop {
            write_half
                .write_all("gibberish\n".as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let seen = collector.wait().await.unwrap();

    assert_that!(seen).contains_exactly(["start", "break"]);
}

#[tokio::test]
async fn inspect_lines_async_preserves_unterminated_final_line() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_in_task = Arc::clone(&seen);
    let inspector = os.inspect_lines_async(
        move |line| {
            let seen = Arc::clone(&seen_in_task);
            let line = line.into_owned();
            async move {
                seen.lock().expect("lock").push(line);
                Next::Continue
            }
        },
        LineParsingOptions::default(),
    );

    write_half.write_all(b"tail").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    inspector.wait().await.unwrap();

    let seen = seen.lock().expect("lock").clone();
    assert_that!(seen).contains_exactly(["tail"]);
    drop(os);
}

#[tokio::test]
async fn collect_chunks_async_into_vec() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let collector = os.collect_chunks_async(Vec::new(), ExtendChunks);

    write_half.write_all(b"abcdef").await.unwrap();
    drop(write_half);

    let seen = collector.wait().await.unwrap();
    assert_that!(seen).is_equal_to(b"abcdef".to_vec());
}

#[tokio::test]
async fn collect_chunks_accepts_stateful_callback() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let mut chunk_index = 0;
    let collector = os.collect_chunks(Vec::new(), move |chunk, indexed_chunks| {
        chunk_index += 1;
        indexed_chunks.push((chunk_index, chunk.as_ref().to_vec()));
    });

    write_half.write_all(b"abcdef").await.unwrap();
    drop(write_half);

    let indexed_chunks = collector.wait().await.unwrap();
    assert_that!(indexed_chunks).is_equal_to(vec![
        (1, b"ab".to_vec()),
        (2, b"cd".to_vec()),
        (3, b"ef".to_vec()),
    ]);
}

#[tokio::test]
async fn collect_lines_to_file() {
    let (read_half, write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        32,
    );

    let temp_file = tempfile::tempfile().unwrap();
    let collector = os.collect_lines(
        temp_file,
        |line, temp_file| {
            writeln!(temp_file, "{line}").unwrap();
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    tokio::spawn(write_test_data(write_half)).await.unwrap();

    let mut temp_file = collector.cancel().await.unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();
    let mut contents = String::new();
    temp_file.read_to_string(&mut contents).unwrap();

    assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
}

#[tokio::test]
async fn collect_lines_accepts_stateful_callback() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let mut line_index = 0;
    let collector = os.collect_lines(
        Vec::new(),
        move |line, indexed_lines| {
            line_index += 1;
            indexed_lines.push(format!("{line_index}:{line}"));
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    write_half.write_all(b"alpha\nbeta\ngamma\n").await.unwrap();
    drop(write_half);

    let indexed_lines = collector.wait().await.unwrap();
    assert_that!(indexed_lines).is_equal_to(vec![
        "1:alpha".to_string(),
        "2:beta".to_string(),
        "3:gamma".to_string(),
    ]);
}

#[tokio::test]
async fn collect_lines_async_to_file() {
    let (read_half, write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        32.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let temp_file = tempfile::tempfile().unwrap();
    let collector = os.collect_lines_async(temp_file, WriteLine, LineParsingOptions::default());

    tokio::spawn(write_test_data(write_half)).await.unwrap();

    let mut temp_file = collector.cancel().await.unwrap();
    temp_file.seek(SeekFrom::Start(0)).unwrap();
    let mut contents = String::new();
    temp_file.read_to_string(&mut contents).unwrap();

    assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
}

#[tokio::test]
async fn collect_lines_into_write_respects_requested_line_delimiter_mode() {
    let (read_half, write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let temp_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
    let collector = os.collect_lines_into_write(
        temp_file,
        LineParsingOptions::default(),
        LineWriteMode::AsIs,
        WriteCollectionOptions::fail_fast(),
    );

    tokio::spawn(write_test_data(write_half)).await.unwrap();

    let mut temp_file = collector.cancel().await.unwrap();
    temp_file.seek(SeekFrom::Start(0)).await.unwrap();
    let mut contents = String::new();
    temp_file.read_to_string(&mut contents).await.unwrap();

    assert_that!(contents).is_equal_to("Cargo.lockCargo.tomlREADME.mdsrctarget");
}

#[tokio::test]
#[traced_test]
async fn collect_chunks_into_write_mapped() {
    let (read_half, write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::BestEffort,
        32.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );

    let temp_file = tokio::fs::File::options()
        .create(true)
        .truncate(true)
        .write(true)
        .read(true)
        .open(std::env::temp_dir().join(
            "tokio_process_tools_test_single_subscriber_collect_chunks_into_write_mapped.txt",
        ))
        .await
        .unwrap();

    let collector = os.collect_chunks_into_write_mapped(
        temp_file,
        |chunk| String::from_utf8_lossy(chunk.as_ref()).to_string(),
        WriteCollectionOptions::fail_fast(),
    );

    tokio::spawn(write_test_data(write_half)).await.unwrap();

    let mut temp_file = collector.cancel().await.unwrap();
    temp_file.seek(SeekFrom::Start(0)).await.unwrap();
    let mut contents = String::new();
    temp_file.read_to_string(&mut contents).await.unwrap();

    assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
}

#[tokio::test]
async fn collect_chunks_into_write_returns_sink_write_error() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = os.collect_chunks_into_write(
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
async fn collect_chunks_into_write_mapped_returns_sink_write_error() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = os.collect_chunks_into_write_mapped(
        FailingWrite::new(0, io::ErrorKind::ConnectionReset),
        |chunk| chunk,
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"abc").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::ConnectionReset);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn collect_lines_into_write_returns_sink_write_error_for_line_bytes() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = os.collect_lines_into_write(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        LineParsingOptions::default(),
        LineWriteMode::AppendLf,
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"one\n").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn collect_lines_into_write_returns_sink_write_error_for_appended_delimiter() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = os.collect_lines_into_write(
        FailingWrite::new(1, io::ErrorKind::WriteZero),
        LineParsingOptions::default(),
        LineWriteMode::AppendLf,
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"one\n").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::WriteZero);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn collect_lines_into_write_mapped_returns_sink_write_error() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = os.collect_lines_into_write_mapped(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        |line| line.into_owned().into_bytes(),
        LineParsingOptions::default(),
        LineWriteMode::AsIs,
        WriteCollectionOptions::fail_fast(),
    );

    write_half.write_all(b"one\n").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn write_error_handler_can_continue_after_sink_write_errors() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let handled_events = Arc::clone(&events);
    let collector = os.collect_lines_into_write(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        LineParsingOptions::default(),
        LineWriteMode::AppendLf,
        WriteCollectionOptions::with_error_handler(move |err| {
            handled_events.lock().unwrap().push((
                err.stream_name(),
                err.operation(),
                err.attempted_len(),
                err.source().kind(),
            ));
            SinkWriteErrorAction::Continue
        }),
    );

    write_half.write_all(b"a\nb\n").await.unwrap();
    drop(write_half);

    let write = collector.wait().await.unwrap();
    assert_that!(write.bytes_written).is_equal_to(0);
    assert_that!(events.lock().unwrap().as_slice()).is_equal_to([
        (
            "custom",
            SinkWriteOperation::Line,
            1,
            io::ErrorKind::BrokenPipe,
        ),
        (
            "custom",
            SinkWriteOperation::Line,
            1,
            io::ErrorKind::BrokenPipe,
        ),
    ]);
}

#[tokio::test]
async fn write_error_handler_can_continue_then_stop() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        1.bytes(),
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let handled_count = Arc::new(Mutex::new(0_usize));
    let count_for_handler = Arc::clone(&handled_count);
    let collector = os.collect_chunks_into_write(
        FailingWrite::new(0, io::ErrorKind::BrokenPipe),
        WriteCollectionOptions::with_error_handler(move |err| {
            assert_that!(err.operation()).is_equal_to(SinkWriteOperation::Chunk);
            let mut count = count_for_handler.lock().unwrap();
            *count += 1;
            if *count == 1 {
                SinkWriteErrorAction::Continue
            } else {
                SinkWriteErrorAction::Stop
            }
        }),
    );

    write_half.write_all(b"ab").await.unwrap();
    drop(write_half);

    match collector.wait().await {
        Err(CollectorError::SinkWrite { source, .. }) => {
            assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
        }
    }
    assert_that!(*handled_count.lock().unwrap()).is_equal_to(2);
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
        .is_equal_to("Cannot create multiple consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one inspector or collector can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.");
}

#[tokio::test]
async fn bounded_line_collection_drains_until_eof_after_limit_is_reached() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream_with_delivery_guarantee(
        read_half,
        "custom",
        DeliveryGuarantee::ReliableForActiveSubscribers,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS,
    );
    let collector = stream.collect_lines_into_vec(
        LineParsingOptions::default(),
        LineCollectionOptions::builder()
            .max_bytes(3.bytes())
            .max_lines(1)
            .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
            .build(),
    );

    write_half.write_all(b"one\ntwo\nthree\n").await.unwrap();
    drop(write_half);

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["one"]);
    assert_that!(collected.truncated()).is_true();
}
