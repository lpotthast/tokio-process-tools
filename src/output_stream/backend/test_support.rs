#![cfg(test)]

use crate::output_stream::event::{Chunk, StreamEvent};
use assertr::prelude::*;
use bytes::Bytes;

pub(super) fn chunk(bytes: &'static [u8]) -> StreamEvent {
    StreamEvent::Chunk(Chunk(Bytes::from_static(bytes)))
}

pub(super) fn assert_chunk(event: &StreamEvent, expected: &[u8]) {
    match event {
        StreamEvent::Chunk(chunk) => {
            assert_that!(chunk.as_ref()).is_equal_to(expected);
        }
        other => {
            assert_that!(other).fail(format_args!("expected chunk, got {other:?}"));
        }
    }
}
