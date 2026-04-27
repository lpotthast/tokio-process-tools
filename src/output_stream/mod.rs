//! Process output stream types and helpers.

#[macro_use]
pub(crate) mod consumer;

/// Output stream backend implementations.
pub mod backend;

/// Shared stream consumption configuration.
pub mod config;

pub(crate) mod event;
pub(crate) mod line_waiter;

/// Line parsing types and options.
pub mod line;

/// Stream sizing and paired stdout/stderr option types.
pub mod options;

/// Delivery and replay policy types shared by output stream backends.
pub mod policy;

use crate::{
    CollectedBytes, CollectedLines, Collector, LineCollectionOptions, RawCollectionOptions,
};
use event::StreamEvent;
use line::LineParsingOptions;
use options::NumBytes;

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

/// Capability trait for stream backends that can attach built-in collectors.
///
/// [`OutputStream`] intentionally only describes stream metadata. Generic process-handle
/// collection methods need an additional bound for the concrete collector constructors they call,
/// so this trait marks backends that support collecting output into the crate's standard in-memory
/// buffers.
pub trait Collectable: OutputStream {
    /// Starts a collector that parses stream output into lines and stores them in memory.
    fn collect_lines_into_vec(
        &self,
        parsing_options: LineParsingOptions,
        collection_options: LineCollectionOptions,
    ) -> Collector<CollectedLines>;

    /// Starts a collector that stores raw output chunks in memory.
    fn collect_chunks_into_vec(&self, options: RawCollectionOptions) -> Collector<CollectedBytes>;
}

pub(crate) trait Subscription: Send + 'static {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_;
}

pub(crate) trait Subscribable: OutputStream {
    fn subscribe(&self) -> impl Subscription;
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
