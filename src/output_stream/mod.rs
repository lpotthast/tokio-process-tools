//! Process output stream types and helpers.

pub(crate) mod consumer;

/// Output stream backend implementations.
pub mod backend;

/// Shared stream consumption configuration.
pub mod config;

pub(crate) mod event;

/// Line parsing types and options.
pub mod line;

/// `NumBytes` newtype and convenience constructors used throughout the public API.
pub mod num_bytes;

/// Delivery and replay policy types shared by output stream backends.
pub mod policy;

use crate::StreamConsumerError;
use event::StreamEvent;
use num_bytes::NumBytes;

/// We support the following implementations:
///
/// - [`crate::BroadcastOutputStream`]
/// - [`crate::SingleSubscriberOutputStream`]
pub trait OutputStream {
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
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_;
}

/// Output stream backend that can reject consumer subscriptions.
pub trait TrySubscribable: OutputStream {
    /// Creates a new subscription for a consumer, or returns why the consumer cannot be started.
    fn try_subscribe(&self) -> Result<impl Subscription, StreamConsumerError>;
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
