use super::super::SingleSubscriberOutputStream;
use super::common::wait_for_no_active_consumer;
use crate::{NumBytesExt, RawCollectionOptions, StreamConfig};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

#[tokio::test]
async fn reader_drains_after_consumer_drop() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(16.bytes())
            .max_buffered_chunks(1)
            .build(),
    );

    let collector = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .unwrap();
    drop(collector);
    wait_for_no_active_consumer(&stream).await;

    let idle_output = vec![b'x'; 4096];
    tokio::time::timeout(Duration::from_secs(1), write_half.write_all(&idle_output))
        .await
        .expect("reader should keep draining with no active consumer")
        .unwrap();
    sleep(Duration::from_millis(25)).await;

    let collector = stream
        .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
        .unwrap();
    write_half.write_all(b"tail").await.unwrap();
    drop(write_half);

    let bytes = collector.wait().await.unwrap();
    assert_that!(bytes.bytes).is_equal_to(b"tail".to_vec());
}
