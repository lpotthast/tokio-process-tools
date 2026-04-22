use super::FastClosureState;
use super::state::{Shared, append_event};
use crate::output_stream::{Chunk, Delivery, Replay, StreamConfig, StreamEvent};
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
                shared.reader_available.notify_waiters();
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
