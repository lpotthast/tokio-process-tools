use crate::{CollectionOverflowBehavior, LineCollectionOptions, NumBytesExt};
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

pub(crate) fn line_collection_options() -> LineCollectionOptions {
    LineCollectionOptions::Bounded {
        max_bytes: 1.megabytes(),
        max_lines: 1024,
        overflow_behavior: CollectionOverflowBehavior::default(),
    }
}

#[derive(Debug)]
pub(crate) struct ReadErrorAfterBytes {
    bytes: &'static [u8],
    offset: usize,
    error_kind: io::ErrorKind,
}

impl ReadErrorAfterBytes {
    pub(crate) fn new(bytes: &'static [u8], error_kind: io::ErrorKind) -> Self {
        Self {
            bytes,
            offset: 0,
            error_kind,
        }
    }
}

impl AsyncRead for ReadErrorAfterBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset < self.bytes.len() {
            let remaining = &self.bytes[self.offset..];
            let len = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..len]);
            self.offset += len;
            return Poll::Ready(Ok(()));
        }

        Poll::Ready(Err(io::Error::new(
            self.error_kind,
            "injected read failure",
        )))
    }
}
