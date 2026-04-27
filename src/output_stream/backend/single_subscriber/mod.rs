use crate::output_stream::config::StreamConfig;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::policy::{Delivery, Replay, ReplayRetention};
use crate::output_stream::{OutputStream, Subscribable, Subscription};
use crate::{
    CollectedBytes, CollectedLines, Collector, LineCollectionOptions, LineParsingOptions, NumBytes,
    RawCollectionOptions,
};
use std::collections::VecDeque;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

mod reader;
mod state;
mod subscription;

use crate::output_stream::Collectable;
use reader::read_chunked;
use state::{ActiveSubscriber, ConfiguredShared};
use subscription::SingleSubscriberSubscription;

impl Subscription for mpsc::Receiver<StreamEvent> {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        self.recv()
    }
}

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one active consumer at a time.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::backend::broadcast::BroadcastOutputStream`.
pub struct SingleSubscriberOutputStream {
    /// The task that reads output from the underlying stream and routes it to the active
    /// subscriber, replay storage, or discard path.
    stream_reader: JoinHandle<()>,

    /// The maximum size of every chunk read by the backing `stream_reader`.
    read_chunk_size: NumBytes,

    /// The maximum capacity of the channel caching the chunks before being processed.
    max_buffered_chunks: usize,

    /// Shared replay state for typed single-subscriber configurations.
    configured_shared: Option<Arc<ConfiguredShared>>,

    /// Replay retention for typed single-subscriber configurations.
    replay_retention: Option<ReplayRetention>,

    /// Whether replay-specific APIs are enabled.
    replay_enabled: bool,

    /// Name of this stream.
    name: &'static str,
}

impl Drop for SingleSubscriberOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
        if let Some(shared) = &self.configured_shared {
            shared.clear_active();
        }
    }
}

impl Debug for SingleSubscriberOutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleSubscriberOutputStream")
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field("configured", &self.configured_shared.is_some())
            .finish_non_exhaustive()
    }
}

impl OutputStream for SingleSubscriberOutputStream {
    fn read_chunk_size(&self) -> NumBytes {
        self.read_chunk_size
    }

    fn max_buffered_chunks(&self) -> usize {
        self.max_buffered_chunks
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl Collectable for SingleSubscriberOutputStream {
    fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines> {
        SingleSubscriberOutputStream::collect_lines_into_vec(
            self,
            parsing_options,
            collection_options,
        )
    }

    fn collect_chunks_into_vec(&self, options: RawCollectionOptions) -> Collector<CollectedBytes> {
        SingleSubscriberOutputStream::collect_chunks_into_vec(self, options)
    }
}

impl SingleSubscriberOutputStream {
    /// Creates a new single-subscriber output stream from an async read stream and typed stream config.
    pub fn from_stream<S, D, R>(
        stream: S,
        stream_name: &'static str,
        options: StreamConfig<D, R>,
    ) -> SingleSubscriberOutputStream
    where
        S: AsyncRead + Unpin + Send + 'static,
        D: Delivery,
        R: Replay,
    {
        options.assert_valid("options");

        let shared = Arc::new(ConfiguredShared::new());
        let active_rx = shared.subscribe_active();
        let delivery_guarantee = options.delivery_guarantee();
        let replay_retention = options.replay_retention();
        let replay_enabled = options.replay_enabled();

        let stream_reader = tokio::spawn(read_chunked(
            stream,
            Arc::clone(&shared),
            active_rx,
            options.read_chunk_size,
            delivery_guarantee,
            replay_retention,
            stream_name,
        ));

        SingleSubscriberOutputStream {
            stream_reader,
            read_chunk_size: options.read_chunk_size,
            max_buffered_chunks: options.max_buffered_chunks,
            configured_shared: Some(shared),
            replay_retention,
            replay_enabled,
            name: stream_name,
        }
    }

    /// Returns whether replay-specific APIs are enabled for this stream.
    #[must_use]
    pub fn replay_enabled(&self) -> bool {
        self.replay_enabled
    }

    /// Returns the configured replay retention.
    #[must_use]
    pub fn replay_retention(&self) -> Option<ReplayRetention> {
        self.replay_retention
    }

    fn panic_on_multiple_consumers(&self) -> ! {
        panic!(
            "Cannot create multiple active consumers on SingleSubscriberOutputStream (stream: '{}'). \
            Only one active inspector, collector, or line waiter can be active at a time. \
            Use .stdout_and_stderr(|stream| stream.broadcast().best_effort_delivery().no_replay().read_chunk_size(DEFAULT_READ_CHUNK_SIZE).max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)).spawn() to support multiple consumers.",
            self.name
        )
    }

    fn take_subscription(&self) -> SingleSubscriberSubscription {
        let Some(shared) = &self.configured_shared else {
            panic!("configured single-subscriber subscription requested without shared state");
        };

        let (sender, receiver) = mpsc::channel(self.max_buffered_chunks);
        let (id, replay, terminal_event) = {
            let mut state = shared
                .state
                .lock()
                .expect("single-subscriber state poisoned");

            if state.active_id.is_some() {
                drop(state);
                self.panic_on_multiple_consumers();
            }

            let replay = if state.replay_sealed || self.replay_retention.is_none() {
                VecDeque::default()
            } else {
                state.snapshot_events()
            };
            let id = state.attach_subscriber();
            shared
                .active_tx
                .send_replace(Some(Arc::new(ActiveSubscriber { id, sender })));
            (id, replay, state.terminal_event.clone())
        };

        SingleSubscriberSubscription {
            id,
            shared: Arc::clone(shared),
            replay,
            terminal_event,
            live_receiver: Some(receiver),
        }
    }

    /// Seals replay history for future subscribers.
    ///
    /// This is a one-way, idempotent operation.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    pub fn seal_replay(&self) {
        let Some(shared) = &self.configured_shared else {
            return;
        };
        let mut state = shared
            .state
            .lock()
            .expect("single-subscriber state poisoned");
        state.replay_sealed = true;
        state.trim_replay_window(self.replay_retention);
    }

    /// Returns `true` once replay history has been sealed.
    ///
    /// # Panics
    ///
    /// Panics if the internal state mutex is poisoned.
    #[must_use]
    pub fn is_replay_sealed(&self) -> bool {
        let Some(shared) = &self.configured_shared else {
            return false;
        };
        shared
            .state
            .lock()
            .expect("single-subscriber state poisoned")
            .replay_sealed
    }
}

impl Subscribable for SingleSubscriberOutputStream {
    fn subscribe(&self) -> impl Subscription {
        self.take_subscription()
    }
}

impl_output_stream_consumer_api! {
    impl SingleSubscriberOutputStream
}

#[cfg(test)]
mod tests;
