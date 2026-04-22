use crate::output_stream::options::NumBytes;

mod sealed {
    pub trait DeliverySealed {}

    pub trait ReplaySealed {}
}

/// Marker trait implemented by supported stream delivery marker types.
///
/// This trait is sealed. External crates cannot add new delivery marker types.
///
/// ```compile_fail
/// use tokio_process_tools::{
///     DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, DeliveryGuarantee, NoReplay,
///     StreamConfig,
/// };
///
/// let _config: StreamConfig<DeliveryGuarantee, NoReplay> = StreamConfig {
///     read_chunk_size: DEFAULT_READ_CHUNK_SIZE,
///     max_buffered_chunks: DEFAULT_MAX_BUFFERED_CHUNKS,
///     delivery: DeliveryGuarantee::BestEffort,
///     replay: NoReplay,
/// };
/// ```
pub trait Delivery:
    sealed::DeliverySealed + Clone + Copy + std::fmt::Debug + PartialEq + Eq + Send + Sync + 'static
{
    /// Returns the runtime delivery guarantee represented by this marker.
    fn guarantee(self) -> DeliveryGuarantee;
}

/// Best-effort stream delivery marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BestEffortDelivery;

impl sealed::DeliverySealed for BestEffortDelivery {}

impl Delivery for BestEffortDelivery {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::BestEffort
    }
}

/// Reliable active-subscriber stream delivery marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliableDelivery;

impl sealed::DeliverySealed for ReliableDelivery {}

impl Delivery for ReliableDelivery {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::ReliableForActiveSubscribers
    }
}

/// Marker trait implemented by supported stream replay marker types.
///
/// This trait is sealed. External crates cannot add new replay marker types.
///
/// ```compile_fail
/// use tokio_process_tools::{
///     BestEffortDelivery, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, StreamConfig,
/// };
///
/// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// struct CustomReplay;
///
/// let _config: StreamConfig<BestEffortDelivery, CustomReplay> = StreamConfig {
///     read_chunk_size: DEFAULT_READ_CHUNK_SIZE,
///     max_buffered_chunks: DEFAULT_MAX_BUFFERED_CHUNKS,
///     delivery: BestEffortDelivery,
///     replay: CustomReplay,
/// };
/// ```
pub trait Replay:
    sealed::ReplaySealed + Clone + Copy + std::fmt::Debug + PartialEq + Eq + Send + Sync + 'static
{
    /// Returns the replay retention represented by this marker.
    fn replay_retention(self) -> Option<ReplayRetention>;

    /// Returns the sealed-replay behavior represented by this marker.
    fn sealed_replay_behavior(self) -> Option<SealedReplayBehavior>;

    /// Returns whether replay-specific APIs are enabled for this marker.
    fn replay_enabled(self) -> bool;
}

/// Marker for streams without replay support.
///
/// Replay-only methods are not callable for this mode:
///
/// ```compile_fail
/// use tokio_process_tools::{
///     broadcast::BroadcastOutputStream, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
///     StreamConfig,
/// };
///
/// let stream = BroadcastOutputStream::from_stream(
///     tokio::io::empty(),
///     "stdout",
///     StreamConfig::builder()
///         .best_effort_delivery()
///         .no_replay()
///         .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
///         .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
///         .build(),
/// );
/// stream.seal_replay();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoReplay;

impl sealed::ReplaySealed for NoReplay {}

impl Replay for NoReplay {
    fn replay_retention(self) -> Option<ReplayRetention> {
        None
    }

    fn sealed_replay_behavior(self) -> Option<SealedReplayBehavior> {
        None
    }

    fn replay_enabled(self) -> bool {
        false
    }
}

/// Marker for streams with replay support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayEnabled {
    /// Replay history retained for subscribers created after output has already arrived.
    pub replay_retention: ReplayRetention,

    /// How explicit replay-from-start subscriptions behave after replay is sealed.
    pub sealed_replay_behavior: SealedReplayBehavior,
}

impl ReplayEnabled {
    /// Creates a replay-enabled marker with the given retention and sealed-replay behavior.
    #[must_use]
    pub fn new(
        replay_retention: ReplayRetention,
        sealed_replay_behavior: SealedReplayBehavior,
    ) -> Self {
        replay_retention.assert_non_zero("replay_retention");
        Self {
            replay_retention,
            sealed_replay_behavior,
        }
    }
}

impl sealed::ReplaySealed for ReplayEnabled {}

impl Replay for ReplayEnabled {
    fn replay_retention(self) -> Option<ReplayRetention> {
        Some(self.replay_retention)
    }

    fn sealed_replay_behavior(self) -> Option<SealedReplayBehavior> {
        Some(self.sealed_replay_behavior)
    }

    fn replay_enabled(self) -> bool {
        true
    }
}

/// Runtime delivery behavior used by typed stream modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryGuarantee {
    /// Keep reading output and emit gaps to slow subscribers when bounded buffers overflow.
    BestEffort,

    /// Wait for active subscribers before reading more output when bounded buffers are full.
    ReliableForActiveSubscribers,
}

/// Replay history retained by replay-enabled streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRetention {
    /// Keep the latest number of chunks for future subscribers.
    LastChunks(usize),

    /// Keep whole chunks covering at least the latest number of bytes.
    LastBytes(NumBytes),

    /// Keep all output for the stream lifetime.
    ///
    /// This can retain unbounded memory. Use it only when the child process and its output volume
    /// are trusted.
    All,
}

impl ReplayRetention {
    pub(crate) fn assert_non_zero(self, parameter_name: &str) {
        match self {
            ReplayRetention::LastChunks(0) => {
                panic!("{parameter_name} must retain at least one chunk");
            }
            ReplayRetention::LastBytes(bytes) if bytes.bytes() == 0 => {
                panic!("{parameter_name} must retain at least one byte");
            }
            ReplayRetention::LastChunks(_)
            | ReplayRetention::LastBytes(_)
            | ReplayRetention::All => {}
        }
    }
}

/// Explicit replay-from-start subscription behavior after replay history has been sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealedReplayBehavior {
    /// Explicit replay-from-start subscribers created after replay is sealed start at live output.
    StartAtLiveOutput,

    /// Explicit replay-from-start subscribers created after replay is sealed are rejected.
    RejectReplaySubscribers,
}

/// Error returned when an explicit replay subscription cannot be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySubscribeError {
    /// Replay history was sealed and the stream was configured to reject replay subscribers.
    ReplaySealed,

    /// The requested replay start is no longer retained.
    ReplayUnavailable,
}
