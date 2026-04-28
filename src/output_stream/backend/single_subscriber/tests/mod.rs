use super::SingleSubscriberOutputStream;
use crate::{
    BestEffortDelivery, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NoReplay, NumBytes,
    StreamConfig,
};

mod active_consumer;
mod collector_lifecycle;
mod common;
mod inspector_lifecycle;
mod line_waiter;
mod reader_backpressure;
mod reader_chunking;
mod reader_drain;
mod reader_errors;
mod reader_test_support;
mod replay;
mod writer_lifecycle;

#[test]
#[should_panic(expected = "read_chunk_size must be greater than zero bytes")]
fn from_stream_panics_on_zero_read_chunk_size() {
    let _stream = SingleSubscriberOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        StreamConfig {
            read_chunk_size: NumBytes::zero(),
            max_buffered_chunks: DEFAULT_MAX_BUFFERED_CHUNKS,
            delivery: BestEffortDelivery,
            replay: NoReplay,
        },
    );
}

#[test]
#[should_panic(expected = "max_buffered_chunks must be greater than zero")]
fn from_stream_panics_on_zero_max_buffered_chunks() {
    let _stream = SingleSubscriberOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        StreamConfig {
            read_chunk_size: DEFAULT_READ_CHUNK_SIZE,
            max_buffered_chunks: 0,
            delivery: BestEffortDelivery,
            replay: NoReplay,
        },
    );
}
