use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::{
    BestEffortDelivery, Delivery, OutputStream, ReliableDelivery, Replay, ReplayEnabled,
    StreamConfig, StreamConfigBuilder, StreamConfigMaxBufferedChunksBuilder,
    StreamConfigReadChunkSizeBuilder, StreamConfigReadyBuilder, StreamConfigReplayBuilder,
};
use std::marker::PhantomData;
use tokio::io::AsyncRead;

mod process_stream_config {
    use super::OutputStream;
    use tokio::io::AsyncRead;

    pub trait Sealed<Stream>
    where
        Stream: OutputStream,
    {
        fn into_stream<S>(self, stream: S, stream_name: &'static str) -> Stream
        where
            S: AsyncRead + Unpin + Send + 'static;
    }
}

/// Marker trait for process stream builder configurations.
///
/// This trait is sealed. External crates cannot implement additional process stream
/// configuration types; use [`ProcessStreamBuilder`] to select one of the supported backends.
pub trait ProcessStreamConfig<Stream>: process_stream_config::Sealed<Stream>
where
    Stream: OutputStream,
{
}

impl<Config, Stream> ProcessStreamConfig<Stream> for Config
where
    Config: process_stream_config::Sealed<Stream>,
    Stream: OutputStream,
{
}

impl<D, R> process_stream_config::Sealed<BroadcastOutputStream<D, R>>
    for ProcessStreamConfigStage<BroadcastBackend, StreamConfigReadyBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    fn into_stream<S>(self, stream: S, stream_name: &'static str) -> BroadcastOutputStream<D, R>
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        BroadcastOutputStream::from_stream(stream, stream_name, self.stage.build())
    }
}

impl<D, R> process_stream_config::Sealed<SingleSubscriberOutputStream>
    for ProcessStreamConfigStage<SingleSubscriberBackend, StreamConfigReadyBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    fn into_stream<S>(self, stream: S, stream_name: &'static str) -> SingleSubscriberOutputStream
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        SingleSubscriberOutputStream::from_stream(stream, stream_name, self.stage.build())
    }
}

/// Builder for selecting the output stream backend for one process stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStreamBuilder;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastBackend;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleSubscriberBackend;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStreamConfigStage<Backend, Stage> {
    stage: Stage,
    _backend: PhantomData<fn() -> Backend>,
}

impl<Backend, Stage> ProcessStreamConfigStage<Backend, Stage> {
    fn new(stage: Stage) -> Self {
        Self {
            stage,
            _backend: PhantomData,
        }
    }
}

impl<Backend> ProcessStreamConfigStage<Backend, StreamConfigBuilder> {
    /// Selects best-effort delivery.
    #[must_use]
    pub fn best_effort_delivery(
        self,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReplayBuilder<BestEffortDelivery>> {
        ProcessStreamConfigStage::new(self.stage.best_effort_delivery())
    }

    /// Selects delivery that waits for active subscribers when they lag behind.
    #[must_use]
    pub fn reliable_for_active_subscribers(
        self,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReplayBuilder<ReliableDelivery>> {
        ProcessStreamConfigStage::new(self.stage.reliable_for_active_subscribers())
    }
}

impl<Backend, D> ProcessStreamConfigStage<Backend, StreamConfigReplayBuilder<D>>
where
    D: Delivery,
{
    /// Disables replay for future subscribers.
    #[must_use]
    pub fn no_replay(
        self,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReadChunkSizeBuilder<D, crate::NoReplay>>
    {
        ProcessStreamConfigStage::new(self.stage.no_replay())
    }

    /// Keeps the latest number of chunks for future subscribers.
    #[must_use]
    pub fn replay_last_chunks(
        self,
        chunks: usize,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        ProcessStreamConfigStage::new(self.stage.replay_last_chunks(chunks))
    }

    /// Keeps whole chunks covering at least the latest number of bytes.
    #[must_use]
    pub fn replay_last_bytes(
        self,
        bytes: crate::NumBytes,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        ProcessStreamConfigStage::new(self.stage.replay_last_bytes(bytes))
    }

    /// Keeps all output for the stream lifetime.
    #[must_use]
    pub fn replay_all(
        self,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        ProcessStreamConfigStage::new(self.stage.replay_all())
    }
}

impl<Backend, D, R> ProcessStreamConfigStage<Backend, StreamConfigReadChunkSizeBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the size of chunks read from the underlying process stream.
    #[must_use]
    pub fn read_chunk_size(
        self,
        read_chunk_size: crate::NumBytes,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigMaxBufferedChunksBuilder<D, R>> {
        ProcessStreamConfigStage::new(self.stage.read_chunk_size(read_chunk_size))
    }
}

impl<Backend, D, R> ProcessStreamConfigStage<Backend, StreamConfigMaxBufferedChunksBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the maximum number of chunks held by the underlying async channel.
    #[must_use]
    pub fn max_buffered_chunks(
        self,
        max_buffered_chunks: usize,
    ) -> ProcessStreamConfigStage<Backend, StreamConfigReadyBuilder<D, R>> {
        ProcessStreamConfigStage::new(self.stage.max_buffered_chunks(max_buffered_chunks))
    }
}

impl<Backend, D, R> ProcessStreamConfigStage<Backend, StreamConfigReadyBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    /// Builds the shared stream configuration payload.
    #[must_use]
    pub fn build(self) -> StreamConfig<D, R> {
        self.stage.build()
    }
}

impl ProcessStreamBuilder {
    /// Selects the broadcast backend for this stream.
    #[must_use]
    pub fn broadcast(self) -> ProcessStreamConfigStage<BroadcastBackend, StreamConfigBuilder> {
        ProcessStreamConfigStage::new(StreamConfig::builder())
    }

    /// Selects the single-subscriber backend for this stream.
    #[must_use]
    pub fn single_subscriber(
        self,
    ) -> ProcessStreamConfigStage<SingleSubscriberBackend, StreamConfigBuilder> {
        ProcessStreamConfigStage::new(StreamConfig::builder())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NumBytes, NumBytesExt};

    #[test]
    #[should_panic(expected = "read_chunk_size must be greater than zero bytes")]
    fn process_stream_builder_panics_on_zero_read_chunk_size() {
        let _config = ProcessStreamBuilder
            .single_subscriber()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(NumBytes::zero())
            .max_buffered_chunks(1);
    }

    #[test]
    #[should_panic(expected = "max_buffered_chunks must be greater than zero")]
    fn single_subscriber_process_stream_builder_panics_on_zero_max_buffered_chunks() {
        let _config = ProcessStreamBuilder
            .single_subscriber()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(8.bytes())
            .max_buffered_chunks(0);
    }

    #[test]
    #[should_panic(expected = "max_buffered_chunks must be greater than zero")]
    fn broadcast_process_stream_builder_panics_on_zero_max_buffered_chunks() {
        let _config = ProcessStreamBuilder
            .broadcast()
            .best_effort_delivery()
            .no_replay()
            .read_chunk_size(8.bytes())
            .max_buffered_chunks(0);
    }
}
