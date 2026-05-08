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

/// Lossy stream delivery marker that never applies backpressure to the child.
///
/// **Mechanism:** The reader task keeps draining the child's pipe regardless of consumer pace. When
/// a subscriber's buffer fills, the chunk is dropped for that subscriber rather than pausing the
/// child.
///
/// **Cost:** Slow active consumers may observe gaps or dropped output. Line-aware consumers
/// discard the in-progress partial line and resync at the next newline rather than splicing across
/// the gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossyWithoutBackpressure;

impl sealed::DeliverySealed for LossyWithoutBackpressure {}

impl Delivery for LossyWithoutBackpressure {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::LossyWithoutBackpressure
    }
}

/// Reliable stream delivery marker that applies backpressure to the child to keep active
/// subscribers gap-free.
///
/// **Mechanism:** When an active subscriber's buffer is full, the reader task waits before reading
/// more from the child's pipe. The kernel pipe then fills and the child's next write blocks. This
/// is the cost paid for reliability.
///
/// **Scope:** The guarantee applies only to subscribers that are *currently attached* when each
/// chunk is produced. Subscribers that attach later do not retroactively receive earlier chunks
/// from this delivery policy; that is what the replay axis is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReliableWithBackpressure;

impl sealed::DeliverySealed for ReliableWithBackpressure {}

impl Delivery for ReliableWithBackpressure {
    fn guarantee(self) -> DeliveryGuarantee {
        DeliveryGuarantee::ReliableWithBackpressure
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
///
/// The two variants describe the same trade-off as the typed [`LossyWithoutBackpressure`] and
/// [`ReliableWithBackpressure`] markers. Active subscribers either get every chunk at the cost of
/// pausing the child when their buffer is full, or they tolerate dropped chunks so the child is
/// never blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryGuarantee {
    /// Keep reading output and drop chunks for slow subscribers when bounded buffers overflow.
    /// The child is never blocked by consumer pace.
    LossyWithoutBackpressure,

    /// Wait for active subscribers before reading more output when bounded buffers are full. The
    /// child's next write blocks once the kernel pipe fills. The reliability scope is limited to
    /// subscribers attached at the time each chunk is produced; late attachers depend on the
    /// replay axis for earlier output.
    ReliableWithBackpressure,
}

/// Replay history retained by replay-enabled streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayRetention {
    /// Keep the latest number of chunks for future subscribers.
    LastChunks(usize),

    /// Keep whole chunks covering at least the latest number of bytes.
    ///
    /// Trimming happens at chunk boundaries: chunks are removed from the front of the retained
    /// log only while doing so leaves the remaining size at or above the requested limit. Chunks
    /// are never split, so the actual retained size is "rounded up to whole chunks." A single
    /// chunk that is itself larger than the configured limit is retained in full until a newer
    /// chunk arrives that can replace it; the limit is therefore a soft floor on retention, not
    /// a hard upper bound on memory.
    ///
    /// To make the practical bound predictable, pair this with a `read_chunk_size` smaller than
    /// the retention limit so a single chunk cannot exceed the configured budget.
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
