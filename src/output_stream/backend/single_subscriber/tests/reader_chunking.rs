use super::reader_test_support::spawn_configured_reader;
use crate::NumBytesExt;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::DeliveryGuarantee;
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tracing_test::traced_test;

#[tokio::test]
#[traced_test]
async fn configured_reader_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
{
    let (read_half, mut write_half) = tokio::io::duplex(64);

    // Write more data than the configured chunk size so the initial read can fill the buffer.
    write_half.write_all(b"hello world").await.unwrap();
    write_half.flush().await.unwrap();

    let (stream_reader, mut rx, _shared) =
        spawn_configured_reader(read_half, DeliveryGuarantee::BestEffort, 2.bytes(), 64);

    drop(write_half);
    tokio::time::timeout(Duration::from_secs(1), stream_reader)
        .await
        .expect("stream reader did not finish")
        .unwrap();

    let mut chunks = Vec::<String>::new();
    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Chunk(chunk) => {
                chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
            }
            StreamEvent::Gap => {}
            StreamEvent::Eof => break,
            StreamEvent::ReadError(err) => {
                assert_that!(&err).fail(format_args!("unexpected read error: {err}"));
            }
        }
    }
    assert_that!(chunks).contains_exactly(["he", "ll", "o ", "wo", "rl", "d"]);
}
