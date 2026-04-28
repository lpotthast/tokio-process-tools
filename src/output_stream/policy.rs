use crate::output_stream::num_bytes::NumBytes;

mod sealed {
    pub trait DeliverySealed {}

    pub trait ReplaySealed {}
}

/// Marker trait implemented by supported stream delivery marker types.
pub trait Delivery:
    sealed::DeliverySealed + Clone + Copy + std::fmt::Debug + PartialEq + Eq + Send + Sync + 'static
{
    /// Returns the runtime delivery guarantee represented by this marker.
    fn guarantee(self) -> DeliveryGuarantee;
}

/// Best-effort stream delivery marker.
///
/// Slow active consumers may observe gaps or dropped output when bounded buffers overflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BestEffortDelivery;

impl sealed::DeliverySealed for BestEffortDelivery {}

impl Delivery for BestEffortDelivery {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::BestEffort
    }
}

/// Reliable active-subscriber stream delivery marker.
///
/// Active consumers apply backpressure when their buffers are full. Consumers that attach later
/// still depend on replay settings for earlier output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliableDelivery;

impl sealed::DeliverySealed for ReliableDelivery {}

impl Delivery for ReliableDelivery {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::ReliableForActiveSubscribers
    }
}

/// Marker trait implemented by supported stream replay marker types.
pub trait Replay:
    sealed::ReplaySealed + Clone + Copy + std::fmt::Debug + PartialEq + Eq + Send + Sync + 'static
{
    /// Returns the replay retention represented by this marker.
    fn replay_retention(self) -> Option<ReplayRetention>;

    /// Returns whether replay-specific APIs are enabled for this marker.
    fn replay_enabled(self) -> bool;
}

/// Marker for streams without replay support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoReplay;

impl sealed::ReplaySealed for NoReplay {}

impl Replay for NoReplay {
    fn replay_retention(self) -> Option<ReplayRetention> {
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
}

impl ReplayEnabled {
    /// Creates a replay-enabled marker with the given retention.
    #[must_use]
    pub fn new(replay_retention: ReplayRetention) -> Self {
        replay_retention.assert_non_zero("replay_retention");
        Self { replay_retention }
    }
}

impl sealed::ReplaySealed for ReplayEnabled {}

impl Replay for ReplayEnabled {
    fn replay_retention(self) -> Option<ReplayRetention> {
        Some(self.replay_retention)
    }

    fn replay_enabled(self) -> bool {
        true
    }
}

/// Runtime delivery behavior used by typed stream modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryGuarantee {
    /// Keep reading output and emit gaps or drop output for slow consumers when bounded buffers
    /// overflow.
    BestEffort,

    /// Wait for active consumers before reading more output when bounded buffers are full.
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
