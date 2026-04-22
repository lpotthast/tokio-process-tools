use super::state::{Shared, SubscriberId, evict_locked};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    BestEffortDelivery, Delivery, NoReplay, Replay, StreamConfig, StreamEvent,
};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

#[derive(Debug)]
pub(super) struct FastSubscription {
    pub(super) receiver: broadcast::Receiver<StreamEvent>,
    pub(super) emit_terminal_event: Option<StreamEvent>,
}

impl FastSubscription {
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        if let Some(event) = self.emit_terminal_event.take() {
            return Some(event);
        }

        match self.receiver.recv().await {
            Ok(event) => Some(event),
            Err(RecvError::Closed) => None,
            Err(RecvError::Lagged(lagged)) => {
                tracing::warn!(lagged, "Broadcast subscriber is lagging behind");
                Some(StreamEvent::Gap)
            }
        }
    }
}

#[derive(Debug)]
pub(super) struct SharedSubscription<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    pub(super) shared: Arc<Shared>,
    pub(super) id: SubscriberId,
    pub(super) options: StreamConfig<D, R>,
    pub(super) done: bool,
}

impl<D, R> Drop for SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        if !self.done {
            let mut state = self.shared.state.lock().expect("broadcast state poisoned");
            state.remove_subscriber(self.id);
            evict_locked(&mut state, self.options);
            drop(state);
            self.shared.reader_available.notify_waiters();
        }
    }
}

impl<D, R> SharedSubscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        loop {
            let notified = self.shared.output_available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut state = self.shared.state.lock().expect("broadcast state poisoned");
                let Some(subscriber) = state.subscribers.get(&self.id) else {
                    self.done = true;
                    return None;
                };
                let cursor = subscriber.cursor;

                if cursor < state.buffer_start_seq {
                    let buffer_start_seq = state.buffer_start_seq;
                    if let Some(subscriber) = state.subscribers.get_mut(&self.id) {
                        subscriber.cursor = buffer_start_seq;
                    }
                    return Some(StreamEvent::Gap);
                }

                if let Some(event) = state.event_at(cursor) {
                    if let Some(subscriber) = state.subscribers.get_mut(&self.id) {
                        subscriber.cursor += 1;
                    }
                    if matches!(event, StreamEvent::Eof | StreamEvent::ReadError(_)) {
                        state.remove_subscriber(self.id);
                        self.done = true;
                    }
                    evict_locked(&mut state, self.options);
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(event);
                }

                if state.eof {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(StreamEvent::Eof);
                }

                if let Some(err) = state.read_error.clone() {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return Some(StreamEvent::ReadError(err));
                }

                if state.closed {
                    state.remove_subscriber(self.id);
                    self.done = true;
                    drop(state);
                    self.shared.reader_available.notify_waiters();
                    return None;
                }
            };

            notified.as_mut().await;
        }
    }
}

#[derive(Debug)]
pub(super) enum Subscription<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    Fast(FastSubscription),
    Shared(SharedSubscription<D, R>),
}

impl<D, R> Subscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) async fn recv(&mut self) -> Option<StreamEvent> {
        match self {
            Subscription::Fast(subscription) => subscription.recv().await,
            Subscription::Shared(subscription) => subscription.recv().await,
        }
    }
}

impl<D, R> EventSubscription for Subscription<D, R>
where
    D: Delivery,
    R: Replay,
{
    #[allow(
        clippy::manual_async_fn,
        reason = "the trait method must expose a Send future for tokio::spawn"
    )]
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        async move { self.recv().await }
    }
}
