use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::ReplayRetention;
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
    pub(super) bytes_ingested_tx: watch::Sender<u64>,
    pub(super) terminal_tx: watch::Sender<Option<StreamEvent>>,
}

impl ConfiguredShared {
    pub(super) fn new() -> Self {
        let (active_tx, _) = watch::channel(None);
        let (bytes_ingested_tx, _) = watch::channel(0);
        let (terminal_tx, _) = watch::channel(None);
        Self {
            state: Mutex::new(ReplayState::new()),
            active_tx,
            bytes_ingested_tx,
            terminal_tx,
        }
    }

    pub(super) fn subscribe_active(&self) -> watch::Receiver<Option<Arc<ActiveSubscriber>>> {
        self.active_tx.subscribe()
    }

    #[cfg(test)]
    pub(super) fn subscribe_bytes_ingested(&self) -> watch::Receiver<u64> {
        self.bytes_ingested_tx.subscribe()
    }

    #[cfg(test)]
    pub(super) fn subscribe_terminal(&self) -> watch::Receiver<Option<StreamEvent>> {
        self.terminal_tx.subscribe()
    }

    pub(super) fn record_bytes_ingested(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.bytes_ingested_tx
            .send_modify(|n| *n = n.saturating_add(bytes as u64));
    }

    pub(super) fn clear_active_if_current(&self, id: SubscriberId) {
        let mut state = self.state.lock().expect("single-subscriber state poisoned");

        if state.active_id == Some(id) {
            state.active_id = None;
            self.active_tx.send_replace(None);
        }
    }

    pub(super) fn clear_active(&self) {
        let mut state = self.state.lock().expect("single-subscriber state poisoned");

        state.active_id = None;
        self.active_tx.send_replace(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NumBytesExt, StreamReadError};
    use assertr::prelude::*;
    use std::io;

    fn retained_bytes(state: &ReplayState) -> Vec<u8> {
        state
            .snapshot_events()
            .into_iter()
            .flat_map(|event| match event {
                StreamEvent::Chunk(chunk) => chunk.as_ref().to_vec(),
                StreamEvent::Gap | StreamEvent::Eof | StreamEvent::ReadError(_) => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn no_replay_retains_no_chunks() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"old"), None);

        assert_that!(state.events).is_empty();
        assert_that!(state.chunk_count).is_equal_to(0);
        assert_that!(state.byte_count).is_equal_to(0);
    }

    #[test]
    fn replay_all_retains_all_chunks() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"old"), Some(ReplayRetention::All));
        state.push_replay_event(StreamEvent::chunk(b"live"), Some(ReplayRetention::All));

        assert_that!(retained_bytes(&state)).is_equal_to(b"oldlive".to_vec());
        assert_that!(state.chunk_count).is_equal_to(2);
        assert_that!(state.byte_count).is_equal_to(7);
    }

    #[test]
    fn replay_last_chunks_retains_bounded_tail() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"aa"), Some(ReplayRetention::LastChunks(2)));
        state.push_replay_event(StreamEvent::chunk(b"bb"), Some(ReplayRetention::LastChunks(2)));
        state.push_replay_event(StreamEvent::chunk(b"cc"), Some(ReplayRetention::LastChunks(2)));

        assert_that!(retained_bytes(&state)).is_equal_to(b"bbcc".to_vec());
        assert_that!(state.chunk_count).is_equal_to(2);
        assert_that!(state.byte_count).is_equal_to(4);
    }

    #[test]
    fn replay_last_bytes_retains_bounded_tail() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"aa"), Some(ReplayRetention::LastBytes(4.bytes())));
        state.push_replay_event(StreamEvent::chunk(b"bb"), Some(ReplayRetention::LastBytes(4.bytes())));
        state.push_replay_event(StreamEvent::chunk(b"cc"), Some(ReplayRetention::LastBytes(4.bytes())));

        assert_that!(retained_bytes(&state)).is_equal_to(b"bbcc".to_vec());
        assert_that!(state.chunk_count).is_equal_to(2);
        assert_that!(state.byte_count).is_equal_to(4);
    }

    #[test]
    fn replay_last_bytes_keeps_whole_chunks_covering_boundary() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"aa"), Some(ReplayRetention::LastBytes(3.bytes())));
        state.push_replay_event(StreamEvent::chunk(b"bb"), Some(ReplayRetention::LastBytes(3.bytes())));
        state.push_replay_event(StreamEvent::chunk(b"cc"), Some(ReplayRetention::LastBytes(3.bytes())));

        assert_that!(retained_bytes(&state)).is_equal_to(b"bbcc".to_vec());
        assert_that!(state.byte_count).is_equal_to(4);
    }

    #[test]
    fn seal_trims_retained_replay() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::chunk(b"old"), Some(ReplayRetention::All));
        state.replay_sealed = true;
        state.trim_replay_window(Some(ReplayRetention::All));

        assert_that!(state.events).is_empty();
        assert_that!(state.chunk_count).is_equal_to(0);
        assert_that!(state.byte_count).is_equal_to(0);
    }

    #[test]
    fn eof_is_stored_as_terminal_state_not_replay_chunk() {
        let mut state = ReplayState::new();

        state.push_replay_event(StreamEvent::Eof, Some(ReplayRetention::All));

        assert_that!(state.events).is_empty();
        assert_that!(state.terminal_event).is_equal_to(Some(StreamEvent::Eof));
    }

    #[test]
    fn read_error_is_stored_as_terminal_state_not_replay_chunk() {
        let mut state = ReplayState::new();

        state.push_replay_event(
            StreamEvent::ReadError(StreamReadError::new(
                "custom",
                io::Error::from(io::ErrorKind::BrokenPipe),
            )),
            Some(ReplayRetention::All),
        );

        assert_that!(state.events).is_empty();
        match state.terminal_event {
            Some(StreamEvent::ReadError(err)) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
            }
        }
    }
}
