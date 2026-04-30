use crate::StreamReadError;
use bytes::{Buf, Bytes};

/// A "chunk" is an arbitrarily sized byte slice read from the underlying stream.
/// The slices' length is at max of the previously configured maximum `chunk_size`.
///
/// We use the word "chunk", as it is often used when processing collections in segments or when
/// dealing with buffered I/O operations where data arrives in variable-sized pieces.
///
/// In contrast to this, a "frame" typically carries more specific semantics. It usually implies a
/// complete logical unit with defined boundaries within a protocol or format. This we do not have
/// here.
///
/// Note: If the underlying stream is of lower buffer size, chunks of full `chunk_size` length may
/// never be observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chunk(pub(crate) Bytes);

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.0.chunk()
    }
}

impl From<Bytes> for Chunk {
    fn from(bytes: Bytes) -> Self {
        Self(bytes)
    }
}

impl From<&'static [u8]> for Chunk {
    fn from(bytes: &'static [u8]) -> Self {
        Self(Bytes::from_static(bytes))
    }
}

impl<const N: usize> From<&'static [u8; N]> for Chunk {
    fn from(bytes: &'static [u8; N]) -> Self {
        Self(Bytes::from_static(bytes))
    }
}

impl PartialEq<[u8]> for Chunk {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_ref() == other
    }
}

impl PartialEq<&[u8]> for Chunk {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_ref() == *other
    }
}

impl<const N: usize> PartialEq<&[u8; N]> for Chunk {
    fn eq(&self, other: &&[u8; N]) -> bool {
        self.as_ref() == other.as_slice()
    }
}

/// Event emitted by an output stream backend.
///
/// Stream backends send these events to communicate raw process output, deliberate loss of output
/// when a bounded buffer cannot keep up, and the terminal end-of-stream marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// Bytes read from the underlying stdout or stderr stream.
    Chunk(Chunk),

    /// Marker indicating that one or more chunks were skipped.
    ///
    /// This can be emitted when a lossy backend drops buffered output for a slow consumer. Line
    /// parsers use it to discard any partially accumulated line so data from before and after the
    /// gap is not joined into a line that never existed in the source stream.
    Gap,

    /// End of the underlying stdout or stderr stream.
    ///
    /// This is the terminal event for the stream. After it is emitted, no more events are expected.
    Eof,

    /// The underlying stdout or stderr stream failed while being read.
    ///
    /// This is terminal, but distinct from EOF because the stream did not end cleanly.
    ReadError(StreamReadError),
}

impl StreamEvent {
    /// Convenience constructor for [`StreamEvent::Chunk`].
    pub fn chunk(chunk: impl Into<Chunk>) -> Self {
        Self::Chunk(chunk.into())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use assertr::AssertThat;
    use assertr::actual::Actual;
    use assertr::mode::Panic;
    use tokio::sync::mpsc;

    pub(crate) async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        rx
    }

    pub(crate) trait StreamEventAssertions<'t> {
        /// Assert this event is the [`StreamEvent::Chunk`] variant and continue the chain
        /// against the contained [`Chunk`].
        #[allow(clippy::wrong_self_convention)]
        fn is_chunk(self) -> AssertThat<'t, Chunk, Panic>;
    }

    impl<'t> StreamEventAssertions<'t> for AssertThat<'t, StreamEvent, Panic> {
        #[track_caller]
        fn is_chunk(self) -> AssertThat<'t, Chunk, Panic> {
            if !matches!(self.actual(), StreamEvent::Chunk(_)) {
                let actual = self.actual();
                self.fail(format_args!(
                    "Actual: {actual:#?}\n\nis not of expected variant: StreamEvent::Chunk"
                ));
            }

            self.map(|actual| match actual {
                Actual::Owned(StreamEvent::Chunk(chunk)) => Actual::Owned(chunk),
                Actual::Borrowed(StreamEvent::Chunk(chunk)) => Actual::Borrowed(chunk),
                _ => unreachable!("variant verified above"),
            })
        }
    }
}
