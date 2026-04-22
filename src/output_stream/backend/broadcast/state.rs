use crate::StreamReadError;
use crate::output_stream::{
    Delivery, DeliveryGuarantee, Replay, ReplayRetention, StreamConfig, StreamEvent,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::Notify;

pub(super) type SubscriberId = u64;

#[derive(Debug, Clone)]
pub(super) struct SubscriberState {
    pub(super) cursor: u64,
}

#[derive(Debug)]
pub(super) struct BroadcastState {
    pub(super) events: VecDeque<StreamEvent>,
    pub(super) next_seq: u64,
    /// Oldest event still physically stored in the shared buffer.
    pub(super) buffer_start_seq: u64,
    pub(super) retained_chunk_count: usize,
    pub(super) retained_byte_count: usize,
    /// Oldest event still allowed for replay by future subscribers.
    pub(super) replay_start_seq: u64,
    replay_chunk_count: usize,
    pub(super) replay_byte_count: usize,
    next_subscriber_id: SubscriberId,
    pub(super) subscribers: HashMap<SubscriberId, SubscriberState>,
    pub(super) replay_sealed: bool,
    pub(super) eof: bool,
    pub(super) read_error: Option<StreamReadError>,
    pub(super) closed: bool,
}

impl BroadcastState {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            next_seq: 0,
            buffer_start_seq: 0,
            retained_chunk_count: 0,
            retained_byte_count: 0,
            replay_start_seq: 0,
            replay_chunk_count: 0,
            replay_byte_count: 0,
            next_subscriber_id: 0,
            subscribers: HashMap::new(),
            replay_sealed: false,
            eof: false,
            read_error: None,
            closed: false,
        }
    }

    fn push(&mut self, event: StreamEvent, retention: Option<ReplayRetention>) {
        let seq = self.next_seq;
        self.next_seq += 1;
        if let StreamEvent::Chunk(chunk) = &event {
            let chunk_len = chunk.as_ref().len();
            self.retained_chunk_count += 1;
            self.retained_byte_count += chunk_len;
            self.replay_chunk_count += 1;
            self.replay_byte_count += chunk_len;
        }
        match &event {
            StreamEvent::Eof => {
                self.eof = true;
            }
            StreamEvent::ReadError(err) => {
                self.read_error = Some(err.clone());
            }
            StreamEvent::Chunk(_) | StreamEvent::Gap => {}
        }
        self.events.push_back(event);
        if self.events.len() == 1 {
            self.buffer_start_seq = seq;
        }
        self.trim_replay_window(retention);
    }

    fn pop_front(&mut self) -> Option<StreamEvent> {
        let event = self.events.pop_front()?;
        if let StreamEvent::Chunk(chunk) = &event {
            self.retained_chunk_count -= 1;
            self.retained_byte_count -= chunk.as_ref().len();
        }

        if self.events.is_empty() {
            self.buffer_start_seq = self.next_seq;
        } else {
            self.buffer_start_seq += 1;
        }

        Some(event)
    }

    pub(super) fn event_at(&self, seq: u64) -> Option<StreamEvent> {
        let offset = usize::try_from(seq.checked_sub(self.buffer_start_seq)?).ok()?;
        self.events.get(offset).cloned()
    }

    fn replay_event_at(&self) -> Option<&StreamEvent> {
        let offset =
            usize::try_from(self.replay_start_seq.checked_sub(self.buffer_start_seq)?).ok()?;
        self.events.get(offset)
    }

    fn advance_replay_start(&mut self) {
        if self.replay_start_seq >= self.next_seq {
            return;
        }

        let replayed_chunk_len = match self.replay_event_at() {
            Some(StreamEvent::Chunk(chunk)) => Some(chunk.as_ref().len()),
            Some(StreamEvent::Gap | StreamEvent::Eof | StreamEvent::ReadError(_)) | None => None,
        };

        if let Some(chunk_len) = replayed_chunk_len {
            self.replay_chunk_count -= 1;
            self.replay_byte_count -= chunk_len;
        }
        self.replay_start_seq += 1;
    }

    pub(super) fn trim_replay_window(&mut self, retention: Option<ReplayRetention>) {
        self.replay_start_seq = self.replay_start_seq.max(self.buffer_start_seq);

        match retention {
            None | Some(ReplayRetention::LastChunks(0)) => {
                while self.replay_start_seq < self.next_seq {
                    self.advance_replay_start();
                }
            }
            Some(
                ReplayRetention::LastChunks(_)
                | ReplayRetention::LastBytes(_)
                | ReplayRetention::All,
            ) if self.replay_sealed => {
                while self.replay_start_seq < self.next_seq {
                    self.advance_replay_start();
                }
            }
            Some(ReplayRetention::LastBytes(bytes)) if bytes.bytes() == 0 => {
                while self.replay_start_seq < self.next_seq {
                    self.advance_replay_start();
                }
            }
            Some(ReplayRetention::LastChunks(chunks)) => {
                while self.replay_chunk_count > chunks {
                    self.advance_replay_start();
                }
            }
            Some(ReplayRetention::LastBytes(bytes)) => {
                while let Some(event) = self.replay_event_at() {
                    match event {
                        StreamEvent::Chunk(chunk)
                            if self.replay_byte_count.saturating_sub(chunk.as_ref().len())
                                >= bytes.bytes() =>
                        {
                            self.advance_replay_start();
                        }
                        StreamEvent::Chunk(_)
                        | StreamEvent::Gap
                        | StreamEvent::Eof
                        | StreamEvent::ReadError(_) => break,
                    }
                }
            }
            Some(ReplayRetention::All) => {}
        }
    }

    fn min_subscriber_cursor(&self) -> Option<u64> {
        self.subscribers
            .values()
            .map(|subscriber| subscriber.cursor)
            .min()
    }

    pub(super) fn add_subscriber(&mut self, cursor: u64) -> SubscriberId {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        self.subscribers.insert(id, SubscriberState { cursor });
        id
    }

    pub(super) fn remove_subscriber(&mut self, id: SubscriberId) {
        self.subscribers.remove(&id);
    }
}

