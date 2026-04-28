use super::super::reader::{read_chunked_best_effort, read_chunked_reliable};
use super::super::state::{ActiveSubscriber, ConfiguredShared};
use super::common::wait_for_no_active_consumer;
use crate::output_stream::Next;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::DeliveryGuarantee;
use crate::test_support::ReadErrorAfterBytes;
use crate::{
    DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, NumBytes, NumBytesExt, RawCollectionOptions,
};
use assertr::prelude::*;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing_test::traced_test;

fn spawn_configured_reader<R>(
    read: R,
    delivery_guarantee: DeliveryGuarantee,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::Receiver<StreamEvent>,
    Arc<ConfiguredShared>,
)
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

    let stream_reader = match delivery_guarantee {
        DeliveryGuarantee::BestEffort => tokio::spawn(read_chunked_best_effort(
            read,
            Arc::clone(&shared),
            active_rx,
            read_chunk_size,
            None,
            "custom",
        )),
        DeliveryGuarantee::ReliableForActiveSubscribers => tokio::spawn(read_chunked_reliable(
            read,
            Arc::clone(&shared),
            active_rx,
            read_chunk_size,
            None,
            "custom",
        )),
    };

    (stream_reader, receiver, shared)
}

async fn wait_for_terminal_event(shared: &ConfiguredShared) -> StreamEvent {
    for _ in 0..50 {
        {
            let state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");
            if let Some(event) = &state.terminal_event {
                return event.clone();
            }
        }
        sleep(Duration::from_millis(10)).await;
    }

    panic!("terminal event was not recorded");
}

async fn recv_event_with_timeout(rx: &mut mpsc::Receiver<StreamEvent>) -> StreamEvent {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timed out waiting for stream event")
        .expect("stream closed before expected event")
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

#[tokio::test]
#[traced_test]
async fn configured_reader_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
 {
    let (read_half, mut write_half) = tokio::io::duplex(64);

    // Write more data than the configured chunk size so the initial read can fill the buffer.
    write_half.write_all(b"hello world").await.unwrap();
    write_half.flush().await.unwrap();

    let (stream_reader, mut rx, _shared) =
        spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 64);

    drop(write_half);
    tokio::time::timeout(Duration::from_secs(1), stream_reader)
        .await
        .expect("stream reader did not finish")
        .unwrap();

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

#[tokio::test(flavor = "multi_thread")]
async fn configured_reader_sends_pending_gap_before_terminal_eof() {
    let read = Cursor::new(b"aabbcc".to_vec());
    let (stream_reader, mut rx, shared) =
        spawn_configured_reader(read, DeliveryGuarantee::BestEffort, 2.bytes(), 1);
    drop(shared);

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
            StreamEvent::Chunk(_) => {}
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

#[tokio::test(flavor = "multi_thread")]
async fn configured_reader_records_eof_while_active_queue_is_full() {
    let read = Cursor::new(b"aabbcc".to_vec());
    let (stream_reader, mut rx, shared) =
        spawn_configured_reader(read, DeliveryGuarantee::BestEffort, 2.bytes(), 1);

    let terminal =
        tokio::time::timeout(Duration::from_secs(1), wait_for_terminal_event(&shared))
            .await
            .expect("EOF should be recorded even while live delivery is blocked");
    assert_that!(terminal).is_equal_to(StreamEvent::Eof);
    assert_that!(stream_reader.is_finished()).is_false();
    drop(shared);

    match recv_event_with_timeout(&mut rx).await {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"aa".as_slice());
        }
        other => {
            assert_that!(&other).fail(format_args!("expected buffered chunk, got {other:?}"));
        }
    }
    assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Gap);
    assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Eof);

    stream_reader.await.unwrap();
    assert_that!(rx.recv().await).is_none();
}

#[tokio::test(flavor = "current_thread")]
async fn configured_best_effort_yields_when_pending_gap_channel_is_full() {
    let total_bytes = 1024;
    let bytes_read = Arc::new(AtomicUsize::new(0));
    let (stream_reader, mut rx, _shared) = spawn_configured_reader(
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

#[tokio::test(flavor = "multi_thread")]
async fn configured_reader_sends_pending_gap_before_resumed_chunk_delivery() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let (stream_reader, mut rx, shared) =
        spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 2);
    drop(shared);

    write_half.write_all(b"aabbcc").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(25)).await;

    for expected in [b"aa".as_slice(), b"bb".as_slice()] {
        match rx.recv().await.unwrap() {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(expected);
            }
            other => {
                assert_that!(&other)
                    .fail(format_args!("expected buffered chunk, got {other:?}"));
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
    let (stream_reader, mut rx, _shared) = spawn_configured_reader(
        ReadErrorAfterBytes::new(b"ready\n", io::ErrorKind::BrokenPipe),
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        64,
    );

    stream_reader.await.unwrap();

    let mut saw_error = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("timed out waiting for stream event")
    {
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

#[tokio::test(flavor = "multi_thread")]
async fn configured_reader_records_read_error_while_active_queue_is_full() {
    let (stream_reader, mut rx, shared) = spawn_configured_reader(
        ReadErrorAfterBytes::new(b"aabbcc", io::ErrorKind::BrokenPipe),
        DeliveryGuarantee::BestEffort,
        2.bytes(),
        1,
    );

    let terminal =
        tokio::time::timeout(Duration::from_secs(1), wait_for_terminal_event(&shared))
            .await
            .expect("read error should be recorded even while live delivery is blocked");
    match terminal {
        StreamEvent::ReadError(err) => {
            assert_that!(err.stream_name()).is_equal_to("custom");
            assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
        }
    }
    assert_that!(stream_reader.is_finished()).is_false();
    drop(shared);

    match rx.recv().await.unwrap() {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(b"aa".as_slice());
        }
        other => {
            assert_that!(&other).fail(format_args!("expected buffered chunk, got {other:?}"));
        }
    }
    assert_that!(rx.recv().await.unwrap()).is_equal_to(StreamEvent::Gap);
    match rx.recv().await.unwrap() {
        StreamEvent::ReadError(err) => {
            assert_that!(err.stream_name()).is_equal_to("custom");
            assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
        }
    }

    stream_reader.await.unwrap();
    assert_that!(
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for stream closure")
    )
    .is_none();
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
#[traced_test]
async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
            .max_buffered_chunks(2)
            .build(),
    );

    let inspector = os.inspect_lines_async(
        |_line| async move {
            sleep(Duration::from_millis(100)).await;
            Next::Continue
        },
        LineParsingOptions::default(),
    );

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
