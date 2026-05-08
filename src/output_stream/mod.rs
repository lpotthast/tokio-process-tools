//! Process output stream types and helpers.
//!
//! The submodules below correspond to the four conceptual layers this subsystem is built from:
//!
//! - **Core abstractions:** the [`OutputStream`] / [`Subscription`] / [`Subscribable`] /
//!   [`Next`] traits defined here, [`event`]'s [`Chunk`](event::Chunk) /
//!   [`StreamEvent`](StreamEvent), the [`policy`] / [`config`] / [`num_bytes`] / [`line`]
//!   modules, and the [`visitor`] trait pair every visitor implementation builds against. These
//!   files have no tokio dependency.
//! - **Tokio runtime adapter** ([`consumer`]): the [`Consumer<S>`](Consumer) handle
//!   plus the driver loops that step a visitor over a subscription on a tokio task with
//!   cooperative-cancel / abort semantics.
//! - **Tokio stream backends** ([`backend`]): `broadcast` and `single_subscriber`, which ingest
//!   any [`tokio::io::AsyncRead`] and emit [`StreamEvent`](StreamEvent)s.
//! - **User-replaceable convenience layer** ([`visitors`]): the built-in `collect`, `inspect`,
//!   `wait`, and `write` visitor implementations. `consume(my_visitor)` /
//!   `consume_async(my_visitor)` on any [`Consumable`] stream is the single entry point;
//!   construct a bundled visitor for the common cases or implement [`StreamVisitor`] /
//!   [`AsyncStreamVisitor`] for custom ones.

pub(crate) mod consumer;

/// Output stream backend implementations.
pub(crate) mod backend;

/// Shared stream consumption configuration.
pub(crate) mod config;

pub(crate) mod event;

/// Line parsing types and options.
pub(crate) mod line;

/// `NumBytes` newtype and convenience constructors used throughout the public API.
pub(crate) mod num_bytes;

/// Delivery and replay policy types shared by output stream backends.
pub(crate) mod policy;

/// Visitor traits, the runtime-agnostic contract every stream observer implements.
pub(crate) mod visitor;

/// Built-in [`StreamVisitor`] / [`AsyncStreamVisitor`] implementations covering the common
/// inspect / collect / write / wait cases.
pub mod visitors;

use crate::output_stream::consumer::{spawn_consumer_async, spawn_consumer_sync};
use crate::{AsyncStreamVisitor, Consumer, StreamVisitor};
use core::error::Error;
use event::StreamEvent;
use num_bytes::NumBytes;

/// We support the following implementations:
///
/// - [`crate::BroadcastOutputStream`]
/// - [`crate::SingleSubscriberOutputStream`]
pub trait OutputStream: Consumable {
    /// The maximum size of every chunk read by the backing `stream_reader`.
    fn read_chunk_size(&self) -> NumBytes;

    /// The number of chunks held by the underlying async channel.
    fn max_buffered_chunks(&self) -> usize;

    /// Type of stream. Can be "stdout" or "stderr". But we do not guarantee this output.
    /// It should only be used for logging/debugging.
    fn name(&self) -> &'static str;
}

/// Stream event subscription used by built-in consumers.
pub trait Subscription: Send + 'static {
    /// Returns the next stream event, or `None` once the subscription is closed.
    ///
    /// `None` is only returned after the subscription has emitted a terminal event
    /// ([`StreamEvent::Eof`] or [`StreamEvent::ReadError`]) on this subscription, after which it
    /// will remain `None` for every subsequent call. Implementations must not return `None` while
    /// further chunks or gap markers are still pending.
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_;
}

/// Output stream backend that can reject consumer subscriptions.
pub trait Subscribable {
    /// The concrete subscription handle returned by [`try_subscribe`](Self::try_subscribe).
    type Subscription: Subscription;

    /// The error returned when subscription fails.
    type SubscribeError: Error + Send + Sync + 'static;

    /// Creates a new subscription for a consumer, or returns why the consumer cannot be started.
    ///
    /// # Errors
    ///
    /// Returns [`Self::SubscribeError`] when the backend cannot start a new consumer, for
    /// example because a single-subscriber backend already has an active consumer.
    fn try_subscribe(&self) -> Result<Self::Subscription, Self::SubscribeError>;
}

/// Enables a stream to be consumed by [`StreamVisitor`]s and [`AsyncStreamVisitor`]s.
///
/// Construct a bundled visitor under [`visitors`] (or your own [`StreamVisitor`] /
/// [`AsyncStreamVisitor`] implementation) and pass it to [`consume`](Self::consume) or
/// [`consume_async`](Self::consume_async). The returned [`Consumer`] owns the spawned tokio
/// task that drives the visitor over this stream.
///
/// Implementors only need to specify [`Error`](Self::Error). The `consume` and `consume_async`
/// methods have default impls that subscribe via [`Subscribable::try_subscribe`] and spawn the
/// consumer task; those defaults additionally require `Self: OutputStream` because they label
/// the consumer task with [`OutputStream::name`].
pub trait Consumable: Subscribable {
    /// Error returned when consumer creation fails. Must be constructible from the underlying
    /// [`Subscribable::SubscribeError`] so the default `consume` / `consume_async` impls can
    /// propagate subscription failures.
    type Error: Error + Send + Sync + 'static + From<Self::SubscribeError>;

