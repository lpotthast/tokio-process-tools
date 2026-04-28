//! Lossy fast path for the `BestEffortDelivery + NoReplay` configuration.
//!
//! This module exists purely as a performance specialization. It delegates straight to
//! `tokio::sync::broadcast`, so a single `send` fans out to every live receiver without a
//! mutex, a per-subscriber queue, or any replay bookkeeping — the cost the [`fanout`]
//! module pays to support reliable delivery and replay.
//!
//! In exchange it gives up replay and backpressure: subscribers that fall behind the
//! channel's bounded buffer observe `broadcast::error::RecvError::Lagged`, which
//! [`FastSubscription::recv`] surfaces as [`StreamEvent::Gap`]. Late subscribers see
//! only events still in the buffer plus the terminal event.
//!
//! This backend is selected by [`BroadcastOutputStream::from_stream`] in
//! [`super`] when the config is exactly `BestEffortDelivery + NoReplay`; every other
//! combination of [`Delivery`](crate::output_stream::policy::Delivery) and
//! [`Replay`](crate::output_stream::policy::Replay) routes to [`fanout`] instead.
//!
//! [`fanout`]: super::fanout
//! [`FastSubscription::recv`]: super::subscription::FastSubscription
//! [`BroadcastOutputStream::from_stream`]: super::BroadcastOutputStream::from_stream

use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::{BestEffortDelivery, NoReplay};
use crate::{NumBytes, StreamReadError};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

pub(super) struct FastClosureState {
    pub(super) closed: bool,
    pub(super) read_error: Option<StreamReadError>,
}

pub(super) struct FastBackend {
    pub(super) stream_reader: JoinHandle<()>,
    pub(super) sender: broadcast::Sender<StreamEvent>,
    pub(super) closure_state: Arc<Mutex<FastClosureState>>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) bytes_ingested_tx: watch::Sender<u64>,
    pub(super) options: StreamConfig<BestEffortDelivery, NoReplay>,
    pub(super) name: &'static str,
}

pub(super) fn new_fast_backend<S>(
    stream: S,
    stream_name: &'static str,
    read_chunk_size: NumBytes,
    max_buffered_chunks: usize,
) -> FastBackend
where
    S: AsyncRead + Unpin + Send + 'static,
{
    let (sender, receiver) = broadcast::channel::<StreamEvent>(max_buffered_chunks);
    drop(receiver);
    let closure_state = Arc::new(Mutex::new(FastClosureState {
        closed: false,
        read_error: None,
    }));
    let (bytes_ingested_tx, _) = watch::channel(0);
    let stream_reader = tokio::spawn(read_chunked_fast(
        stream,
        read_chunk_size,
        sender.clone(),
        Arc::clone(&closure_state),
        bytes_ingested_tx.clone(),
        stream_name,
    ));

    FastBackend {
        stream_reader,
        sender,
        closure_state,
        bytes_ingested_tx,
        options: StreamConfig {
            read_chunk_size,
            max_buffered_chunks,
            delivery: BestEffortDelivery,
            replay: NoReplay,
        },
        name: stream_name,
    }
}

async fn read_chunked_fast<S: AsyncRead + Unpin + Send + 'static>(
    mut read: S,
    chunk_size: NumBytes,
    sender: broadcast::Sender<StreamEvent>,
    closure_state: Arc<Mutex<FastClosureState>>,
    bytes_ingested_tx: watch::Sender<u64>,
    stream_name: &'static str,
) {
    let send_event = move |event: StreamEvent| {
        if let Err(err) = sender.send(event) {
            tracing::debug!(
                error = %err,
                "No active receivers for the output event, dropping it"
            );
        }
    };

    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    loop {
        let _ = buf.try_reclaim(chunk_size.bytes());
        match read.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                if bytes_read > 0 {
                    bytes_ingested_tx
                        .send_modify(|n| *n = n.saturating_add(bytes_read as u64));
                }
                let is_eof = bytes_read == 0;

                if is_eof {
                    let mut state = closure_state.lock().expect("closure_state poisoned");
                    state.closed = true;
                    send_event(StreamEvent::Eof);
                    break;
                }

                while !buf.is_empty() {
                    let split_to = usize::min(chunk_size.bytes(), buf.len());
                    send_event(StreamEvent::Chunk(Chunk(buf.split_to(split_to).freeze())));
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                {
                    let mut state = closure_state.lock().expect("closure_state poisoned");
                    state.read_error = Some(err.clone());
                }
                send_event(StreamEvent::ReadError(err));
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumBytesExt;
    use crate::output_stream::backend::test_support::assert_chunk;
    use assertr::prelude::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn fast_reader_splits_input_into_configured_chunks_and_eof() {
        let (sender, mut receiver) = broadcast::channel(8);
        let closure_state = Arc::new(Mutex::new(FastClosureState {
            closed: false,
            read_error: None,
        }));

        let (bytes_ingested_tx, _) = watch::channel(0);
        read_chunked_fast(
            Cursor::new(b"abcdef".to_vec()),
            2.bytes(),
            sender,
            Arc::clone(&closure_state),
            bytes_ingested_tx,
            "custom",
        )
        .await;

        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        let third = receiver.recv().await.unwrap();
        let eof = receiver.recv().await.unwrap();

        assert_chunk(&first, b"ab");
        assert_chunk(&second, b"cd");
        assert_chunk(&third, b"ef");
        assert_that!(eof).is_equal_to(StreamEvent::Eof);
        assert_that!(closure_state.lock().expect("closure_state poisoned").closed).is_true();
    }
}
