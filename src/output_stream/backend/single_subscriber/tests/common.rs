use super::super::SingleSubscriberOutputStream;
use super::super::state::ConfiguredShared;
use crate::AsyncChunkCollector;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::{Delivery, Replay};
use crate::{Next, NumBytes};
use crate::{NumBytesExt, ReplayRetention, StreamConfig};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWrite;
use tokio::sync::oneshot;

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

pub(super) async fn wait_for_no_active_consumer<D, R>(stream: &SingleSubscriberOutputStream<D, R>)
where
    D: Delivery,
    R: Replay,
{
    stream
        .configured_shared
        .subscribe_active()
        .wait_for(Option::is_none)
        .await
        .expect("active subscriber watch closed");
}

pub(super) async fn wait_for_bytes_ingested<D, R>(
    stream: &SingleSubscriberOutputStream<D, R>,
    bytes: u64,
) where
    D: Delivery,
    R: Replay,
{
    wait_for_bytes_ingested_shared(&stream.configured_shared, bytes).await;
}

pub(super) async fn wait_for_bytes_ingested_shared(shared: &ConfiguredShared, bytes: u64) {
    shared
        .subscribe_bytes_ingested()
        .wait_for(|observed| *observed >= bytes)
        .await
        .expect("bytes-ingested watch closed");
}

pub(super) async fn wait_for_terminal<D, R>(
    stream: &SingleSubscriberOutputStream<D, R>,
) -> StreamEvent
where
    D: Delivery,
    R: Replay,
{
    wait_for_terminal_shared(&stream.configured_shared).await
}

pub(super) async fn wait_for_terminal_shared(shared: &ConfiguredShared) -> StreamEvent {
    shared
        .subscribe_terminal()
        .wait_for(Option::is_some)
        .await
        .expect("terminal watch closed")
        .clone()
        .expect("terminal watch settled with None after wait_for is_some")
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
