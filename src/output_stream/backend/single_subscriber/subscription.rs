use super::state::{ConfiguredShared, SubscriberId};
use crate::output_stream::{StreamEvent, Subscription};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(super) struct SingleSubscriberSubscription {
    pub(super) id: SubscriberId,
    pub(super) shared: Arc<ConfiguredShared>,
    pub(super) replay: VecDeque<StreamEvent>,
    pub(super) terminal_event: Option<StreamEvent>,
    pub(super) live_receiver: Option<mpsc::Receiver<StreamEvent>>,
}

impl Subscription for SingleSubscriberSubscription {
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

impl Drop for SingleSubscriberSubscription {
    fn drop(&mut self) {
        self.live_receiver = None;
        self.shared.clear_active_if_current(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::chunk;
    use super::*;
    use crate::StreamReadError;
    use assertr::prelude::*;
    use std::io;

    fn attach_active(shared: &Arc<ConfiguredShared>) -> SubscriberId {
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.attach_subscriber()
    }

    fn subscription_with(
        shared: Arc<ConfiguredShared>,
        id: SubscriberId,
        replay: impl IntoIterator<Item = StreamEvent>,
        terminal_event: Option<StreamEvent>,
        live_receiver: Option<mpsc::Receiver<StreamEvent>>,
    ) -> SingleSubscriberSubscription {
        SingleSubscriberSubscription {
            id,
            shared,
            replay: replay.into_iter().collect(),
            terminal_event,
            live_receiver,
        }
    }

    #[tokio::test]
    async fn emits_replay_before_live_events() {
        let shared = Arc::new(ConfiguredShared::new());
        let id = attach_active(&shared);
        let (sender, receiver) = mpsc::channel(4);
        let mut subscription = subscription_with(
            Arc::clone(&shared),
            id,
            [chunk(b"old")],
            None,
            Some(receiver),
        );

        sender.send(chunk(b"live")).await.unwrap();
        drop(sender);

        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(chunk(b"old"));
        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(chunk(b"live"));
        assert_that!(subscription.next_event().await).is_none();
    }

    #[tokio::test]
    async fn emits_terminal_event_after_replay_and_closes_live_receiver() {
        let shared = Arc::new(ConfiguredShared::new());
        let id = attach_active(&shared);
        let (sender, receiver) = mpsc::channel(4);
        let mut subscription = subscription_with(
            Arc::clone(&shared),
            id,
            [chunk(b"old")],
            Some(StreamEvent::Eof),
            Some(receiver),
        );

        sender.send(chunk(b"ignored-live")).await.unwrap();

        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(chunk(b"old"));
        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(StreamEvent::Eof);
        assert_that!(subscription.next_event().await).is_none();
    }

    #[tokio::test]
    async fn emits_gap_read_error_and_eof_from_replay() {
        let shared = Arc::new(ConfiguredShared::new());
        let id = attach_active(&shared);
        let mut subscription = subscription_with(
            Arc::clone(&shared),
            id,
            [
                StreamEvent::Gap,
                StreamEvent::ReadError(StreamReadError::new(
                    "custom",
                    io::Error::from(io::ErrorKind::BrokenPipe),
                )),
                StreamEvent::Eof,
            ],
            None,
            None,
        );

        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(StreamEvent::Gap);
        match subscription.next_event().await {
            Some(StreamEvent::ReadError(err)) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected read error, got {other:?}"));
            }
        }
        assert_that!(subscription.next_event().await)
            .is_some()
            .is_equal_to(StreamEvent::Eof);
        assert_that!(subscription.next_event().await).is_none();
    }

    #[tokio::test]
    async fn drop_clears_active_backend_registration() {
        let shared = Arc::new(ConfiguredShared::new());
        let id = attach_active(&shared);
        let (_sender, receiver) = mpsc::channel(4);
        let subscription = subscription_with(Arc::clone(&shared), id, [], None, Some(receiver));

        {
            let state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");
            assert_that!(state.active_id).is_some().is_equal_to(id);
        }

        drop(subscription);

        let state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        assert_that!(state.active_id).is_none();
    }
}
