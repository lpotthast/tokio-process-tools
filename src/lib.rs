//!
#![doc = include_str!("../README.md")]
//!

mod async_drop;
mod error;
mod output_stream;
mod panic_on_drop;
mod process;
mod process_handle;
#[cfg(test)]
mod send_sync_proof;
#[cfg(any(unix, windows))]
mod terminate_on_drop;
#[cfg(test)]
mod test_support;

pub use error::{
    SpawnError, StreamConsumerError, StreamReadError, TerminationAction, TerminationAttemptError,
    TerminationError, WaitError, WaitForCompletionOrTerminateResult, WaitForCompletionResult,
    WaitForLineResult, WaitOrTerminateError, WaitWithOutputError,
};
pub use output_stream::backend::broadcast::{BroadcastOutputStream, BroadcastSubscription};
pub use output_stream::backend::discard::DiscardedOutputStream;
pub use output_stream::backend::single_subscriber::{
    SingleSubscriberOutputStream, SingleSubscriberSubscription,
};
pub use output_stream::config::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, StreamConfig, StreamConfigBuilder,
};
pub use output_stream::consumer::{Consumer, ConsumerCancelOutcome, ConsumerError, Sink};
pub use output_stream::event::{Chunk, StreamEvent};
pub use output_stream::line::adapter::{AsyncLineVisitor, LineVisitor, ParseLines};
pub use output_stream::line::options::{
    DEFAULT_MAX_LINE_LENGTH, LineOverflowBehavior, LineParsingOptions,
};
pub use output_stream::line::parser::LineParser;
pub use output_stream::num_bytes::{NumBytes, NumBytesExt};
pub use output_stream::policy::{
    Delivery, DeliveryGuarantee, LossyWithoutBackpressure, NoReplay, ReliableWithBackpressure,
    Replay, ReplayEnabled, ReplayRetention,
};
pub use output_stream::visitor::{AsyncStreamVisitor, StreamVisitor};
pub use output_stream::visitors;
pub use output_stream::visitors::collect::{
    CollectedBytes, CollectedLines, CollectionOverflowBehavior, LineCollectionOptions,
    RawCollectionOptions,
};
pub use output_stream::visitors::write::{
    LineWriteMode, SinkWriteError, SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation,
    WriteCollectionOptions,
};
pub use output_stream::{Consumable, Next, OutputStream, Subscribable, Subscription};
pub use process::builder::Process;
pub use process::name::{AutoName, AutoNameSettings, ProcessName};
pub use process::stream_config::{
    DiscardedStreamConfig, ProcessStreamBuilder, ProcessStreamConfig,
};
pub use process_handle::output_collection::ProcessOutput;
pub use process_handle::output_collection::options::{
    DEFAULT_OUTPUT_EOF_TIMEOUT, LineOutputOptions, RawOutputOptions,
};
#[cfg(any(unix, windows))]
pub use process_handle::termination::{
    GracefulShutdown, GracefulShutdownBuilder, UnixGracefulPhase, UnixGracefulShutdown,
    UnixGracefulSignal, WindowsGracefulShutdown,
};
pub use process_handle::wait_builder::{WaitForCompletion, state as wait_builder_state};
pub use process_handle::{ProcessHandle, RunningState, Stdin};
#[cfg(any(unix, windows))]
pub use terminate_on_drop::TerminateOnDrop;
