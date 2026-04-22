use crate::StreamReadError;
use bytes::Buf;

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
pub struct Chunk(pub(crate) bytes::Bytes);

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.0.chunk()
    }
}

/// Event emitted by an output stream backend.
///
/// Stream backends send these events to communicate raw process output, deliberate loss of output
/// when a bounded buffer cannot keep up, and the terminal end-of-stream marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamEvent {
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
