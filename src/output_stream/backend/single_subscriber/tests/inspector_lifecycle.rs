use super::super::SingleSubscriberOutputStream;
use super::common::{best_effort_no_replay_options, wait_for_no_active_consumer};
use crate::{InspectorCancelOutcome, LineParsingOptions, Next, RawCollectionOptions};
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
async fn wait_cancellation_releases_single_subscriber_claim() {
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
async fn cancel_waits_for_hanging_async_callback() {
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
async fn abort_releases_single_subscriber_claim() {
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
async fn cancel_or_abort_after_aborts_hanging_callback() {
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
