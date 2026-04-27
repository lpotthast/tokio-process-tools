use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::{Delivery, DeliveryGuarantee, Replay, ReplayRetention};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};

pub(super) type SubscriberId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IndexedEvent {
    pub(super) seq: u64,
    pub(super) event: StreamEvent,
}

#[derive(Debug)]
struct BestEffortQueueState {
    events: VecDeque<IndexedEvent>,
    closed: bool,
}

#[derive(Debug)]
pub(super) struct BestEffortLiveQueue {
    capacity: usize,
    state: Mutex<BestEffortQueueState>,
    available: Notify,
}

impl BestEffortLiveQueue {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(BestEffortQueueState {
                events: VecDeque::new(),
                closed: false,
            }),
            available: Notify::new(),
        }
    }

    pub(super) fn push(&self, event: IndexedEvent) -> bool {
        let mut state = self.state.lock().expect("best-effort queue poisoned");
        if state.closed {
            return false;
        }

        match &event.event {
            StreamEvent::Chunk(_) | StreamEvent::Gap => {
                if state.events.len() >= self.capacity {
                    state.events.clear();
                    state.events.push_back(IndexedEvent {
                        seq: event.seq,
                        event: StreamEvent::Gap,
                    });
                    if self.capacity > 1 {
                        state.events.push_back(event);
                    }
                } else {
                    state.events.push_back(event);
                }
            }
            StreamEvent::Eof | StreamEvent::ReadError(_) => {
                state.events.push_back(event);
                state.closed = true;
            }
        }

        drop(state);
        self.available.notify_waiters();
        true
    }

    pub(super) async fn recv(&self) -> Option<IndexedEvent> {
        loop {
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut state = self.state.lock().expect("best-effort queue poisoned");
                if let Some(event) = state.events.pop_front() {
                    return Some(event);
                }
                if state.closed {
                    return None;
                }
            }

            notified.as_mut().await;
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().expect("best-effort queue poisoned");
        state.closed = true;
        drop(state);
        self.available.notify_waiters();
    }
}

#[derive(Debug, Clone)]
pub(super) enum SubscriberSender {
    Reliable(mpsc::Sender<IndexedEvent>),
    BestEffort(Arc<BestEffortLiveQueue>),
}

impl SubscriberSender {
    fn close(&self) {
        match self {
            Self::Reliable(_) => {}
            Self::BestEffort(queue) => queue.close(),
        }
    }
}

#[derive(Debug)]
pub(super) struct BroadcastState {
    pub(super) replay: VecDeque<IndexedEvent>,
    pub(super) next_seq: u64,
    pub(super) replay_start_seq: u64,
    replay_chunk_count: usize,
    pub(super) replay_byte_count: usize,
    next_subscriber_id: SubscriberId,
    pub(super) subscribers: HashMap<SubscriberId, SubscriberSender>,
    pub(super) replay_sealed: bool,
    pub(super) terminal: Option<IndexedEvent>,
    pub(super) closed: bool,
}

impl BroadcastState {
    fn new() -> Self {
        Self {
            replay: VecDeque::new(),
            next_seq: 0,
            replay_start_seq: 0,
            replay_chunk_count: 0,
            replay_byte_count: 0,
            next_subscriber_id: 0,
            subscribers: HashMap::new(),
            replay_sealed: false,
            terminal: None,
            closed: false,
        }
    }

    pub(super) fn push_event<D, R>(
        &mut self,
        event: StreamEvent,
        options: StreamConfig<D, R>,
    ) -> IndexedEvent
    where
        D: Delivery,
        R: Replay,
    {
        let indexed = IndexedEvent {
            seq: self.next_seq,
            event,
        };
        self.next_seq += 1;

        match &indexed.event {
            StreamEvent::Chunk(chunk)
                if options.replay_retention().is_some() && !self.replay_sealed =>
            {
                self.replay_chunk_count += 1;
                self.replay_byte_count += chunk.as_ref().len();
                self.replay.push_back(indexed.clone());
                self.trim_replay_window(options.replay_retention());
            }
            StreamEvent::Eof | StreamEvent::ReadError(_) => {
                self.terminal = Some(indexed.clone());
            }
            StreamEvent::Chunk(_) | StreamEvent::Gap => {}
        }

        indexed
    }

    pub(super) fn replay_snapshot<D, R>(
        &self,
        options: StreamConfig<D, R>,
    ) -> (VecDeque<IndexedEvent>, u64)
    where
        D: Delivery,
        R: Replay,
    {
        let mut snapshot = if options.replay_retention().is_some() && !self.replay_sealed {
            self.replay.clone()
        } else {
            VecDeque::new()
        };

        if let Some(terminal) = &self.terminal {
            snapshot.push_back(terminal.clone());
        }

        (snapshot, self.next_seq)
    }

