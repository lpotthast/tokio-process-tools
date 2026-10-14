use crate::StreamReadError;
use bytes::{Buf, Bytes};

/// An arbitrarily sized slice of bytes read from the underlying stream.
///
/// A chunk is never longer than the `read_chunk_size` configured for the stream, but it may well
/// be shorter. If the underlying stream buffers less data than that, full-length chunks may never
/// be observed.
///
/// The term "chunk" is deliberate: unlike a "frame", it carries no protocol semantics. Chunk
/// boundaries are an artifact of buffered reading and can fall anywhere, including in the middle
/// of a line or a multi-byte character.
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
    use assertr::actual::Actual;
    use assertr::borrow_for::BorrowFor;
    use assertr::prelude::*;
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
            self.is_matching(pattern!(StreamEvent::Chunk(_)))
                .map(|actual| match actual {
                    Actual::Owned(StreamEvent::Chunk(chunk)) => Actual::Owned(chunk),
                    Actual::Borrowed(StreamEvent::Chunk(chunk)) => Actual::Borrowed(chunk),
                    _ => unreachable!("variant verified above"),
                })
        }
    }

    // Let byte literals serve as expected values for `Chunk` subjects, as in
    // `assert_that!(event).is_chunk().is_equal_to(b"ab")`.
    impl<'a, const N: usize> BorrowFor<Chunk> for &'a [u8; N] {
        type View = &'a [u8; N];
    }

    impl BorrowFor<Chunk> for &[u8] {
        type View = [u8];
    }
}
