use crate::output_stream::OutputStream;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::output_stream::backend::discard::DiscardedOutputStream;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::config::{
    StreamConfig, StreamConfigBuilder, StreamConfigMaxBufferedChunksBuilder,
    StreamConfigReadChunkSizeBuilder, StreamConfigReadyBuilder, StreamConfigReplayBuilder,
};
use crate::output_stream::policy::{
    BestEffortDelivery, Delivery, ReliableDelivery, Replay, ReplayEnabled,
};
use std::marker::PhantomData;
use std::process::Stdio;
use tokio::io::AsyncRead;

mod process_stream_config {
    use super::OutputStream;
    use std::process::Stdio;
    use tokio::io::AsyncRead;

    pub trait Sealed<Stream>
    where
        Stream: OutputStream,
    {
        /// Returns the [`Stdio`] disposition this configuration requires for the matching child
        /// stdio slot. Piped backends return [`Stdio::piped()`]; the discard backend returns
        /// [`Stdio::null()`] so the OS routes the bytes to `/dev/null` without ever crossing into
        /// the parent.
        fn child_stdio(&self) -> Stdio;

        /// Constructs the stream wrapper. `captured` is the parent end of the child's pipe when
        /// `child_stdio()` returned [`Stdio::piped()`]; it is `None` when this configuration set
        /// the child slot to [`Stdio::null()`] (in which case there is nothing to read from).
        fn into_stream<S>(self, captured: Option<S>, stream_name: &'static str) -> Stream
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
    for PipedStreamConfig<BroadcastBackend, StreamConfigReadyBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    fn child_stdio(&self) -> Stdio {
        Stdio::piped()
    }

    fn into_stream<S>(
        self,
        captured: Option<S>,
        stream_name: &'static str,
    ) -> BroadcastOutputStream<D, R>
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        let stream = captured.expect(
            "broadcast backend requires a captured pipe; child_stdio() promised Stdio::piped()",
        );
        BroadcastOutputStream::from_stream(stream, stream_name, self.stage.build())
    }
}

impl<D, R> process_stream_config::Sealed<SingleSubscriberOutputStream<D, R>>
    for PipedStreamConfig<SingleSubscriberBackend, StreamConfigReadyBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    fn child_stdio(&self) -> Stdio {
        Stdio::piped()
    }

    fn into_stream<S>(
        self,
        captured: Option<S>,
        stream_name: &'static str,
    ) -> SingleSubscriberOutputStream<D, R>
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        let stream = captured.expect(
            "single-subscriber backend requires a captured pipe; child_stdio() promised \
             Stdio::piped()",
        );
        SingleSubscriberOutputStream::from_stream(stream, stream_name, self.stage.build())
    }
}

impl process_stream_config::Sealed<DiscardedOutputStream> for DiscardedStreamConfig {
    fn child_stdio(&self) -> Stdio {
        Stdio::null()
    }

    fn into_stream<S>(
        self,
        _captured: Option<S>,
        stream_name: &'static str,
    ) -> DiscardedOutputStream
    where
        S: AsyncRead + Unpin + Send + 'static,
    {
        DiscardedOutputStream::new(stream_name)
    }
}

/// Builder for selecting the output stream backend for one process stream.
///
/// Backend choice controls stream ownership and fanout. Delivery policy and replay policy are
/// selected in later builder stages and are independent decisions. The [`Self::discard`] entry
/// short-circuits the chain entirely for stdio that should be routed to `/dev/null` at the OS
/// level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStreamBuilder;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastBackend;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingleSubscriberBackend;

/// Configuration produced by [`ProcessStreamBuilder::discard`]. The matching child stdio slot is
/// set to [`Stdio::null()`], so the OS discards the bytes; no pipe is allocated and no reader
/// task runs in the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardedStreamConfig;

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipedStreamConfig<Backend, Stage> {
    stage: Stage,
    _backend: PhantomData<Backend>,
}

impl<Backend, Stage> PipedStreamConfig<Backend, Stage> {
    fn new(stage: Stage) -> Self {
        Self {
            stage,
            _backend: PhantomData,
        }
    }
}

impl<Backend> PipedStreamConfig<Backend, StreamConfigBuilder> {
    /// Selects bounded live delivery where slow consumers may observe gaps or dropped output.
    #[must_use]
    pub fn best_effort_delivery(
        self,
    ) -> PipedStreamConfig<Backend, StreamConfigReplayBuilder<BestEffortDelivery>> {
        PipedStreamConfig::new(self.stage.best_effort_delivery())
    }

    /// Selects delivery that waits for active consumers when their buffers are full.
    #[must_use]
    pub fn reliable_for_active_subscribers(
        self,
    ) -> PipedStreamConfig<Backend, StreamConfigReplayBuilder<ReliableDelivery>> {
        PipedStreamConfig::new(self.stage.reliable_for_active_subscribers())
    }
}

