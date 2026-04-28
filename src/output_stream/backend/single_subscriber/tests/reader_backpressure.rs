use super::super::SingleSubscriberOutputStream;
use super::common::wait_for_terminal_shared;
use super::reader_test_support::{recv_event_with_timeout, spawn_configured_reader};
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::DeliveryGuarantee;
use crate::{DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, Next, NumBytesExt, StreamConfig};
use assertr::prelude::*;
use std::io::{self, Cursor};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tokio::time::sleep;
use tracing_test::traced_test;

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

    let terminal = tokio::time::timeout(Duration::from_secs(1), wait_for_terminal_shared(&shared))
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

    let inspector = os
        .inspect_lines_async(
            |_line| async move {
                sleep(Duration::from_millis(100)).await;
                Next::Continue
            },
            LineParsingOptions::default(),
        )
        .unwrap();

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
