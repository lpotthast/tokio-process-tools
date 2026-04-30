//! General-purpose `<D: Delivery, R: Replay>` broadcast path used for every configuration
//! the [`fast`] module does not handle.
//!
//! Events flow through an `Arc<Shared>` (see [`state`]) that owns the replay `VecDeque`,
//! the subscriber registry (`HashMap<SubscriberId, SubscriberSender>`), the terminal-event
//! slot, and the replay-sealing flag. Per-event dispatch happens in
//! [`state::append_event`], which branches on the configured
//! [`DeliveryGuarantee`](crate::output_stream::policy::DeliveryGuarantee): `ReliableForActiveSubscribers`
//! pushes through a per-subscriber `mpsc::Sender` and applies backpressure when active
//! consumers fall behind, while `BestEffort` pushes through a `BestEffortLiveQueue` that
//! drops on overflow.
//!
//! Compared with [`fast`], every event takes a mutex on the shared state and the replay
//! buffer is maintained on the hot path. That overhead is the price of supporting late
//! subscribers (via replay retention), reliable delivery, or both. This backend is
//! selected by [`BroadcastOutputStream::from_stream`] in [`super`] for any
//! `ReliableDelivery` config or any `ReplayEnabled` config.
//!
//! [`fast`]: super::fast
//! [`state`]: super::state
//! [`state::append_event`]: super::state
//! [`BroadcastOutputStream::from_stream`]: super::BroadcastOutputStream::from_stream

use super::state::{Shared, append_event};
use crate::StreamReadError;
use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::{Delivery, Replay};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::task::JoinHandle;

pub(super) struct FanoutReplayBackend<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) stream_reader: JoinHandle<()>,
    pub(super) shared: Arc<Shared>,
    pub(super) options: StreamConfig<D, R>,
    pub(super) name: &'static str,
}

pub(super) fn new_fanout_backend<S, D, R>(
    stream: S,
    stream_name: &'static str,
    options: StreamConfig<D, R>,
) -> FanoutReplayBackend<D, R>
where
    S: AsyncRead + Unpin + Send + 'static,
    D: Delivery,
    R: Replay,
{
    let shared = Arc::new(Shared::new());
    let stream_reader = tokio::spawn(read_chunked_shared(
        stream,
        Arc::clone(&shared),
        options,
        stream_name,
    ));

    FanoutReplayBackend {
        stream_reader,
        shared,
        options,
        name: stream_name,
    }
}

async fn read_chunked_shared<S, D, R>(
    mut read: S,
    shared: Arc<Shared>,
    options: StreamConfig<D, R>,
    stream_name: &'static str,
) where
    S: AsyncRead + Unpin + Send + 'static,
    D: Delivery,
    R: Replay,
{
    let read_chunk_size = options.read_chunk_size;
    let mut buf = bytes::BytesMut::with_capacity(read_chunk_size.bytes());

    loop {
        let _ = buf.try_reclaim(read_chunk_size.bytes());
        match read.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                shared.record_bytes_ingested(bytes_read);
                let is_eof = bytes_read == 0;

                if is_eof {
                    append_event(&shared, options, StreamEvent::Eof).await;
                    break;
                }

                while !buf.is_empty() {
                    let split_to = usize::min(read_chunk_size.bytes(), buf.len());
                    let event = StreamEvent::Chunk(Chunk(buf.split_to(split_to).freeze()));
                    append_event(&shared, options, event).await;
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                append_event(&shared, options, StreamEvent::ReadError(err)).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::event::tests::StreamEventAssertions;
    use crate::{NumBytesExt, StreamConfig};
    use assertr::prelude::*;
    use std::io::Cursor;

    fn replay_all_options() -> StreamConfig<crate::BestEffortDelivery, crate::ReplayEnabled> {
        StreamConfig::builder()
            .best_effort_delivery()
            .replay_all()
            .read_chunk_size(2.bytes())
            .max_buffered_chunks(8)
            .build()
    }

    #[tokio::test]
    async fn fanout_reader_splits_input_into_configured_chunks_and_eof() {
        let shared = Arc::new(Shared::new());
        let options = replay_all_options();

        read_chunked_shared(
            Cursor::new(b"abcdef".to_vec()),
            Arc::clone(&shared),
            options,
            "custom",
        )
        .await;

        let state = shared.state.lock().expect("broadcast state poisoned");
        assert_that!(state.replay.len()).is_equal_to(3);
        assert_that!(&state.replay[0].event)
            .is_chunk()
            .is_equal_to(b"ab");
        assert_that!(&state.replay[1].event)
            .is_chunk()
            .is_equal_to(b"cd");
        assert_that!(&state.replay[2].event)
            .is_chunk()
            .is_equal_to(b"ef");
        assert_that!(state.terminal.as_ref().map(|event| &event.event))
            .is_some()
            .is_equal_to(&StreamEvent::Eof);
    }
}
