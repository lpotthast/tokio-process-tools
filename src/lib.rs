//!
#![doc = include_str!("../README.md")]
//!

mod async_drop;
mod error;
mod output_stream;
mod panic_on_drop;
mod process;
mod process_handle;
mod signal;
mod terminate_on_drop;
#[cfg(test)]
mod send_sync_proof;
#[cfg(test)]
mod test_support;

pub use error::{
    SpawnError, StreamConsumerError, StreamReadError, TerminationAttemptError,
    TerminationAttemptOperation, TerminationAttemptPhase, TerminationError, WaitError,
    WaitForCompletionOrTerminateResult, WaitForCompletionResult, WaitForLineResult,
    WaitOrTerminateError, WaitWithOutputError,
};
pub use output_stream::backend::broadcast::BroadcastOutputStream;
pub use output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
pub use output_stream::config::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, StreamConfig, StreamConfigBuilder,
};
pub use output_stream::consumer::collect::{
    AsyncChunkCollector, AsyncLineCollector, CollectedBytes, CollectedLines,
    CollectionOverflowBehavior, Collector, CollectorCancelOutcome, CollectorError,
    LineCollectionOptions, RawCollectionOptions, Sink,
};
pub use output_stream::consumer::inspect::{Inspector, InspectorCancelOutcome, InspectorError};
pub use output_stream::consumer::line_waiter::LineWaiter;
pub use output_stream::consumer::write::{
    LineWriteMode, SinkWriteError, SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation,
    WriteCollectionOptions,
};
pub use output_stream::event::Chunk;
pub use output_stream::line::{LineOverflowBehavior, LineParsingOptions};
pub use output_stream::num_bytes::{NumBytes, NumBytesExt};
pub use output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, ReliableDelivery, Replay,
    ReplayEnabled, ReplayRetention,
};
pub use output_stream::{Next, OutputStream};
pub use process::builder::Process;
pub use process::name::{AutoName, AutoNameSettings, ProcessName};
pub use process::stream_config::{ProcessStreamBuilder, ProcessStreamConfig};
pub use process_handle::options::WaitForCompletionOrTerminateOptions;
pub use process_handle::output_collection::options::{LineOutputOptions, RawOutputOptions};
pub use process_handle::output_collection::output::ProcessOutput;
pub use process_handle::{ProcessHandle, RunningState, Stdin};
pub use terminate_on_drop::TerminateOnDrop;
