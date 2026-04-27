use super::super::SingleSubscriberOutputStream;
use super::common::{
    HangingChunkCollector, best_effort_no_replay_options, wait_for_no_active_consumer,
};
use crate::{CollectorCancelOutcome, RawCollectionOptions};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

#[tokio::test]
async fn drop_allows_later_collector() {
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
async fn cancel_allows_later_collector() {
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
async fn wait_cancellation_releases_single_subscriber_claim() {
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

    let result = tokio::time::timeout(Duration::from_millis(25), collector.wait()).await;
    assert_that!(result.is_err()).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream.collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded);
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}

#[tokio::test]
async fn cancel_waits_for_hanging_async_collector() {
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
async fn abort_releases_single_subscriber_claim() {
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
async fn cancel_or_abort_after_aborts_hanging_async_collector() {
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
