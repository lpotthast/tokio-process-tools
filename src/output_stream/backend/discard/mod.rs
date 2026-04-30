//! Discard backend: a zero-cost stream marker for stdio configured as `Stdio::null()`.
//!
//! A [`DiscardedOutputStream`] holds no buffers and spawns no reader task. The OS routes the child's
//! stdout or stderr to `/dev/null` (or its platform equivalent), and the parent never sees the
//! bytes. Because there is nothing to subscribe to, the type intentionally does not implement
//! [`crate::TrySubscribable`], so consumer-attaching APIs (`wait_for_completion_with_output`,
//! `inspect_lines`, etc.) are not in scope on a process whose stream is discarded.

use crate::output_stream::OutputStream;
use crate::output_stream::num_bytes::NumBytes;

/// Marker stream for a stdio slot configured with [`std::process::Stdio::null()`].
///
/// The OS discards the child's writes; no pipe is allocated and no reader task runs. The
/// `read_chunk_size` and `max_buffered_chunks` accessors are present because [`OutputStream`]
/// requires them, but they have no operational meaning for this variant and return zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardedOutputStream {
    name: &'static str,
}

impl DiscardedOutputStream {
    pub(crate) fn new(name: &'static str) -> Self {
        Self { name }
    }
}

impl OutputStream for DiscardedOutputStream {
    fn read_chunk_size(&self) -> NumBytes {
        NumBytes::zero()
    }

    fn max_buffered_chunks(&self) -> usize {
        0
    }

    fn name(&self) -> &'static str {
        self.name
    }
}
