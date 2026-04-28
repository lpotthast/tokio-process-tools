use super::common::wait_for_terminal_shared;
use super::reader_test_support::spawn_configured_reader;
use crate::NumBytesExt;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::DeliveryGuarantee;
use crate::test_support::ReadErrorAfterBytes;
use assertr::prelude::*;
use std::io;
use std::time::Duration;

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

    let terminal = tokio::time::timeout(Duration::from_secs(1), wait_for_terminal_shared(&shared))
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
