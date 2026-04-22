use super::state::{ConfiguredShared, SubscriberId};
use crate::output_stream::StreamEvent;
use crate::output_stream::subscription::EventSubscription;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(super) struct ConfiguredSubscription {
    pub(super) id: SubscriberId,
    pub(super) shared: Arc<ConfiguredShared>,
    pub(super) replay: VecDeque<StreamEvent>,
    pub(super) terminal_event: Option<StreamEvent>,
    pub(super) live_receiver: Option<mpsc::Receiver<StreamEvent>>,
}

impl EventSubscription for ConfiguredSubscription {
    #[allow(
        clippy::manual_async_fn,
        reason = "the trait method must expose a Send future for tokio::spawn"
    )]
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        async move {
            if let Some(event) = self.replay.pop_front() {
                return Some(event);
            }
            if let Some(event) = self.terminal_event.take() {
                self.live_receiver = None;
                return Some(event);
            }
            match &mut self.live_receiver {
                Some(receiver) => receiver.recv().await,
                None => None,
            }
        }
    }
}

impl Drop for ConfiguredSubscription {
    fn drop(&mut self) {
        self.live_receiver = None;
        self.shared.clear_active_if_current(self.id);
    }
}

pub(super) type SingleSubscription = ConfiguredSubscription;

#[derive(Debug, Clone, Copy)]
pub(super) enum SubscriptionStart {
    ReplayAvailable,
    ReplayFromStart,
}
