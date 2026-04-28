//! Multi-consumer broadcast backend with optional replay.
//!
//! Two implementations live side by side and are selected by
//! [`BroadcastOutputStream::from_stream`]:
//!
//! - [`fast`] — a thin wrapper around `tokio::sync::broadcast` used only when the config
//!   is exactly `BestEffortDelivery + NoReplay`. It avoids the shared-state mutex and the
//!   replay buffer entirely, at the cost of dropping output for slow or late subscribers.
//! - [`fanout`] — the generic `<D: Delivery, R: Replay>` path used for every other
//!   combination. It owns an `Arc<Shared>` that tracks the subscriber registry and replay
//!   history, and routes per-event dispatch through [`state::append_event`] to honor the
//!   configured delivery guarantee.
//!
//! The dispatch lives in [`BroadcastOutputStream::from_stream`] below; see each
//! submodule's `//!` block for the rationale of that path.

use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, Replay, ReplayEnabled,
};
use crate::output_stream::{OutputStream, Subscribable, TrySubscribable};
use crate::{NumBytes, StreamConsumerError};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use tokio::io::AsyncRead;
#[cfg(test)]
use tokio::sync::watch;

mod fanout;
mod fast;
mod state;
mod subscription;

use fanout::{FanoutReplayBackend, new_fanout_backend};
use fast::{FastBackend, new_fast_backend};
use state::{BestEffortLiveQueue, SubscriberSender};
use subscription::{BroadcastSubscription, FastSubscription, LiveReceiver, SharedSubscription};

enum Backend<D, R>
where
    D: Delivery,
    R: Replay,
{
    Fast(FastBackend),
    FanoutReplay(FanoutReplayBackend<D, R>),
}

/// The output stream from a process using a multi-consumer broadcast backend.
///
/// Broadcast streams support multiple consumers and can optionally retain replay history for
/// consumers that attach after output has already arrived. Use this backend when the same stream
/// needs concurrent fanout, such as logging plus readiness checks or logging plus collection.
/// Delivery policy still determines whether slow active consumers observe gaps or apply
/// backpressure.
pub struct BroadcastOutputStream<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    backend: Backend<D, R>,
}

impl<D, R> Drop for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn drop(&mut self) {
        match &self.backend {
            Backend::Fast(backend) => {
                backend.stream_reader.abort();
            }
            Backend::FanoutReplay(backend) => {
                backend.stream_reader.abort();
                {
                    let mut state = backend
                        .shared
                        .state
                        .lock()
                        .expect("broadcast state poisoned");
                    state.close_for_drop();
                }
            }
        }
    }
}

impl<D, R> Debug for BroadcastOutputStream<D, R>
where
    D: Delivery + Debug,
    R: Replay + Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("BroadcastOutputStream");
        debug.field("output_collector", &"non-debug < JoinHandle<()> >");
        match &self.backend {
            Backend::Fast(backend) => {
                debug.field("backend", &"tokio::sync::broadcast");
                debug.field("options", &backend.options);
                debug.field("name", &backend.name);
            }
            Backend::FanoutReplay(backend) => {
                debug.field("backend", &"fanout replay");
                debug.field("options", &backend.options);
                debug.field("name", &backend.name);
            }
        }
        debug.finish_non_exhaustive()
    }
}

impl<D, R> OutputStream for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn read_chunk_size(&self) -> NumBytes {
        match &self.backend {
            Backend::Fast(backend) => backend.options.read_chunk_size,
            Backend::FanoutReplay(backend) => backend.options.read_chunk_size,
        }
    }

    fn max_buffered_chunks(&self) -> usize {
        match &self.backend {
            Backend::Fast(backend) => backend.options.max_buffered_chunks,
            Backend::FanoutReplay(backend) => backend.options.max_buffered_chunks,
        }
    }

    fn name(&self) -> &'static str {
        match &self.backend {
            Backend::Fast(backend) => backend.name,
            Backend::FanoutReplay(backend) => backend.name,
        }
    }
}

impl<D, R> BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Creates a new broadcast output stream from an async read stream and typed stream config.
    pub fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        stream_name: &'static str,
        options: StreamConfig<D, R>,
    ) -> Self {
        options.assert_valid("options");

        if options.delivery_guarantee() == DeliveryGuarantee::BestEffort
            && !options.replay_enabled()
        {
            return Self {
                backend: Backend::Fast(new_fast_backend(
                    stream,
                    stream_name,
                    options.read_chunk_size,
                    options.max_buffered_chunks,
                )),
            };
        }

        Self {
            backend: Backend::FanoutReplay(new_fanout_backend(stream, stream_name, options)),
        }
    }
}

