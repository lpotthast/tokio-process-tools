//!
#![doc = include_str!("../README.md")]
//!

mod async_drop;
mod collector;
mod error;
mod inspector;
mod output;
mod output_stream;
mod panic_on_drop;
mod process;
mod process_handle;
mod signal;
mod terminate_on_drop;
#[cfg(test)]
mod test_support;

pub use collector::{
    AsyncChunkCollector, AsyncLineCollector, Collector, CollectorCancelOutcome, CollectorError,
    Sink,
};
pub use error::{
    SpawnError, StreamReadError, TerminationAttemptError, TerminationAttemptOperation,
    TerminationAttemptPhase, TerminationError, WaitError, WaitForCompletionWithOutputError,
    WaitForCompletionWithOutputOrTerminateError, WaitForLineResult, WaitOrTerminateError,
};
pub use inspector::{Inspector, InspectorCancelOutcome, InspectorError};
pub use output::ProcessOutput;
pub use output_stream::backend::{broadcast, single_subscriber};
pub use output_stream::collection::{
    CollectedBytes, CollectedLines, CollectionOverflowBehavior, FailOnSinkWriteError,
    LineCollectionOptions, LogAndContinueSinkWriteErrors, RawCollectionOptions, SinkWriteError,
    SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation, WriteCollectionOptions,
};
pub use output_stream::config::{
    StreamConfig, StreamConfigBuilder, StreamConfigMaxBufferedChunksBuilder,
    StreamConfigReadChunkSizeBuilder, StreamConfigReadyBuilder, StreamConfigReplayBuilder,
};
pub use output_stream::line::{LineOverflowBehavior, LineParsingOptions, LineWriteMode};
pub use output_stream::options::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytes, NumBytesExt,
};
pub use output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, ReliableDelivery, Replay,
    ReplayEnabled, ReplayRetention,
};
pub use output_stream::{Chunk, Next, OutputStream};
pub use process::{
    AutoName, AutoNameSettings, ConfiguredProcessBuilder, NamedProcess, Process,
    ProcessBuilderWithStderr, ProcessBuilderWithStdout, ProcessName, ProcessStreamBuilder,
    ProcessStreamConfig,
};
pub use process_handle::{
    LineOutputOptions, ProcessHandle, RawOutputOptions, RunningState, Stdin,
    WaitForCompletionOrTerminateOptions,
};
pub use terminate_on_drop::TerminateOnDrop;

/// Private compile-time assertion that stream types and the public `ProcessHandle` remain
/// `Send + Sync`.
///
/// `Send` matters because users should be able to move a `ProcessHandle` into a spawned task.
///
/// `Sync` matters because output streams are accessed through shared references from
/// `ProcessHandle::stdout()` / `ProcessHandle::stderr()`.
/// Broadcast streams are explicitly multi-consumer. Single-subscriber streams still use interior
/// synchronization so concurrent attempts can be safely rejected rather than becoming unsound.
///
/// These assertion mainly protect an API guarantee: future internal changes must not accidentally
/// add something like `Rc`, `RefCell`, or another non-thread-safe field that would make
/// handles/streams awkward or impossible to use in normal async task patterns. It is not strictly required for
/// every local use case, but it is a sensible contract for this crate.
#[allow(dead_code)] // Never really used.
trait SendSync: Send + Sync {}

impl SendSync for single_subscriber::SingleSubscriberOutputStream {}

impl<D, R> SendSync for broadcast::BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
}

impl<Stdout, Stderr> SendSync for ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream + SendSync,
    Stderr: OutputStream + SendSync,
{
}
