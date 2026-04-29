use super::super::SingleSubscriberOutputStream;
use super::common::{best_effort_no_replay_options, wait_for_no_active_consumer};
use crate::{LineParsingOptions, RawCollectionOptions, WaitForLineResult};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn timeout_allows_later_collector() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let result = stream
        .wait_for_line(
            Duration::from_millis(25),
            |line| line == "ready",
            LineParsingOptions::default(),
        )
        .unwrap()
        .await;
    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Timeout));
    wait_for_no_active_consumer(&stream).await;

    let collector = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .unwrap();
    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"ready\n".to_vec());
}

#[tokio::test]
async fn subscribes_before_polling() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    // Construct the waiter first; output written *before* the first poll must still be observed.
    let waiter = stream
        .wait_for_line(
            Duration::from_secs(1),
            |line| line == "ready",
            LineParsingOptions::default(),
        )
        .unwrap();
    write_half.write_all(b"ready\n").await.unwrap();
    drop(write_half);

    assert_that!(waiter.await)
        .is_ok()
        .is_equal_to(WaitForLineResult::Matched);
}

#[tokio::test]
async fn stream_drop_closes_waiting_line_waiters() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let waiter = stream
        .wait_for_line(
            Duration::from_secs(1),
            |line| line == "never",
            LineParsingOptions::default(),
        )
        .unwrap();
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}
