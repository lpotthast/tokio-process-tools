use super::super::BroadcastOutputStream;
use super::common::{best_effort_no_replay_options, reliable_options};
use crate::{LineParsingOptions, ReplayRetention, WaitForLineResult};
use assertr::prelude::*;
use std::time::Duration;

#[tokio::test]
async fn dropping_fast_stream_closes_waiting_subscribers() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_no_replay_options());

    let waiter = stream.wait_for_line(
        Duration::from_secs(1),
        |line| line == "never",
        LineParsingOptions::default(),
    );
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}

#[tokio::test]
async fn dropping_fanout_replay_stream_closes_waiting_subscribers() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    let waiter = stream.wait_for_line(
        Duration::from_secs(1),
        |line| line == "never",
        LineParsingOptions::default(),
    );
    drop(stream);

    let result = tokio::time::timeout(Duration::from_secs(1), waiter).await;
    assert_that!(result)
        .is_ok()
        .is_equal_to(Ok(WaitForLineResult::StreamClosed));
}
