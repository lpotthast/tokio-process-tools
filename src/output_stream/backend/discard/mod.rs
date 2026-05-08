//! Discard backend: a zero-cost stream marker for stdio configured as `Stdio::null()`.
//!
//! A [`DiscardedOutputStream`] holds no buffers and spawns no reader task. The OS routes the
//! child's stdout or stderr to `/dev/null` (or its platform equivalent), and the parent never
//! sees the bytes. The type still implements [`Subscribable`] and [`Consumable`] for API
//! uniformity (so generic helpers can target any backend), but its subscription emits a single
//! [`StreamEvent::Eof`] and then `None`: any visitor consumed against a discarded stream
//! observes zero chunks and terminates immediately.

use crate::output_stream::num_bytes::NumBytes;
use crate::output_stream::{Consumable, OutputStream};
use crate::{StreamEvent, Subscribable, Subscription};
use std::convert::Infallible;

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

#[doc(hidden)]
pub struct ImmediateEof {
    eof_emitted: bool,
}

impl ImmediateEof {
    fn new() -> Self {
        Self { eof_emitted: false }
    }
}

impl Subscription for ImmediateEof {
    fn next_event(&mut self) -> impl Future<Output = Option<StreamEvent>> + Send + '_ {
        let eof_emitted = self.eof_emitted;
        self.eof_emitted = true;
        async move {
            if eof_emitted {
                None
            } else {
                Some(StreamEvent::Eof)
            }
        }
    }
}

impl Subscribable for DiscardedOutputStream {
    type Subscription = ImmediateEof;
    type SubscribeError = Infallible;

    fn try_subscribe(&self) -> Result<Self::Subscription, Self::SubscribeError> {
        Ok(Self::Subscription::new())
    }
}

impl Consumable for DiscardedOutputStream {
    type Error = Infallible;
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
