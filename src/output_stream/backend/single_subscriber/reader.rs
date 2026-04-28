use super::state::{ActiveSubscriber, ConfiguredShared, SubscriberId};
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::ReplayRetention;
use crate::{NumBytes, StreamReadError};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::watch;

fn record_terminal_event(shared: &ConfiguredShared, event: StreamEvent) {
    {
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.push_replay_event(event.clone(), None);
    }
    shared.terminal_tx.send_replace(Some(event));
}

fn record_replay_chunk(
    shared: &ConfiguredShared,
    event: &StreamEvent,
    replay_retention: Option<ReplayRetention>,
) {
    if replay_retention.is_none() {
        return;
    }

    let mut state = shared
        .state
        .lock()
        .expect("single-subscriber state poisoned");
    state.push_replay_event(event.clone(), replay_retention);
}

fn refresh_cached_active(
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
) {
    cached_active.clone_from(&active_rx.borrow_and_update());
}

async fn send_reliable_event(
    mut event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
) {
    match &event {
        StreamEvent::Chunk(_) => record_replay_chunk(shared, &event, replay_retention),
        StreamEvent::Eof | StreamEvent::ReadError(_) => {
            record_terminal_event(shared, event.clone());
        }
        StreamEvent::Gap => {}
    }

    let mut retried = false;
    loop {
        let Some(active) = cached_active.clone() else {
            return;
        };

        match active.sender.send(event).await {
            Ok(()) => return,
            Err(SendError(returned)) => {
                shared.clear_active_if_current(active.id);
                refresh_cached_active(cached_active, active_rx);
                event = returned;
                if retried {
                    return;
                }
                retried = true;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BestEffortSend {
    Delivered,
    Full,
    Inactive,
}

fn try_send_best_effort_event(
    mut event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
    retry_on_closed: bool,
) -> BestEffortSend {
    if let StreamEvent::Chunk(_) = &event {
        record_replay_chunk(shared, &event, replay_retention);
    }

    let mut retried = false;
    loop {
        let Some(active) = cached_active.clone() else {
            return BestEffortSend::Inactive;
        };

        match active.sender.try_send(event) {
            Ok(()) => return BestEffortSend::Delivered,
            Err(TrySendError::Full(_returned)) => return BestEffortSend::Full,
            Err(TrySendError::Closed(returned)) => {
                shared.clear_active_if_current(active.id);
                refresh_cached_active(cached_active, active_rx);
                event = returned;
                if !retry_on_closed || retried {
                    return BestEffortSend::Inactive;
                }
                retried = true;
            }
        }
    }
}

#[derive(Debug, Default)]
struct BestEffortLag {
    subscriber_id: Option<SubscriberId>,
    lagged: usize,
    gap_pending: bool,
}

impl BestEffortLag {
    fn observe_active(&mut self, active: Option<&Arc<ActiveSubscriber>>) {
        let subscriber_id = active.map(|active| active.id);
        if self.subscriber_id != subscriber_id {
            self.subscriber_id = subscriber_id;
            self.lagged = 0;
            self.gap_pending = false;
        }
    }
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

async fn send_pending_gap_before_terminal(
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    lag: &mut BestEffortLag,
) {
    if !lag.gap_pending {
        return;
    }

    let Some(active) = cached_active.clone() else {
        lag.observe_active(None);
        return;
    };

    match active.sender.send(StreamEvent::Gap).await {
        Ok(()) => {
            log_if_lagged(&mut lag.lagged);
            lag.gap_pending = false;
        }
        Err(SendError(_event)) => {
            shared.clear_active_if_current(active.id);
            refresh_cached_active(cached_active, active_rx);
            lag.observe_active(cached_active.as_ref());
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the reader keeps the read/chunk loop inline to preserve delivery invariants"
)]
pub(super) async fn read_chunked_best_effort<R: AsyncRead + Unpin + Send + 'static>(
    mut read: R,
    shared: Arc<ConfiguredShared>,
    mut active_rx: watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    chunk_size: NumBytes,
    replay_retention: Option<ReplayRetention>,
    stream_name: &'static str,
) {
    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    let mut cached_active = active_rx.borrow().clone();
    let mut best_effort_lag = BestEffortLag::default();
    best_effort_lag.observe_active(cached_active.as_ref());

    // A cached active sender does not need to poll shared active state while its receiver is
    // alive: the mpsc sender remains valid until that consumer drops its receiver. A closed
    // channel is the signal to refresh the cached active slot. Subscriber ids prevent a stale
    // sender failure from clearing a newer active subscriber.
    'outer: loop {
        let _ = buf.try_reclaim(chunk_size.bytes());

        let read_result = if cached_active.is_none() {
            tokio::select! {
                result = read.read_buf(&mut buf) => {
                    refresh_cached_active(&mut cached_active, &mut active_rx);
                    best_effort_lag.observe_active(cached_active.as_ref());
                    result
                }
                changed = active_rx.changed() => {
                    if changed.is_ok() {
                        refresh_cached_active(&mut cached_active, &mut active_rx);
                        best_effort_lag.observe_active(cached_active.as_ref());
                        continue 'outer;
                    }
                    read.read_buf(&mut buf).await
                }
            }
        } else {
            read.read_buf(&mut buf).await
        };

        match read_result {
            Ok(bytes_read) => {
                shared.record_bytes_ingested(bytes_read);
                let is_eof = bytes_read == 0;

                if is_eof {
                    record_terminal_event(&shared, StreamEvent::Eof);
                    send_pending_gap_before_terminal(
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        &mut best_effort_lag,
                    )
                    .await;

                    send_reliable_event(
                        StreamEvent::Eof,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                    )
                    .await;
                    break;
                }

                while !buf.is_empty() {
                    best_effort_lag.observe_active(cached_active.as_ref());

                    if best_effort_lag.gap_pending {
                        match try_send_best_effort_event(
                            StreamEvent::Gap,
                            &shared,
                            &mut cached_active,
                            &mut active_rx,
                            replay_retention,
                            false,
                        ) {
                            BestEffortSend::Delivered => {
                                log_if_lagged(&mut best_effort_lag.lagged);
                                best_effort_lag.gap_pending = false;
                            }
                            BestEffortSend::Full => {
                                best_effort_lag.lagged +=
                                    buffered_chunk_count(buf.len(), chunk_size);
                                buf.clear();
                                tokio::task::yield_now().await;
                                continue;
                            }
                            BestEffortSend::Inactive => {
                                best_effort_lag.observe_active(cached_active.as_ref());
                            }
                        }
                    }

                    let split_to = usize::min(chunk_size.bytes(), buf.len());
                    let chunk = Chunk(buf.split_to(split_to).freeze());
                    let event = StreamEvent::Chunk(chunk);

                    match try_send_best_effort_event(
                        event,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                        true,
                    ) {
                        BestEffortSend::Delivered => {
                            log_if_lagged(&mut best_effort_lag.lagged);
                        }
                        BestEffortSend::Full => {
                            best_effort_lag.lagged += 1;
                            best_effort_lag.gap_pending = true;
                            tokio::task::yield_now().await;
                        }
                        BestEffortSend::Inactive => {
                            best_effort_lag.observe_active(cached_active.as_ref());
                        }
                    }
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                let event = StreamEvent::ReadError(err);
                record_terminal_event(&shared, event.clone());
                send_pending_gap_before_terminal(
                    &shared,
                    &mut cached_active,
                    &mut active_rx,
                    &mut best_effort_lag,
                )
                .await;
                send_reliable_event(
                    event,
                    &shared,
                    &mut cached_active,
                    &mut active_rx,
                    replay_retention,
                )
                .await;
                break;
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the reader keeps the read/chunk loop inline to preserve delivery invariants"
)]
pub(super) async fn read_chunked_reliable<R: AsyncRead + Unpin + Send + 'static>(
    mut read: R,
    shared: Arc<ConfiguredShared>,
    mut active_rx: watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    chunk_size: NumBytes,
    replay_retention: Option<ReplayRetention>,
    stream_name: &'static str,
) {
    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    let mut cached_active = active_rx.borrow().clone();

    // A cached active sender does not need to poll shared active state while its receiver is
    // alive: the mpsc sender remains valid until that consumer drops its receiver. A closed
    // channel is the signal to refresh the cached active slot. Subscriber ids prevent a stale
    // sender failure from clearing a newer active subscriber.
    'outer: loop {
        let _ = buf.try_reclaim(chunk_size.bytes());

        let read_result = if cached_active.is_none() {
            tokio::select! {
                result = read.read_buf(&mut buf) => {
                    refresh_cached_active(&mut cached_active, &mut active_rx);
                    result
                }
                changed = active_rx.changed() => {
                    if changed.is_ok() {
                        refresh_cached_active(&mut cached_active, &mut active_rx);
                        continue 'outer;
                    }
                    read.read_buf(&mut buf).await
                }
            }
        } else {
            read.read_buf(&mut buf).await
        };

        match read_result {
            Ok(bytes_read) => {
                shared.record_bytes_ingested(bytes_read);
                let is_eof = bytes_read == 0;

                if is_eof {
                    send_reliable_event(
                        StreamEvent::Eof,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                    )
                    .await;
                    break;
                }

                while !buf.is_empty() {
                    let split_to = usize::min(chunk_size.bytes(), buf.len());
                    let chunk = Chunk(buf.split_to(split_to).freeze());
                    let event = StreamEvent::Chunk(chunk);

                    send_reliable_event(
                        event,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                    )
                    .await;
                }
            }
            Err(err) => {
                let err = StreamReadError::new(stream_name, err);
                tracing::warn!(error = %err, "Could not read from stream");
                send_reliable_event(
                    StreamEvent::ReadError(err),
                    &shared,
                    &mut cached_active,
                    &mut active_rx,
                    replay_retention,
                )
                .await;
                break;
            }
        }
    }
}
