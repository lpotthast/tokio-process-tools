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
async fn stream_drop_closes_waiting_line_waiters() {
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
