use super::super::SingleSubscriberOutputStream;
use crate::AsyncChunkCollector;
use crate::output_stream::event::Chunk;
use crate::{Next, NumBytes};
use crate::{NumBytesExt, ReplayRetention, StreamConfig};
use assertr::prelude::*;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;
use tokio::time::sleep;

pub(super) const ACTIVE_CONSUMER_PANIC: &str = "Cannot create multiple active consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one active inspector, collector, or line waiter can be active at a time. Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.";

pub(super) fn best_effort_no_replay_options() -> StreamConfig {
    best_effort_no_replay_options_with(4.bytes(), 4)
}

pub(super) fn best_effort_no_replay_options_with(
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> StreamConfig {
    StreamConfig::builder()
        .best_effort_delivery()
        .no_replay()
        .read_chunk_size(read_chunk_size)
        .max_buffered_chunks(max_buffered_chunks)
        .build()
}

pub(super) fn reliable_replay_options(
    replay_retention: ReplayRetention,
) -> StreamConfig<crate::ReliableDelivery, crate::ReplayEnabled> {
    let builder = StreamConfig::builder().reliable_for_active_subscribers();
    match replay_retention {
        ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
        ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
        ReplayRetention::All => builder.replay_all(),
    }
    .read_chunk_size(4.bytes())
    .max_buffered_chunks(4)
    .build()
}

pub(super) async fn wait_for_no_active_consumer(stream: &SingleSubscriberOutputStream) {
    let shared = Arc::clone(stream.configured_shared.as_ref().unwrap());
    for _ in 0..50 {
        {
            let state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");
            if state.active_id.is_none() {
                return;
            }
        }
        sleep(Duration::from_millis(10)).await;
    }

    assert_that!(()).fail("active consumer did not detach");
}

pub(super) struct HangingChunkCollector {
    entered_tx: Option<oneshot::Sender<()>>,
}

impl HangingChunkCollector {
    pub(super) fn new(entered_tx: oneshot::Sender<()>) -> Self {
        Self {
            entered_tx: Some(entered_tx),
        }
    }
}

impl AsyncChunkCollector<Vec<u8>> for HangingChunkCollector {
    async fn collect<'a>(&'a mut self, _chunk: Chunk, _seen: &'a mut Vec<u8>) -> Next {
        if let Some(entered_tx) = self.entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        std::future::pending::<Next>().await
    }
}

pub(super) struct PendingWrite {
    entered_tx: Option<oneshot::Sender<()>>,
}

impl PendingWrite {
    pub(super) fn new(entered_tx: oneshot::Sender<()>) -> Self {
        Self {
            entered_tx: Some(entered_tx),
        }
    }
}

impl AsyncWrite for PendingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(entered_tx) = self.entered_tx.take() {
            entered_tx.send(()).unwrap();
        }
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
