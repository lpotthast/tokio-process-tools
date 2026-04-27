use super::FastClosureState;
use super::state::{Shared, append_event};
use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::{Delivery, Replay};
use crate::{NumBytes, StreamReadError};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::broadcast;

pub(super) async fn read_chunked_shared<S, D, R>(
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

pub(super) async fn read_chunked_fast<S: AsyncRead + Unpin + Send + 'static>(
    mut read: S,
    chunk_size: NumBytes,
    sender: broadcast::Sender<StreamEvent>,
    closure_state: Arc<Mutex<FastClosureState>>,
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
    use super::super::FastClosureState;
    use super::super::state::Shared;
    use super::*;
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

    fn assert_chunk(event: &StreamEvent, expected: &'static [u8]) {
        match event {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(expected);
            }
            other => {
                assert_that!(other).fail(format_args!("expected chunk, got {other:?}"));
            }
        }
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
        assert_chunk(&state.replay[0].event, b"ab");
        assert_chunk(&state.replay[1].event, b"cd");
        assert_chunk(&state.replay[2].event, b"ef");
        assert_that!(state.terminal.as_ref().map(|event| &event.event))
            .is_some()
            .is_equal_to(&StreamEvent::Eof);
    }

    #[tokio::test]
    async fn fast_reader_splits_input_into_configured_chunks_and_eof() {
        let (sender, mut receiver) = broadcast::channel(8);
        let closure_state = Arc::new(Mutex::new(FastClosureState {
            closed: false,
            read_error: None,
        }));

        read_chunked_fast(
            Cursor::new(b"abcdef".to_vec()),
            2.bytes(),
            sender,
            Arc::clone(&closure_state),
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
