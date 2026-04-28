use super::super::SingleSubscriberOutputStream;
use super::common::{
    best_effort_no_replay_options, best_effort_no_replay_options_with, reliable_replay_options,
};
use crate::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, Next,
    RawCollectionOptions, ReplayRetention, StreamConsumerError,
};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing_test::traced_test;

#[tokio::test]
async fn configured_second_subscriber_error_does_not_poison_state_or_stop_reader() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        reliable_replay_options(ReplayRetention::All),
    );

    let collector = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .unwrap();

    let err = stream
        .wait_for_line(
            Duration::from_millis(25),
            |_line| false,
            LineParsingOptions::default(),
        )
        .err()
        .expect("second consumer should be rejected");
    assert_that!(err).is_equal_to(StreamConsumerError::ActiveConsumer {
        stream_name: "custom",
    });

    assert_that!(stream.is_replay_sealed()).is_false();

    write_half.write_all(b"live").await.unwrap();
    write_half.flush().await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"live".to_vec());
}

#[tokio::test]
async fn wait_for_line_claims_receiver_before_polling() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let _waiter = stream
        .wait_for_line(
            Duration::from_secs(1),
            |_line| false,
            LineParsingOptions::default(),
        )
        .unwrap();

    let err = stream
        .wait_for_line(
            Duration::from_secs(1),
            |_line| false,
            LineParsingOptions::default(),
        )
        .err()
        .expect("second consumer should be rejected");
    assert_that!(err).is_equal_to(StreamConsumerError::ActiveConsumer {
        stream_name: "custom",
    });
}

#[tokio::test]
#[traced_test]
async fn multiple_subscribers_are_not_possible() {
    let (read_half, _write_half) = tokio::io::duplex(64);
    let os = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options_with(DEFAULT_READ_CHUNK_SIZE, DEFAULT_MAX_BUFFERED_CHUNKS),
    );

    let _inspector = os
        .inspect_lines(|_line| Next::Continue, LineParsingOptions::default())
        .unwrap();

    let err = os
        .inspect_lines(|_line| Next::Continue, LineParsingOptions::default())
        .err()
        .expect("second consumer should be rejected");
    assert_that!(err).is_equal_to(StreamConsumerError::ActiveConsumer {
        stream_name: "custom",
    });
}
