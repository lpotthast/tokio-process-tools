use crate::NumBytes;
use crate::output_stream::options::assert_max_buffered_chunks_non_zero;
use crate::output_stream::policy::{
    BestEffortDelivery, Delivery, DeliveryGuarantee, NoReplay, ReliableDelivery, Replay,
    ReplayEnabled, ReplayRetention, SealedReplayBehavior,
};

/// Shared output stream configuration for all stream backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig<D = BestEffortDelivery, R = NoReplay>
where
    D: Delivery,
    R: Replay,
{
    /// The size of an individual chunk read from the underlying process stream.
    ///
    /// Must be greater than zero. The default is [`crate::DEFAULT_READ_CHUNK_SIZE`].
    pub read_chunk_size: NumBytes,

    /// The number of chunks held by the underlying async channel.
    ///
    /// Must be greater than zero. The default is [`crate::DEFAULT_MAX_BUFFERED_CHUNKS`].
    /// With [`DeliveryGuarantee::ReliableForActiveSubscribers`], it is the maximum unread chunk
    /// lag an active subscriber can have before reading waits.
    pub max_buffered_chunks: usize,

    /// How slow active subscribers affect reading from the underlying stream.
    pub delivery: D,

    /// Whether and how replay history is retained for subscribers that attach after output arrives.
    pub replay: R,
}

impl StreamConfig<BestEffortDelivery, NoReplay> {
    /// Starts building an output stream configuration.
    ///
    /// The builder requires explicit delivery, replay, read chunk size, and maximum buffered chunk
    /// count before a [`StreamConfig`] can be built.
    ///
    /// ```compile_fail
    /// use tokio_process_tools::StreamConfig;
    ///
    /// let _config = StreamConfig::builder()
    ///     .best_effort_delivery()
    ///     .no_replay()
    ///     .build();
    /// ```
    ///
    /// ```compile_fail
    /// use tokio_process_tools::{DEFAULT_READ_CHUNK_SIZE, StreamConfig};
    ///
    /// let _config = StreamConfig::builder()
    ///     .best_effort_delivery()
    ///     .replay_last_chunks(1)
    ///     .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder() -> StreamConfigBuilder {
        StreamConfigBuilder
    }
}

/// Initial builder stage that requires selecting delivery behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigBuilder;

impl StreamConfigBuilder {
    /// Delivery that lets slow subscribers lag behind, not providing them with all possibly
    /// observable data.
    #[must_use]
    pub fn best_effort_delivery(self) -> StreamConfigReplayBuilder<BestEffortDelivery> {
        StreamConfigReplayBuilder {
            delivery: BestEffortDelivery,
        }
    }

    /// Delivery that waits for active subscribers when they lag behind.
    ///
    /// This does not guarantee full reliability in the terms of "definitely receiving all events".
    /// That is controlled by the upcoming "replay" settings. This setting here only guarantees
    /// that registered and listening consumers will see all events.
    #[must_use]
    pub fn reliable_for_active_subscribers(self) -> StreamConfigReplayBuilder<ReliableDelivery> {
        StreamConfigReplayBuilder {
            delivery: ReliableDelivery,
        }
    }
}

/// Builder stage that requires selecting replay behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigReplayBuilder<D>
where
    D: Delivery,
{
    delivery: D,
}