impl<D> BroadcastOutputStream<D, ReplayEnabled>
where
    D: Delivery,
{
    /// Seals replay history for future subscribers.
    ///
    /// This is a one-way, idempotent operation. Active subscribers keep their unread tail data
    /// according to the configured delivery policy.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    pub fn seal_replay(&self) {
        let Backend::FanoutReplay(backend) = &self.backend else {
            return;
        };
        {
            let mut state = backend
                .shared
                .state
                .lock()
                .expect("broadcast state poisoned");
            state.seal_replay();
        }
    }

    /// Returns `true` once replay history has been sealed.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    #[must_use]
    pub fn is_replay_sealed(&self) -> bool {
        let Backend::FanoutReplay(backend) = &self.backend else {
            return false;
        };
        backend
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned")
            .replay_sealed
    }
}

#[cfg(test)]
impl<D, R> BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    pub(super) fn subscribe_bytes_ingested(&self) -> watch::Receiver<u64> {
        match &self.backend {
            Backend::Fast(backend) => backend.bytes_ingested_tx.subscribe(),
            Backend::FanoutReplay(backend) => backend.shared.subscribe_bytes_ingested(),
        }
    }
}

impl<D, R> BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn subscribe(&self) -> BroadcastSubscription<D, R> {
        let Backend::FanoutReplay(backend) = &self.backend else {
            panic!("fanout broadcast subscription requested for fast backend");
        };
        let mut state = backend
            .shared
            .state
            .lock()
            .expect("broadcast state poisoned");

        let (subscriber_sender, live_receiver) = match backend.options.delivery_guarantee() {
            DeliveryGuarantee::ReliableForActiveSubscribers => {
                let (sender, receiver) =
                    tokio::sync::mpsc::channel(backend.options.max_buffered_chunks);
                (
                    SubscriberSender::Reliable(sender),
                    LiveReceiver::Reliable(receiver),
                )
            }
            DeliveryGuarantee::BestEffort => {
                let queue = Arc::new(BestEffortLiveQueue::new(
                    backend.options.max_buffered_chunks,
                ));
                (
                    SubscriberSender::BestEffort(Arc::clone(&queue)),
                    LiveReceiver::BestEffort(queue),
                )
            }
        };
        let (replay, live_start_seq) = state.replay_snapshot(backend.options);
        let id = if state.closed || state.terminal.is_some() {
            None
        } else {
            Some(state.add_subscriber(subscriber_sender))
        };

        BroadcastSubscription::Shared(SharedSubscription {
            shared: Arc::clone(&backend.shared),
            id,
            replay,
            live_start_seq,
            live_receiver: if id.is_some() {
                live_receiver
            } else {
                LiveReceiver::Closed
            },
            _marker: std::marker::PhantomData,
            done: false,
        })
    }

    fn subscribe_normal(&self) -> BroadcastSubscription<D, R> {
        match &self.backend {
            Backend::Fast(backend) => {
                let (receiver, emit_terminal_event) = {
                    let state = backend
                        .closure_state
                        .lock()
                        .expect("closure_state poisoned");
                    let receiver = backend.sender.subscribe();
                    let terminal_event = state
                        .read_error
                        .clone()
                        .map(StreamEvent::ReadError)
                        .or_else(|| state.closed.then_some(StreamEvent::Eof));
                    (receiver, terminal_event)
                };

                BroadcastSubscription::Fast(FastSubscription {
                    receiver,
                    emit_terminal_event,
                })
            }
            Backend::FanoutReplay(_) => self.subscribe(),
        }
    }
}

impl<D, R> Subscribable for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn subscribe(&self) -> impl crate::output_stream::Subscription {
        self.subscribe_normal()
    }
}

impl<D, R> TrySubscribable for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
    fn try_subscribe(
        &self,
    ) -> Result<impl crate::output_stream::Subscription, StreamConsumerError> {
        Ok(self.subscribe_normal())
    }
}

impl_output_stream_consumer_api! {
    impl<D, R> BroadcastOutputStream<D, R>
    where
        D: Delivery,
        R: Replay,
}

#[cfg(test)]
mod tests;
