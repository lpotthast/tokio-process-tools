use crate::output_stream::{ReplayRetention, StreamEvent};
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::sync::Notify;

#[derive(Debug)]
pub(super) struct ReplayState {
    pub(super) events: VecDeque<StreamEvent>,
    chunk_count: usize,
    byte_count: usize,
    pub(super) replay_start_evicted: bool,
    pub(super) replay_sealed: bool,
    pub(super) consumer_attached: bool,
    pub(super) terminal_event: Option<StreamEvent>,
}

impl ReplayState {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            chunk_count: 0,
            byte_count: 0,
            replay_start_evicted: false,
            replay_sealed: false,
            consumer_attached: false,
            terminal_event: None,
        }
    }

    pub(super) fn push_pre_consumer(
        &mut self,
        event: StreamEvent,
        retention: Option<ReplayRetention>,
    ) {
        match event {
            StreamEvent::Eof | StreamEvent::ReadError(_) => {
                self.terminal_event = Some(event);
            }
            StreamEvent::Chunk(_) | StreamEvent::Gap => {
                self.push_replay_event(event);
                self.trim_replay_window(retention);
            }
        }
    }

    fn push_replay_event(&mut self, event: StreamEvent) {
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

    pub(super) fn take_events(&mut self) -> VecDeque<StreamEvent> {
        self.chunk_count = 0;
        self.byte_count = 0;
        std::mem::take(&mut self.events)
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
    pub(super) consumer_attached: Notify,
}

impl ConfiguredShared {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(ReplayState::new()),
            consumer_attached: Notify::new(),
        }
    }
}
