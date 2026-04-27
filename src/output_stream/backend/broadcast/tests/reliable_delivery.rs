use super::super::BroadcastOutputStream;
use super::common::{
    CountingWrite, GatedChunkedReader, build_chunk_payload, line_collection_options,
    reliable_no_replay_options_with,
};
use crate::{LineParsingOptions, NumBytesExt, WriteCollectionOptions};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn block_until_subscribers_catch_up_preserves_all_output_for_active_subscribers() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = BroadcastOutputStream::from_stream(
        read_half,
        "custom",
        reliable_no_replay_options_with(2.bytes(), 1),
    );
    let collector =
        stream.collect_lines_into_vec(LineParsingOptions::default(), line_collection_options());

    write_half.write_all(b"a\nb\nc\n").await.unwrap();
    drop(write_half);

    let collected = collector.wait().await.unwrap();
    assert_that!(collected.lines().iter().map(String::as_str)).contains_exactly(["a", "b", "c"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn block_until_subscribers_catch_up_gated_multi_subscriber_collection_completes() {
    let total_bytes = 512 * 1024;
    let stream_chunk_size = 16 * 1024;
    let subscriber_count = 2;
    let chunks = build_chunk_payload(total_bytes, stream_chunk_size);
    let (reader, gate) = GatedChunkedReader::new(&chunks);
    let stream = BroadcastOutputStream::from_stream(
        reader,
        "custom",
        reliable_no_replay_options_with(stream_chunk_size.bytes(), 256),
    );
    let collectors = (0..subscriber_count)
        .map(|_| {
            stream.collect_chunks_into_write(
                CountingWrite::default(),
                WriteCollectionOptions::fail_fast(),
            )
        })
        .collect::<Vec<_>>();

    gate.open();

    let result = tokio::time::timeout(Duration::from_secs(1), async {
        let mut bytes_written = 0;
        for collector in collectors {
            bytes_written += collector.wait().await.unwrap().bytes_written;
        }
        bytes_written
    })
    .await;

    assert_that!(result)
        .is_ok()
        .is_equal_to(total_bytes * subscriber_count);
}
