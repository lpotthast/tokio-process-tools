use super::state::ConfiguredShared;
use crate::output_stream::{Chunk, DeliveryGuarantee, ReplayRetention, StreamEvent};
use crate::{NumBytes, StreamReadError};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

fn store_or_deliver_live(
    shared: &ConfiguredShared,
    event: StreamEvent,
    replay_retention: Option<ReplayRetention>,
) -> Option<StreamEvent> {
    let mut state = shared
        .state
        .lock()
        .expect("single-subscriber state poisoned");
    if state.consumer_attached {
        Some(event)
    } else {
        state.push_pre_consumer(event, replay_retention);
        None
    }
}

async fn send_live_event(event: StreamEvent, sender: &mpsc::Sender<StreamEvent>) -> Result<(), ()> {
    sender.send(event).await.map_err(|_err| ())
}

fn try_send_live_event(
    event: StreamEvent,
    sender: &mpsc::Sender<StreamEvent>,
) -> Result<(), TrySendError<StreamEvent>> {
    sender.try_send(event)
}

fn log_if_lagged(lagged: &mut usize) {
    if *lagged > 0 {
        tracing::debug!(lagged = *lagged, "Stream reader is lagging behind");
        *lagged = 0;
    }
}

fn buffered_chunk_count(buffer_len: usize, chunk_size: NumBytes) -> usize {
    buffer_len.div_ceil(chunk_size.bytes())
}

pub(super) async fn read_chunked<R: AsyncRead + Unpin + Send + 'static>(
    mut read: R,
    shared: Arc<ConfiguredShared>,
    sender: mpsc::Sender<StreamEvent>,
    chunk_size: NumBytes,
    delivery_guarantee: DeliveryGuarantee,
    replay_retention: Option<ReplayRetention>,
    stream_name: &'static str,
) {
    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    let mut lagged = 0_usize;
    let mut gap_pending = false;

    let send_or_store =
        |event: StreamEvent| store_or_deliver_live(&shared, event, replay_retention);

    'outer: loop {
        let _ = buf.try_reclaim(chunk_size.bytes());
        match read.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                let is_eof = bytes_read == 0;

                if is_eof {
                    if gap_pending
                        && let Some(event) = send_or_store(StreamEvent::Gap)
                        && send_live_event(event, &sender).await.is_err()
                    {
                        break 'outer;
                    }
                    if let Some(event) = send_or_store(StreamEvent::Eof)
                        && send_live_event(event, &sender).await.is_err()
                    {
                        break 'outer;
                    }
                    break;
                }

                while !buf.is_empty() {
                    match delivery_guarantee {
                        DeliveryGuarantee::ReliableForActiveSubscribers => {
                            let split_to = usize::min(chunk_size.bytes(), buf.len());
                            let chunk = Chunk(buf.split_to(split_to).freeze());
                            let event = StreamEvent::Chunk(chunk);

                            let Some(event) = send_or_store(event) else {
                                continue;
                            };

                            if send_live_event(event, &sender).await.is_err() {
                                break 'outer;
                            }
                        }
                        DeliveryGuarantee::BestEffort => {
                            if gap_pending {
                                match try_send_live_event(StreamEvent::Gap, &sender) {
                                    Ok(()) => {
                                        log_if_lagged(&mut lagged);
                                        gap_pending = false;
                                    }
                                    Err(TrySendError::Full(_event)) => {
                                        lagged += buffered_chunk_count(buf.len(), chunk_size);
                                        buf.clear();
                                        tokio::task::yield_now().await;
                                        continue;
                                    }
                                    Err(TrySendError::Closed(_event)) => break 'outer,
                                }
                            }

                            let split_to = usize::min(chunk_size.bytes(), buf.len());
                            let chunk = Chunk(buf.split_to(split_to).freeze());
                            let event = StreamEvent::Chunk(chunk);

                            let Some(event) = send_or_store(event) else {
                                continue;
                            };

                            match try_send_live_event(event, &sender) {
                                Ok(()) => {
                                    log_if_lagged(&mut lagged);
                                }
                                Err(TrySendError::Full(_event)) => {
                                    lagged += 1;
                                    gap_pending = true;
                                    tokio::task::yield_now().await;
                                }
                                Err(TrySendError::Closed(_event)) => break 'outer,
                            }
                        }
                    }
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                if let Some(event) = send_or_store(StreamEvent::ReadError(err)) {
                    let _ignored = send_live_event(event, &sender).await;
                }
                break;
            }
        }
    }
}
