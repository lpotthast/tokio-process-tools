use super::super::BroadcastOutputStream;
use super::common::{line_collection_options, reliable_options};
use crate::output_stream::event::StreamEvent;
use crate::{
    LineParsingOptions, Next, NumBytesExt, ReplayRetention, StreamConfig, WaitForLineResult,
};
use assertr::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

fn best_effort_replay_options(
    max_buffered_chunks: usize,
) -> StreamConfig<crate::BestEffortDelivery, crate::ReplayEnabled> {
    StreamConfig::builder()
        .best_effort_delivery()
        .replay_all()
        .read_chunk_size(1.bytes())
        .max_buffered_chunks(max_buffered_chunks)
        .build()
}

fn assert_chunk(event: Option<StreamEvent>, expected: &'static [u8]) {
    match event {
        Some(StreamEvent::Chunk(chunk)) => {
            assert_that!(chunk.as_ref()).is_equal_to(expected);
        }
        other => {
            assert_that!(&other).fail(format_args!("expected chunk, got {other:?}"));
        }
    }
}

#[tokio::test]
async fn late_subscriber_receives_startup_line_with_replay_all() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let result = stream
        .wait_for_line_with_timeout(
            |line| line == "ready",
            LineParsingOptions::default(),
            Duration::from_secs(1),
        )
        .await;

    assert_that!(result).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn seal_replay_drops_old_history_for_future_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "old",
        LineParsingOptions::default(),
        Duration::from_millis(50),
    );

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Timeout));
}

#[tokio::test]
async fn subscriber_created_after_seal_starts_live_by_default() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "live",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    write_half.write_all(b"live\n").await.unwrap();
    write_half.flush().await.unwrap();

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Matched));
}

#[tokio::test]
async fn explicit_replay_after_seal_ignores_history_retained_for_active_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );
    let _active = stream.subscribe();

    write_half.write_all(b"old\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let waiter = stream.wait_for_line_with_timeout(
        |line| line == "old",
        LineParsingOptions::default(),
        Duration::from_millis(50),
    );

    assert_that!(waiter.await).is_equal_to(Ok(WaitForLineResult::Timeout));
}

#[tokio::test]
async fn active_subscribers_still_receive_unread_tail_data_after_seal() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::All),
    );
    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

    write_half.write_all(b"tail\n").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;
    stream.seal_replay();

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["tail"]);
}

#[tokio::test]
async fn waiter_created_before_seal_can_match_replayed_startup_line() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_options(ReplayRetention::LastBytes(1.megabytes())),
    );
    let logs = Arc::new(Mutex::new(Vec::<String>::new()));
    let logs_in_task = Arc::clone(&logs);
    let _logger = stream.inspect_lines(
        move |line| {
            logs_in_task.lock().unwrap().push(line.into_owned());
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    write_half.write_all(b"ready\n").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let ready = stream.wait_for_line_with_timeout(
        |line| line == "ready",
        LineParsingOptions::default(),
        Duration::from_secs(1),
    );
    stream.seal_replay();

    assert_that!(ready.await).is_equal_to(Ok(WaitForLineResult::Matched));
    assert_that!(logs.lock().unwrap().clone()).contains_exactly(["ready"]);
}

#[tokio::test]
async fn slow_best_effort_replay_subscriber_observes_gap_then_newer_live_data() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_replay_options(2));
    let mut subscriber = stream.subscribe();

    write_half.write_all(b"abcde").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    drop(write_half);

    assert_that!(subscriber.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Gap);
    assert_chunk(subscriber.recv().await, b"e");
    assert_that!(subscriber.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Eof);
}

#[tokio::test]
async fn best_effort_replay_delivers_terminal_after_pending_gap() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_replay_options(1));
    let mut subscriber = stream.subscribe();

    write_half.write_all(b"ab").await.unwrap();
    drop(write_half);
    sleep(Duration::from_millis(20)).await;

    assert_that!(subscriber.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Gap);
    assert_that!(subscriber.recv().await)
        .is_some()
        .is_equal_to(StreamEvent::Eof);
}

#[tokio::test]
async fn late_best_effort_replay_subscriber_receives_retained_history_after_active_overflow() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream =
        BroadcastOutputStream::from_stream(read_half, "custom", best_effort_replay_options(1));
    let _slow_active_subscriber = stream.subscribe();

    write_half.write_all(b"abc").await.unwrap();
    write_half.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;

    let mut late_subscriber = stream.subscribe();
    assert_chunk(late_subscriber.recv().await, b"a");
    assert_chunk(late_subscriber.recv().await, b"b");
    assert_chunk(late_subscriber.recv().await, b"c");
}