    pub(super) fn add_subscriber(&mut self, sender: SubscriberSender) -> SubscriberId {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        self.subscribers.insert(id, sender);
        id
    }

    pub(super) fn remove_subscriber(&mut self, id: SubscriberId) {
        self.subscribers.remove(&id);
    }

    pub(super) fn seal_replay(&mut self) {
        self.replay_sealed = true;
        self.clear_replay();
        self.replay_start_seq = self.next_seq;
    }

    pub(super) fn close_for_drop(&mut self) {
        self.closed = true;
        for sender in self.subscribers.values() {
            sender.close();
        }
        self.subscribers.clear();
    }

    fn clear_replay(&mut self) {
        self.replay.clear();
        self.replay_chunk_count = 0;
        self.replay_byte_count = 0;
    }

    fn pop_replay_front(&mut self) -> Option<IndexedEvent> {
        let event = self.replay.pop_front()?;
        if let StreamEvent::Chunk(chunk) = &event.event {
            self.replay_chunk_count -= 1;
            self.replay_byte_count -= chunk.as_ref().len();
        }
        self.replay_start_seq = self.replay.front().map_or(self.next_seq, |event| event.seq);
        Some(event)
    }

    fn trim_replay_window(&mut self, retention: Option<ReplayRetention>) {
        match retention {
            None | Some(ReplayRetention::LastChunks(0)) => {
                self.clear_replay();
            }
            Some(
                ReplayRetention::LastChunks(_)
                | ReplayRetention::LastBytes(_)
                | ReplayRetention::All,
            ) if self.replay_sealed => {
                self.clear_replay();
            }
            Some(ReplayRetention::LastBytes(bytes)) if bytes.bytes() == 0 => {
                self.clear_replay();
            }
            Some(ReplayRetention::LastChunks(chunks)) => {
                while self.replay_chunk_count > chunks {
                    let _removed = self.pop_replay_front();
                }
            }
            Some(ReplayRetention::LastBytes(bytes)) => {
                while let Some(event) = self.replay.front() {
                    match &event.event {
                        StreamEvent::Chunk(chunk)
                            if self.replay_byte_count.saturating_sub(chunk.as_ref().len())
                                >= bytes.bytes() =>
                        {
                            let _removed = self.pop_replay_front();
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

        self.replay_start_seq = self.replay.front().map_or(self.next_seq, |event| event.seq);
    }
}

#[derive(Debug)]
pub(super) struct Shared {
    pub(super) state: Mutex<BroadcastState>,
}

impl Shared {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(BroadcastState::new()),
        }
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
    let (indexed, subscribers, terminal) = {
        let mut state = shared.state.lock().expect("broadcast state poisoned");
        if state.closed {
            return;
        }

        let indexed = state.push_event(event, options);
        let terminal = matches!(indexed.event, StreamEvent::Eof | StreamEvent::ReadError(_));
        let subscribers = state
            .subscribers
            .iter()
            .map(|(id, sender)| (*id, sender.clone()))
            .collect::<Vec<_>>();
        if terminal {
            state.subscribers.clear();
        }
        (indexed, subscribers, terminal)
    };

    match options.delivery_guarantee() {
        DeliveryGuarantee::ReliableForActiveSubscribers => {
            let mut stale_subscribers = Vec::new();
            for (id, sender) in subscribers {
                let SubscriberSender::Reliable(sender) = sender else {
                    continue;
                };
                if sender.send(indexed.clone()).await.is_err() {
                    stale_subscribers.push(id);
                }
            }

            if !terminal && !stale_subscribers.is_empty() {
                let mut state = shared.state.lock().expect("broadcast state poisoned");
                for id in stale_subscribers {
                    state.remove_subscriber(id);
                }
            }
        }
        DeliveryGuarantee::BestEffort => {
            let mut stale_subscribers = Vec::new();
            for (id, sender) in subscribers {
                let SubscriberSender::BestEffort(queue) = sender else {
                    continue;
                };
                if !queue.push(indexed.clone()) {
                    stale_subscribers.push(id);
                }
            }

            if !terminal && !stale_subscribers.is_empty() {
                let mut state = shared.state.lock().expect("broadcast state poisoned");
                for id in stale_subscribers {
                    state.remove_subscriber(id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::event::Chunk;
    use crate::{NumBytesExt, ReplayEnabled};
    use assertr::prelude::*;
    use bytes::Bytes;

    fn chunk(bytes: &'static [u8]) -> StreamEvent {
        StreamEvent::Chunk(Chunk(Bytes::from_static(bytes)))
    }

    fn best_effort_options(
        retention: ReplayRetention,
    ) -> StreamConfig<crate::BestEffortDelivery, ReplayEnabled> {
        let builder = StreamConfig::builder().best_effort_delivery();
        match retention {
            ReplayRetention::LastChunks(chunks) => builder.replay_last_chunks(chunks),
            ReplayRetention::LastBytes(bytes) => builder.replay_last_bytes(bytes),
            ReplayRetention::All => builder.replay_all(),
        }
        .read_chunk_size(2.bytes())
        .max_buffered_chunks(4)
        .build()
    }

    fn assert_chunk(event: Option<&IndexedEvent>, expected: &'static [u8]) {
        match event {
            Some(IndexedEvent {
                event: StreamEvent::Chunk(chunk),
                ..
            }) => {
                assert_that!(chunk.as_ref()).is_equal_to(expected);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected chunk, got {other:?}"));
            }
        }
    }

    #[test]
    fn replay_last_chunks_starts_at_retention_boundary() {
        let options = best_effort_options(ReplayRetention::LastChunks(2));
        let mut state = BroadcastState::new();

        state.push_event(chunk(b"a\n"), options);
        state.push_event(chunk(b"b\n"), options);
        state.push_event(chunk(b"c\n"), options);

        assert_that!(state.replay_start_seq).is_equal_to(1);
        assert_that!(state.replay_chunk_count).is_equal_to(2);
        assert_that!(state.replay_byte_count).is_equal_to(4);
        assert_chunk(state.replay.front(), b"b\n");
        assert_chunk(state.replay.get(1), b"c\n");
    }

    #[test]
    fn replay_last_bytes_keeps_whole_chunks_covering_boundary() {
        let options = best_effort_options(ReplayRetention::LastBytes(3.bytes()));
        let mut state = BroadcastState::new();

        state.push_event(chunk(b"aa"), options);
        state.push_event(chunk(b"bb"), options);
        state.push_event(chunk(b"cc"), options);

        assert_that!(state.replay_start_seq).is_equal_to(1);
        assert_that!(state.replay_chunk_count).is_equal_to(2);
        assert_that!(state.replay_byte_count).is_equal_to(4);
        assert_chunk(state.replay.front(), b"bb");
        assert_chunk(state.replay.get(1), b"cc");
    }

    #[test]
    fn seal_trims_replay_without_touching_terminal() {
        let options = best_effort_options(ReplayRetention::All);
        let mut state = BroadcastState::new();

        state.push_event(chunk(b"old"), options);
        state.push_event(StreamEvent::Eof, options);
        state.seal_replay();

        assert_that!(state.replay.len()).is_equal_to(0);
        assert_that!(state.replay_byte_count).is_equal_to(0);
        assert_that!(state.terminal).is_some();
    }

    #[tokio::test]
    async fn reliable_append_waits_for_slow_subscriber_capacity() {
        let options = StreamConfig::builder()
            .reliable_for_active_subscribers()
            .no_replay()
            .read_chunk_size(1.bytes())
            .max_buffered_chunks(1)
            .build();
        let shared = Arc::new(Shared::new());
        let (sender, mut receiver) = mpsc::channel(1);
        shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .add_subscriber(SubscriberSender::Reliable(sender));

        append_event(&shared, options, chunk(b"a")).await;
        let second_append = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move { append_event(&shared, options, chunk(b"b")).await }
        });

        assert_that!(second_append.is_finished()).is_false();
        assert_that!(receiver.recv().await.unwrap().seq).is_equal_to(0);
        second_append.await.unwrap();
        assert_that!(receiver.recv().await.unwrap().seq).is_equal_to(1);
    }

    #[tokio::test]
    async fn best_effort_queue_emits_one_gap_after_overflow() {
        let queue = BestEffortLiveQueue::new(2);

        queue.push(IndexedEvent {
            seq: 0,
            event: chunk(b"a"),
        });
        queue.push(IndexedEvent {
            seq: 1,
            event: chunk(b"b"),
        });
        queue.push(IndexedEvent {
            seq: 2,
            event: chunk(b"c"),
        });

        assert_that!(queue.recv().await.unwrap().event).is_equal_to(StreamEvent::Gap);
        match queue.recv().await.unwrap().event {
            StreamEvent::Chunk(chunk) => {
                assert_that!(chunk.as_ref()).is_equal_to(b"c");
            }
            other => assert_that!(&other).fail(format_args!("expected chunk, got {other:?}")),
        }
    }
}