#[derive(Debug)]
pub(super) struct Shared {
    pub(super) state: Mutex<BroadcastState>,
    pub(super) output_available: Notify,
    pub(super) reader_available: Notify,
}

impl Shared {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(BroadcastState::new()),
            output_available: Notify::new(),
            reader_available: Notify::new(),
        }
    }
}

pub(super) fn evict_locked<D, R>(state: &mut BroadcastState, options: StreamConfig<D, R>)
where
    D: Delivery,
    R: Replay,
{
    state.trim_replay_window(options.replay_retention());
    let active_floor = state.min_subscriber_cursor().unwrap_or(state.next_seq);
    let floor = match options.delivery_guarantee() {
        DeliveryGuarantee::ReliableForActiveSubscribers => active_floor,
        DeliveryGuarantee::BestEffort => {
            let capacity_floor = state
                .next_seq
                .saturating_sub(options.max_buffered_chunks as u64);
            active_floor.max(capacity_floor)
        }
    };

    while state.buffer_start_seq < floor && state.buffer_start_seq < state.replay_start_seq {
        state.pop_front();
    }
}

fn active_lag_at_capacity<D, R>(state: &BroadcastState, options: StreamConfig<D, R>) -> bool
where
    D: Delivery,
    R: Replay,
{
    if state.eof || state.read_error.is_some() || state.closed {
        return false;
    }

    state.min_subscriber_cursor().is_some_and(|cursor| {
        state.next_seq.saturating_sub(cursor) >= options.max_buffered_chunks as u64
    })
}

async fn wait_for_reader_capacity<D, R>(shared: &Shared, options: StreamConfig<D, R>)
where
    D: Delivery,
    R: Replay,
{
    if !matches!(
        options.delivery_guarantee(),
        DeliveryGuarantee::ReliableForActiveSubscribers
    ) {
        return;
    }

    loop {
        let notified = shared.reader_available.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        {
            let state = shared.state.lock().expect("broadcast state poisoned");
            if !active_lag_at_capacity(&state, options) {
                return;
            }
        };

        notified.as_mut().await;
    }
}

pub(super) async fn append_event<D, R>(
    shared: &Shared,
    options: StreamConfig<D, R>,
    event: StreamEvent,
) where
    D: Delivery,
    R: Replay,
{
    if matches!(event, StreamEvent::Chunk(_)) {
        wait_for_reader_capacity(shared, options).await;
    }

    {
        let mut state = shared.state.lock().expect("broadcast state poisoned");
        state.push(event, options.replay_retention());
        evict_locked(&mut state, options);
    }
    shared.output_available.notify_waiters();
}