impl<Backend, D> PipedStreamConfig<Backend, StreamConfigReplayBuilder<D>>
where
    D: Delivery,
{
    /// Disables replay for future subscribers.
    #[must_use]
    pub fn no_replay(
        self,
    ) -> PipedStreamConfig<Backend, StreamConfigReadChunkSizeBuilder<D, crate::NoReplay>> {
        PipedStreamConfig::new(self.stage.no_replay())
    }

    /// Keeps the latest number of chunks for future subscribers.
    #[must_use]
    pub fn replay_last_chunks(
        self,
        chunks: usize,
    ) -> PipedStreamConfig<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        PipedStreamConfig::new(self.stage.replay_last_chunks(chunks))
    }

    /// Keeps whole chunks covering at least the latest number of bytes.
    #[must_use]
    pub fn replay_last_bytes(
        self,
        bytes: crate::NumBytes,
    ) -> PipedStreamConfig<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        PipedStreamConfig::new(self.stage.replay_last_bytes(bytes))
    }

    /// Keeps all output for the stream lifetime.
    #[must_use]
    pub fn replay_all(
        self,
    ) -> PipedStreamConfig<Backend, StreamConfigReadChunkSizeBuilder<D, ReplayEnabled>> {
        PipedStreamConfig::new(self.stage.replay_all())
    }
}

impl<Backend, D, R> PipedStreamConfig<Backend, StreamConfigReadChunkSizeBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the size of chunks read from the underlying process stream.
    #[must_use]
    pub fn read_chunk_size(
        self,
        read_chunk_size: crate::NumBytes,
    ) -> PipedStreamConfig<Backend, StreamConfigMaxBufferedChunksBuilder<D, R>> {
        PipedStreamConfig::new(self.stage.read_chunk_size(read_chunk_size))
    }
}

impl<Backend, D, R> PipedStreamConfig<Backend, StreamConfigMaxBufferedChunksBuilder<D, R>>
where
    D: Delivery,
    R: Replay,
{
    /// Selects the maximum number of chunks held by the underlying async channel.
    #[must_use]
    pub fn max_buffered_chunks(
        self,
        max_buffered_chunks: usize,
    ) -> PipedStreamConfig<Backend, StreamConfigReadyBuilder<D, R>> {
        PipedStreamConfig::new(self.stage.max_buffered_chunks(max_buffered_chunks))
    }
}

impl ProcessStreamBuilder {
    /// Selects the broadcast backend for this stream.
    ///
    /// Use this when the same stdout or stderr stream must be consumed concurrently, such as
    /// logging plus readiness checks or logging plus collection.
    #[must_use]
    pub fn broadcast(self) -> PipedStreamConfig<BroadcastBackend, StreamConfigBuilder> {
        PipedStreamConfig::new(StreamConfig::builder())
    }

    /// Selects the single-subscriber backend for this stream.
    ///
    /// Use this when exactly one active consumer should own the stream and accidental concurrent
    /// fanout should be rejected early. This backend can reduce coordination overhead in some
    /// single-consumer paths, but delivery policy still determines lag behavior.
    #[must_use]
    pub fn single_subscriber(
        self,
    ) -> PipedStreamConfig<SingleSubscriberBackend, StreamConfigBuilder> {
        PipedStreamConfig::new(StreamConfig::builder())
    }

    /// Routes the matching child stdio slot to [`Stdio::null()`].
    ///
    /// No pipe is allocated, no reader task is spawned, and the resulting stream is a
    /// [`DiscardedOutputStream`] that does not expose any consumer methods. Reach for this when only
    /// the exit status matters and the child's output should be dropped at the OS level.
    #[must_use]
    pub fn discard(self) -> DiscardedStreamConfig {
        DiscardedStreamConfig
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NumBytes, NumBytesExt};
    use assertr::prelude::*;

    mod read_chunk_size {
        use super::*;

        #[test]
        fn panics_on_zero_value() {
            assert_that_panic_by(|| {
                let _config = ProcessStreamBuilder
                    .single_subscriber()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(NumBytes::zero())
                    .max_buffered_chunks(1);
            })
            .has_type::<String>()
            .is_equal_to("read_chunk_size must be greater than zero bytes");
        }
    }

    mod max_buffered_chunks {
        use super::*;

        #[test]
        fn panics_on_zero_for_single_subscriber() {
            assert_that_panic_by(|| {
                let _config = ProcessStreamBuilder
                    .single_subscriber()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(8.bytes())
                    .max_buffered_chunks(0);
            })
            .has_type::<String>()
            .is_equal_to("max_buffered_chunks must be greater than zero");
        }

        #[test]
        fn panics_on_zero_for_broadcast() {
            assert_that_panic_by(|| {
                let _config = ProcessStreamBuilder
                    .broadcast()
                    .best_effort_delivery()
                    .no_replay()
                    .read_chunk_size(8.bytes())
                    .max_buffered_chunks(0);
            })
            .has_type::<String>()
            .is_equal_to("max_buffered_chunks must be greater than zero");
        }
    }
}