impl<D> StreamConfigReplayBuilder<D>
where
    D: Delivery,
{
    /// Disables replay for future subscribers.
    ///
    /// Consumers that attach after output has already arrived start at live output.
    #[must_use]
    pub fn no_replay(self) -> StreamConfigReadChunkSizeBuilder<D, NoReplay> {
        StreamConfigReadChunkSizeBuilder {
            delivery: self.delivery,
            replay: NoReplay,
        }
    }

    /// Keeps the latest number of chunks for future subscribers.
    #[must_use]
    pub fn replay_last_chunks(self, chunks: usize) -> StreamConfigSealedReplayBehaviorBuilder<D> {
        let replay_retention = ReplayRetention::LastChunks(chunks);
        replay_retention.assert_non_zero("chunks");
        StreamConfigSealedReplayBehaviorBuilder {
            delivery: self.delivery,
            replay_retention,
        }
    }

    /// Keeps whole chunks covering at least the latest number of bytes.
    #[must_use]
    pub fn replay_last_bytes(self, bytes: NumBytes) -> StreamConfigSealedReplayBehaviorBuilder<D> {
        let replay_retention = ReplayRetention::LastBytes(bytes);
        replay_retention.assert_non_zero("bytes");
        StreamConfigSealedReplayBehaviorBuilder {
            delivery: self.delivery,
            replay_retention,
        }
    }

    /// Retains all output of the stream.
    ///
    /// This can potentially grow massively and could require a lot of memory.
    ///
    /// Make sure to call `seal_replay()` on the stream when all subscribers were created. This
    /// allows the system to free up memory for data already replayed to all subscribers.
    #[must_use]
    pub fn replay_all(self) -> StreamConfigSealedReplayBehaviorBuilder<D> {
        StreamConfigSealedReplayBehaviorBuilder {
            delivery: self.delivery,
            replay_retention: ReplayRetention::All,
        }
    }
}

/// Builder stage that requires selecting sealed-replay behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigSealedReplayBehaviorBuilder<D>
where
    D: Delivery,
{
    delivery: D,
    replay_retention: ReplayRetention,
}

impl<D> StreamConfigSealedReplayBehaviorBuilder<D>
where
    D: Delivery,
{
    /// Selects how explicit replay-from-start subscriptions behave after replay has been sealed.
    #[must_use]
    pub fn sealed_replay_behavior(
        self,
        sealed_replay_behavior: SealedReplayBehavior,
    ) -> StreamConfigReadChunkSizeBuilder<D, ReplayEnabled> {
        StreamConfigReadChunkSizeBuilder {
            delivery: self.delivery,
            replay: ReplayEnabled::new(self.replay_retention, sealed_replay_behavior),
        }
    }
}

/// Builder stage that requires selecting the read chunk size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigReadChunkSizeBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    delivery: D,
    replay: R,
}

impl<D, R> StreamConfigReadChunkSizeBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the size of chunks read from the underlying process stream.
    ///
    /// # Panics
    ///
    /// Panics if `read_chunk_size` is zero bytes.
    #[must_use]
    pub fn read_chunk_size(
        self,
        read_chunk_size: NumBytes,
    ) -> StreamConfigMaxBufferedChunksBuilder<D, R> {
        read_chunk_size.assert_non_zero("read_chunk_size");
        StreamConfigMaxBufferedChunksBuilder {
            delivery: self.delivery,
            replay: self.replay,
            read_chunk_size,
        }
    }
}

/// Builder stage that requires selecting the maximum number of buffered chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigMaxBufferedChunksBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    delivery: D,
    replay: R,
    read_chunk_size: NumBytes,
}

impl<D, R> StreamConfigMaxBufferedChunksBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the number of chunks held by the underlying async channel.
    ///
    /// # Panics
    ///
    /// Panics if `max_buffered_chunks` is zero.
    #[must_use]
    pub fn max_buffered_chunks(self, max_buffered_chunks: usize) -> StreamConfigReadyBuilder<D, R> {
        assert_max_buffered_chunks_non_zero(max_buffered_chunks, "max_buffered_chunks");
        StreamConfigReadyBuilder {
            config: StreamConfig {
                read_chunk_size: self.read_chunk_size,
                max_buffered_chunks,
                delivery: self.delivery,
                replay: self.replay,
            },
        }
    }
}

/// Final builder stage for [`StreamConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfigReadyBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    config: StreamConfig<D, R>,
}

impl<D, R> StreamConfigReadyBuilder<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Builds the configured stream mode.
    #[must_use]
    pub fn build(self) -> StreamConfig<D, R> {
        self.config
    }
}

