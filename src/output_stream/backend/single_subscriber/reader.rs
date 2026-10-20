use super::state::{ActiveSubscriber, ConfiguredShared, SubscriberId};
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::policy::ReplayRetention;
use crate::{NumBytes, StreamReadError};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::watch;

/// Records `event` for subscribers attaching later and reads the active subscriber into
/// `cached_active`, both under the state lock a subscriber attaches under (`try_subscribe`). A
/// subscriber attaching concurrently thereby either finds the event in its replay snapshot or is
/// the active one to send it to, never neither nor both. Returns whether subscribers attaching
/// later receive the event.
fn record_event(
    event: &StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
) -> bool {
    let recorded = {
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        let recorded = state.push_replay_event(event.clone(), replay_retention);
        refresh_cached_active(cached_active, active_rx);
        recorded
    };
    if let StreamEvent::Eof | StreamEvent::ReadError(_) = event {
        shared.terminal_tx.send_replace(Some(event.clone()));
    }
    recorded
}

fn refresh_cached_active(
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
) {
    cached_active.clone_from(&active_rx.borrow_and_update());
}

async fn send_reliable_event(
    event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
) {
    let recorded = record_event(&event, shared, cached_active, active_rx, replay_retention);
    deliver_reliable_event(event, recorded, shared, cached_active, active_rx).await;
}

/// Sends `event` to the active subscriber, waiting for capacity. `recorded` is the result of
/// recording the event with [`record_event`].
async fn deliver_reliable_event(
    mut event: StreamEvent,
    recorded: bool,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
) {
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
                // A subscriber attaching from now on finds a recorded event in its replay, unless
                // replay gets sealed first, which deliberately hides output read before the seal.
                // Either way, sending it the event live would be wrong.
                if recorded || retried {
                    return;
                }
                event = returned;
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
    let recorded = match &event {
        StreamEvent::Chunk(_) => {
            record_event(&event, shared, cached_active, active_rx, replay_retention)
        }
        // A gap belongs to the subscriber that lagged, so it is not sent to a newly active one.
        StreamEvent::Gap => false,
        StreamEvent::Eof | StreamEvent::ReadError(_) => {
            unreachable!("terminal events are sent with `send_terminal_event_best_effort`")
        }
    };

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
                // See `send_reliable_event`.
                if recorded || !retry_on_closed || retried {
                    return BestEffortSend::Inactive;
                }
                event = returned;
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

/// Records the terminal `event`, sends a gap still pending for the active subscriber, and then
/// sends the event itself.
async fn send_terminal_event_best_effort(
    event: StreamEvent,
    shared: &ConfiguredShared,
    cached_active: &mut Option<Arc<ActiveSubscriber>>,
    active_rx: &mut watch::Receiver<Option<Arc<ActiveSubscriber>>>,
    replay_retention: Option<ReplayRetention>,
    lag: &mut BestEffortLag,
) {
    let recorded = record_event(&event, shared, cached_active, active_rx, replay_retention);
    lag.observe_active(cached_active.as_ref());
    send_pending_gap_before_terminal(shared, cached_active, active_rx, lag).await;
    deliver_reliable_event(event, recorded, shared, cached_active, active_rx).await;
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

    // The cached active subscriber is refreshed when one attaches while there is none, under the
    // state lock whenever an event is recorded (see `record_event`), and after a send fails
    // because the subscriber dropped its receiver. Subscriber ids prevent a stale sender failure
    // from clearing a newer active subscriber.
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
                    send_terminal_event_best_effort(
                        StreamEvent::Eof,
                        &shared,
                        &mut cached_active,
                        &mut active_rx,
                        replay_retention,
                        &mut best_effort_lag,
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
                send_terminal_event_best_effort(
                    StreamEvent::ReadError(err),
                    &shared,
                    &mut cached_active,
                    &mut active_rx,
                    replay_retention,
                    &mut best_effort_lag,
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

    // The cached active subscriber is refreshed when one attaches while there is none, under the
    // state lock whenever an event is recorded (see `record_event`), and after a send fails
    // because the subscriber dropped its receiver. Subscriber ids prevent a stale sender failure
    // from clearing a newer active subscriber.
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
