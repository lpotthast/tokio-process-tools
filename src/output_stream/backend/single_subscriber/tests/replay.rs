use super::super::SingleSubscriberOutputStream;
use super::common::{
    best_effort_no_replay_options, reliable_replay_options, wait_for_no_active_consumer,
};
use crate::test_support::ReadErrorAfterBytes;
use crate::{
    CollectorError, LineParsingOptions, NumBytesExt, RawCollectionOptions, ReplayRetention,
    StreamConfig, WaitForLineResult,
};
use assertr::prelude::*;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

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