impl<D, R> StreamConfig<D, R>
where
    D: Delivery,
    R: Replay,
{
    /// Returns the runtime delivery guarantee represented by this configuration.
    #[must_use]
    pub fn delivery_guarantee(self) -> DeliveryGuarantee {
        self.delivery.guarantee()
    }

    /// Returns the replay retention represented by this configuration.
    #[must_use]
    pub fn replay_retention(self) -> Option<ReplayRetention> {
        self.replay.replay_retention()
    }

    /// Returns the sealed-replay behavior represented by this configuration.
    #[must_use]
    pub fn sealed_replay_behavior(self) -> Option<SealedReplayBehavior> {
        self.replay.sealed_replay_behavior()
    }

    /// Returns whether this configuration enables replay-specific APIs.
    #[must_use]
    pub fn replay_enabled(self) -> bool {
        self.replay.replay_enabled()
    }

    pub(crate) fn assert_valid(self, parameter_name: &str) {
        self.read_chunk_size
            .assert_non_zero(&format!("{parameter_name}.read_chunk_size"));
        assert_max_buffered_chunks_non_zero(
            self.max_buffered_chunks,
            &format!("{parameter_name}.max_buffered_chunks"),
        );
        if let Some(replay_retention) = self.replay_retention() {
            replay_retention.assert_non_zero(&format!("{parameter_name}.replay_retention"));
        }
    }
}