    //noinspection RsDoubleMustUse
    /// Tries to drive the provided synchronous [`StreamVisitor`] over this stream and returns a
    /// [`Consumer`] that owns the spawned task.
    ///
    /// The returned [`Consumer`]'s [`wait`](Consumer::wait) yields whatever the visitor produces
    /// through [`StreamVisitor::into_output`], allowing visitor implementors to give and get
    /// back ownership of some value.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the backend rejects the consumer creation (for example,
    /// because a single-subscriber backend already has an active consumer).
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the `Consumer`-internal tokio task, meaning that your visitor is never invoked and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
    fn consume<V>(&self, visitor: V) -> Result<Consumer<V::Output>, Self::Error>
    where
        V: StreamVisitor,
        Self: OutputStream,
    {
        Ok(spawn_consumer_sync(
            self.name(),
            self.try_subscribe()?,
            visitor,
        ))
    }

    //noinspection RsDoubleMustUse
    /// Tries to drive the provided asynchronous [`AsyncStreamVisitor`] over this stream and
    /// returns a [`Consumer`] that owns the spawned task.
    ///
    /// Use this when observing a chunk requires `.await` (for example, forwarding chunks to an
    /// async writer or channel). See [`consume`](Self::consume) for the synchronous variant.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Error`] if the backend rejects the consumer creation.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the `Consumer`-internal tokio task, meaning that your visitor is never invoked and the consumer effectively dies immediately. You can safely do a `let _consumer = ...` binding to ignore the typical 'unused' warning."]
    fn consume_async<V>(&self, visitor: V) -> Result<Consumer<V::Output>, Self::Error>
    where
        V: AsyncStreamVisitor,
        Self: OutputStream,
    {
        Ok(spawn_consumer_async(
            self.name(),
            self.try_subscribe()?,
            visitor,
        ))
    }
}

/// Control flag to indicate whether processing should continue or break.
///
/// Returning `Break` from an `Inspector`/`Consumer` will let that instance stop visiting any
/// more data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Interested in receiving additional data.
    Continue,

    /// Not interested in receiving additional data. Will let the `inspector`/`collector` stop.
    Break,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::backend::broadcast::BroadcastOutputStream;
    use crate::output_stream::backend::discard::DiscardedOutputStream;
    use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::config::StreamConfig;
    use crate::output_stream::event::Chunk;
    use crate::output_stream::visitors::inspect::InspectChunks;
    use crate::{ConsumerError, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE};
    use assertr::prelude::*;
    use std::fmt::Debug;
    use tokio::io::AsyncWriteExt;

    /// Generic helper that runs an `InspectChunks` visitor against any [`Consumable`] +
    /// [`OutputStream`] backend and counts the chunks observed before EOF. Compiles only when
    /// `Consumable` carries everything callers need (independent of the concrete backend's
    /// `Error` type), so it pins the trait shape against silent regressions.
    async fn count_chunks<S>(stream: &S) -> Result<usize, ConsumerError>
    where
        S: Consumable + OutputStream,
        S::SubscribeError: Debug,
    {
        use std::sync::{Arc, Mutex};

        let counter = Arc::new(Mutex::new(0_usize));
        let counter_in_visitor = Arc::clone(&counter);
        let consumer = stream
            .consume(
                InspectChunks::builder()
                    .f(move |_chunk: Chunk| {
                        *counter_in_visitor.lock().unwrap() += 1;
                        Next::Continue
                    })
                    .build(),
            )
            .expect("consumer should start");
        consumer.wait().await?;
        Ok(*counter.lock().unwrap())
    }

    #[tokio::test]
    async fn cross_backend_consumable_smoke() {
        let stream_config: StreamConfig<crate::LossyWithoutBackpressure, crate::NoReplay> =
            StreamConfig::builder()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                .build();

        let (broadcast_read, mut broadcast_write) = tokio::io::duplex(64);
        let broadcast = BroadcastOutputStream::from_stream(broadcast_read, "bcast", stream_config);
        broadcast_write.write_all(b"abc").await.unwrap();
        drop(broadcast_write);
        let broadcast_count = count_chunks(&broadcast).await.unwrap();
        assert_that!(broadcast_count).is_greater_or_equal_to(1);

        let (single_read, mut single_write) = tokio::io::duplex(64);
        let single =
            SingleSubscriberOutputStream::from_stream(single_read, "single", stream_config);
        single_write.write_all(b"abc").await.unwrap();
        drop(single_write);
        let single_count = count_chunks(&single).await.unwrap();
        assert_that!(single_count).is_greater_or_equal_to(1);

        let discarded = DiscardedOutputStream::new("discard");
        let discarded_count = count_chunks(&discarded).await.unwrap();
        assert_that!(discarded_count).is_equal_to(0);
    }
}
