use super::super::SingleSubscriberOutputStream;
use super::common::{PendingWrite, best_effort_no_replay_options, wait_for_no_active_consumer};
use crate::{ConsumerCancelOutcome, RawCollectionOptions, WriteCollectionOptions};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

#[tokio::test]
async fn pending_writer_abort_releases_single_subscriber_claim() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (entered_tx, entered_rx) = oneshot::channel();
    let collector = stream
        .collect_chunks_into_write(
            PendingWrite::new(entered_tx),
            WriteCollectionOptions::fail_fast(),
        )
        .unwrap();

    write_half.write_all(b"ready").await.unwrap();
    entered_rx.await.unwrap();

    let outcome = collector.cancel(Duration::from_millis(25)).await.unwrap();

    assert_that!(matches!(outcome, ConsumerCancelOutcome::Aborted)).is_true();
    wait_for_no_active_consumer(&stream).await;

    let collector = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .unwrap();
    write_half.write_all(b"later").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"later".to_vec());
}
