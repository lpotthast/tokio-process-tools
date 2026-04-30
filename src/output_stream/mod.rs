//! Process output stream types and helpers.
//!
//! The submodules below correspond to the four conceptual layers this subsystem is built from:
//!
//! - **Core abstractions** — the [`OutputStream`] / [`Subscription`] / [`TrySubscribable`] /
//!   [`Next`] traits defined here, [`event`]'s [`Chunk`](event::Chunk) /
//!   [`StreamEvent`](event::StreamEvent), the [`policy`] / [`config`] / [`num_bytes`] / [`line`]
//!   modules, and the [`visitor`] trait pair every visitor implementation builds against. These
//!   files have no tokio dependency.
//! - **Tokio runtime adapter** ([`consumer`]) — the [`Consumer<S>`](consumer::Consumer) handle
//!   plus the driver loops that step a visitor over a subscription on a tokio task with
//!   cooperative-cancel / abort semantics.
//! - **Tokio stream backends** ([`backend`]) — `broadcast` and `single_subscriber`, which ingest
//!   any [`tokio::io::AsyncRead`] and emit [`StreamEvent`](event::StreamEvent)s.
//! - **User-replaceable convenience layer** ([`visitors`]) — the built-in `collect`, `inspect`,
//!   `wait`, and `write` visitors plus the `inspect_*` / `collect_*` factory macro that wires
//!   them as inherent methods on each backend. `consume_with(my_visitor)` is enough to use the
//!   library; everything in this module is sugar for the common cases.

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

/// Visitor traits — the runtime-agnostic contract every stream observer implements.
pub mod visitor;

/// Built-in visitors and the convenience factory macro that instantiates them.
pub(crate) mod visitors;

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
