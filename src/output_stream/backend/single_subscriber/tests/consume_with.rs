use super::super::SingleSubscriberOutputStream;
use super::common::best_effort_no_replay_options;
use crate::output_stream::event::Chunk;
use crate::{AsyncStreamVisitor, Next, StreamVisitor};
use assertr::prelude::*;
use tokio::io::AsyncWriteExt;

/// A synchronous visitor that counts the number of chunks it observed and breaks on the first one.
struct CountChunks {
    count: usize,
}

impl StreamVisitor for CountChunks {
    type Output = usize;

    fn on_chunk(&mut self, _chunk: Chunk) -> Next {
        self.count += 1;
        Next::Break
    }

    fn into_output(self) -> usize {
        self.count
    }
}

#[tokio::test]
async fn consume_with_runs_a_custom_sync_visitor_until_break() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let consumer = stream.consume_with(CountChunks { count: 0 }).unwrap();
    write_half.write_all(b"first").await.unwrap();

    // The visitor returns `Next::Break` on the first chunk, so the consumer must terminate
    // without waiting for EOF. We deliberately keep `write_half` alive while waiting to
    // distinguish "broke early" from "saw EOF."
    let observed = consumer.wait().await.unwrap();
    assert_that!(observed).is_equal_to(1);
    drop(write_half);
}

/// An asynchronous visitor that forwards every observed chunk to an mpsc channel.
struct ForwardChunksAsync {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

impl AsyncStreamVisitor for ForwardChunksAsync {
    type Output = ();

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        match self.tx.send(chunk.as_ref().to_vec()).await {
            Ok(()) => Next::Continue,
            Err(_) => Next::Break,
        }
    }

    fn into_output(self) {}
}

#[tokio::test]
async fn consume_with_async_runs_a_custom_async_visitor_to_eof() {
    let (read_half, mut write_half) = tokio::io::duplex(64);
    let stream = SingleSubscriberOutputStream::from_stream(
        read_half,
        "custom",
        best_effort_no_replay_options(),
    );

    let (forwarded_tx, mut forwarded_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let consumer = stream
        .consume_with_async(ForwardChunksAsync { tx: forwarded_tx })
        .unwrap();

    write_half.write_all(b"alpha").await.unwrap();
    write_half.write_all(b"beta").await.unwrap();
    drop(write_half);

    consumer.wait().await.unwrap();

    // Chunk boundaries are not preserved through `tokio::io::duplex` — the bytes may arrive in
    // one or several chunks — but the concatenation matches the bytes we wrote.
    let mut all_bytes = Vec::new();
    while let Some(bytes) = forwarded_rx.recv().await {
        all_bytes.extend_from_slice(&bytes);
    }
    assert_that!(all_bytes).is_equal_to(b"alphabeta".to_vec());
}