impl<D> StreamConfig<D, ReplayEnabled>
where
    D: Delivery,
{
    /// Returns this replay-enabled configuration with custom replay retention.
    #[must_use]
    pub fn with_replay_retention(mut self, replay_retention: ReplayRetention) -> Self {
        replay_retention.assert_non_zero("replay_retention");
        self.replay.replay_retention = replay_retention;
        self
    }

    /// Returns this replay-enabled configuration with custom sealed-replay behavior.
    #[must_use]
    pub fn with_sealed_replay_behavior(
        mut self,
        sealed_replay_behavior: SealedReplayBehavior,
    ) -> Self {
        self.replay.sealed_replay_behavior = sealed_replay_behavior;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::options::NumBytesExt;
    use crate::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE};
    use assertr::prelude::*;

    fn default_builder<D, R>(builder: StreamConfigReadChunkSizeBuilder<D, R>) -> StreamConfig<D, R>
    where
        D: Delivery,
        R: Replay,
    {
        builder
            .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
            .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            .build()
    }

    #[test]
    fn builder_creates_best_effort_no_replay_config() {
        let config: StreamConfig<BestEffortDelivery, NoReplay> =
            default_builder(StreamConfig::builder().best_effort_delivery().no_replay());

        assert_that!(config.delivery_guarantee()).is_equal_to(DeliveryGuarantee::BestEffort);
        assert_that!(config.replay_enabled()).is_false();
        assert_that!(config.replay_retention()).is_none();
        assert_that!(config.sealed_replay_behavior()).is_none();
        assert_that!(config.read_chunk_size).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(config.max_buffered_chunks).is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
    }

    #[test]
    fn builder_creates_reliable_no_replay_config() {
        let config: StreamConfig<ReliableDelivery, NoReplay> = default_builder(
            StreamConfig::builder()
                .reliable_for_active_subscribers()
                .no_replay(),
        );

        assert_that!(config.delivery_guarantee())
            .is_equal_to(DeliveryGuarantee::ReliableForActiveSubscribers);
        assert_that!(config.replay_enabled()).is_false();
        assert_that!(config.replay_retention()).is_none();
        assert_that!(config.sealed_replay_behavior()).is_none();
    }

    #[test]
    fn builder_creates_best_effort_replay_config() {
        let config: StreamConfig<BestEffortDelivery, ReplayEnabled> = default_builder(
            StreamConfig::builder()
                .best_effort_delivery()
                .replay_last_chunks(2)
                .sealed_replay_behavior(SealedReplayBehavior::RejectReplaySubscribers),
        );

        assert_that!(config.delivery_guarantee()).is_equal_to(DeliveryGuarantee::BestEffort);
        assert_that!(config.replay_retention()).is_equal_to(Some(ReplayRetention::LastChunks(2)));
        assert_that!(config.sealed_replay_behavior())
            .is_equal_to(Some(SealedReplayBehavior::RejectReplaySubscribers));
    }

    #[test]
    fn builder_creates_reliable_replay_config() {
        let config: StreamConfig<ReliableDelivery, ReplayEnabled> = default_builder(
            StreamConfig::builder()
                .reliable_for_active_subscribers()
                .replay_last_bytes(16.bytes())
                .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput),
        );

        assert_that!(config.delivery_guarantee())
            .is_equal_to(DeliveryGuarantee::ReliableForActiveSubscribers);
        assert_that!(config.replay_retention())
            .is_equal_to(Some(ReplayRetention::LastBytes(16.bytes())));
    }

    #[test]
    #[should_panic(expected = "read_chunk_size must be greater than zero bytes")]
    fn builder_rejects_zero_read_chunk_size() {
        let _config = StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(0.bytes());
    }

    #[test]
    #[should_panic(expected = "max_buffered_chunks must be greater than zero")]
    fn builder_rejects_zero_max_buffered_chunks() {
        let _config = StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(8.bytes())
            .max_buffered_chunks(0);
    }

    #[test]
    #[should_panic(expected = "chunks must retain at least one chunk")]
    fn builder_rejects_zero_replay_chunks() {
        let _config = StreamConfig::builder()
            .best_effort_delivery()
            .replay_last_chunks(0);
    }

    #[test]
    #[should_panic(expected = "bytes must retain at least one byte")]
    fn builder_rejects_zero_replay_bytes() {
        let _config = StreamConfig::builder()
            .best_effort_delivery()
            .replay_last_bytes(NumBytes::zero());
    }

    #[test]
    #[should_panic(expected = "replay_retention must retain at least one chunk")]
    fn replay_enabled_rejects_zero_replay_retention() {
        let _replay = ReplayEnabled::new(
            ReplayRetention::LastChunks(0),
            SealedReplayBehavior::StartAtLiveOutput,
        );
    }

    #[test]
    #[should_panic(expected = "replay_retention must retain at least one byte")]
    fn with_replay_retention_rejects_zero_replay_retention() {
        let config = StreamConfig::builder()
            .best_effort_delivery()
            .replay_all()
            .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
            .read_chunk_size(8.bytes())
            .max_buffered_chunks(2)
            .build();

        let _config = config.with_replay_retention(ReplayRetention::LastBytes(NumBytes::zero()));
    }

    #[test]
    #[should_panic(expected = "options.replay_retention must retain at least one byte")]
    fn config_validation_rejects_zero_replay_retention() {
        let config = StreamConfig {
            read_chunk_size: 8.bytes(),
            max_buffered_chunks: 2,
            delivery: BestEffortDelivery,
            replay: ReplayEnabled {
                replay_retention: ReplayRetention::LastBytes(NumBytes::zero()),
                sealed_replay_behavior: SealedReplayBehavior::StartAtLiveOutput,
            },
        };

        config.assert_valid("options");
    }

    #[tokio::test]
    async fn one_config_constructs_both_stream_backends() {
        use crate::OutputStream;
        use crate::output_stream::backend::broadcast::BroadcastOutputStream;
        use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;

        let config = StreamConfig::builder()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(8.bytes())
            .max_buffered_chunks(2)
            .build();

        let broadcast = BroadcastOutputStream::from_stream(tokio::io::empty(), "stdout", config);
        let single_subscriber =
            SingleSubscriberOutputStream::from_stream(tokio::io::empty(), "stderr", config);

        assert_that!(broadcast.read_chunk_size()).is_equal_to(8.bytes());
        assert_that!(single_subscriber.read_chunk_size()).is_equal_to(8.bytes());
        assert_that!(broadcast.max_buffered_chunks()).is_equal_to(2);
        assert_that!(single_subscriber.max_buffered_chunks()).is_equal_to(2);
    }
}
