//! Process output stream types and helpers.

/// Output stream backend implementations.
pub mod backend;

/// Bounded in-memory output collection types and options.
pub mod collection;

pub(crate) mod collectable;

/// Shared stream consumption configuration.
pub mod config;

pub(crate) mod consumer;

mod event;
mod line_waiter;

/// Line parsing types and options.
pub mod line;

/// Stream sizing and paired stdout/stderr option types.
pub mod options;

/// Delivery and replay policy types shared by output stream backends.
pub mod policy;

pub(crate) mod subscription;

pub use event::Chunk;

#[allow(unused_imports)]
pub(crate) use backend::broadcast::BroadcastOutputStream;
#[allow(unused_imports)]
pub(crate) use backend::single_subscriber::SingleSubscriberOutputStream;
pub(crate) use collection::{
    CollectedBytes, CollectedLines, LineCollectionOptions, RawCollectionOptions,
    SinkWriteErrorHandler, WriteCollectionOptions,
};
pub(crate) use config::{
    StreamConfig, StreamConfigBuilder, StreamConfigMaxBufferedChunksBuilder,
    StreamConfigReadChunkSizeBuilder, StreamConfigReadyBuilder, StreamConfigReplayBuilder,
};
pub(crate) use event::StreamEvent;
pub(crate) use line::{LineParserState, LineParsingOptions, LineWriteMode};
pub(crate) use options::NumBytes;
pub(crate) use policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, ReliableDelivery, Replay,
    ReplayEnabled, ReplayRetention,
};

/// We support the following implementations:
///
/// - [`BroadcastOutputStream`]
/// - [`SingleSubscriberOutputStream`]
pub trait OutputStream {
    /// The maximum size of every chunk read by the backing `stream_reader`.
    fn read_chunk_size(&self) -> NumBytes;

    /// The number of chunks held by the underlying async channel.
    fn max_buffered_chunks(&self) -> usize;

    /// Type of stream. Can be "stdout" or "stderr". But we do not guarantee this output.
    /// It should only be used for logging/debugging.
    fn name(&self) -> &'static str;
}

/// Control flag to indicate whether processing should continue or break.
///
/// Returning `Break` from an `Inspector`/`Collector` will let that instance stop visiting any
/// more data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Interested in receiving additional data.
    Continue,

    /// Not interested in receiving additional data. Will let the `inspector`/`collector` stop.
    Break,
}
