use crate::output_stream::{ReplayRetention, StreamEvent};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{mpsc, watch};

pub(super) type SubscriberId = u64;

#[derive(Debug)]
pub(super) struct ActiveSubscriber {
    pub(super) id: SubscriberId,
    pub(super) sender: mpsc::Sender<StreamEvent>,
}

#[derive(Debug)]
pub(super) struct ReplayState {
    pub(super) events: VecDeque<StreamEvent>,
    chunk_count: usize,
    byte_count: usize,
    pub(super) replay_start_evicted: bool,
    pub(super) replay_sealed: bool,
    pub(super) terminal_event: Option<StreamEvent>,
    pub(super) active_id: Option<SubscriberId>,
    pub(super) next_subscriber_id: SubscriberId,
}

impl ReplayState {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            chunk_count: 0,
            byte_count: 0,
            replay_start_evicted: false,
            replay_sealed: false,
            terminal_event: None,
            active_id: None,
            next_subscriber_id: 0,
        }
    }

    pub(super) fn push_replay_event(
        &mut self,
        event: StreamEvent,
        retention: Option<ReplayRetention>,
    ) {
        match event {
            StreamEvent::Eof | StreamEvent::ReadError(_) => {
                self.terminal_event = Some(event);
            }
            StreamEvent::Chunk(_) if retention.is_some() && !self.replay_sealed => {
                self.push_retained_event(event);
                self.trim_replay_window(retention);
            }
            StreamEvent::Chunk(_) | StreamEvent::Gap => {}
        }
    }

    fn push_retained_event(&mut self, event: StreamEvent) {
        if let StreamEvent::Chunk(chunk) = &event {
            self.chunk_count += 1;
            self.byte_count += chunk.as_ref().len();
        }
        self.events.push_back(event);
    }

    fn pop_replay_front(&mut self) -> Option<StreamEvent> {
        let event = self.events.pop_front()?;
        if let StreamEvent::Chunk(chunk) = &event {
            self.chunk_count -= 1;
            self.byte_count -= chunk.as_ref().len();
        }
        self.replay_start_evicted = true;
        Some(event)
    }

    pub(super) fn snapshot_events(&self) -> VecDeque<StreamEvent> {
        self.events.iter().cloned().collect()
    }

    pub(super) fn attach_subscriber(&mut self) -> SubscriberId {
        let id = self.next_subscriber_id;
        self.next_subscriber_id += 1;
        self.active_id = Some(id);
        id
    }

    pub(super) fn trim_replay_window(&mut self, retention: Option<ReplayRetention>) {
        match retention {
            None | Some(ReplayRetention::LastChunks(0)) => {
                while self.pop_replay_front().is_some() {}
            }
            Some(
                ReplayRetention::LastChunks(_)
                | ReplayRetention::LastBytes(_)
                | ReplayRetention::All,
            ) if self.replay_sealed => while self.pop_replay_front().is_some() {},
            Some(ReplayRetention::LastBytes(bytes)) if bytes.bytes() == 0 => {
                while self.pop_replay_front().is_some() {}
            }
            Some(ReplayRetention::LastChunks(chunks)) => {
                while self.chunk_count > chunks {
                    let _removed = self.pop_replay_front();
                }
            }
            Some(ReplayRetention::LastBytes(bytes)) => {
                while let Some(event) = self.events.front() {
                    match event {
                        StreamEvent::Chunk(chunk)
                            if self.byte_count.saturating_sub(chunk.as_ref().len())
                                >= bytes.bytes() =>
                        {
                            let _removed = self.pop_replay_front();
                        }
                        StreamEvent::Gap => {
                            let _removed = self.pop_replay_front();
                        }
                        StreamEvent::Chunk(_) => break,
                        StreamEvent::Eof | StreamEvent::ReadError(_) => {
                            unreachable!("terminal events are stored separately")
                        }
                    }
                }
            }
            Some(ReplayRetention::All) => {}
        }
    }
}

#[derive(Debug)]
pub(super) struct ConfiguredShared {
    pub(super) state: Mutex<ReplayState>,
    pub(super) active_tx: watch::Sender<Option<Arc<ActiveSubscriber>>>,
}

impl ConfiguredShared {
    pub(super) fn new() -> Self {
        let (active_tx, _active_rx) = watch::channel(None);
        Self {
            state: Mutex::new(ReplayState::new()),
            active_tx,
        }
    }

    pub(super) fn subscribe_active(&self) -> watch::Receiver<Option<Arc<ActiveSubscriber>>> {
        self.active_tx.subscribe()
    }

    pub(super) fn clear_active_if_current(&self, id: SubscriberId) {
        let mut state = self.state.lock().expect("single-subscriber state poisoned");

        if state.active_id == Some(id) {
            state.active_id = None;
            self.active_tx.send_replace(None);
        }
    }
}
