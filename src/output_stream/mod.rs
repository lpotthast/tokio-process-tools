use bytes::Buf;
use std::borrow::Cow;

pub mod broadcast;
pub(crate) mod impls;
pub mod single_subscriber;

/// We support the following implementations:
///
/// - [broadcast::BroadcastOutputStream]
/// - [single_subscriber::SingleSubscriberOutputStream]
pub trait OutputStream {}

/// NOTE: The maximum possible memory consumption is: `chunk_size * channel_capacity`.
/// Although reaching that level requires:
/// 1. A receiver to listen for chunks.
/// 2. The channel getting full.
pub struct FromStreamOptions {
    /// The size of the buffer used when reading from the stream in bytes.
    ///
    /// default: 32 * 1024 // 32 kb
    pub read_buffer_size: usize,

    /// The size of an individual chunk read from the read buffer in bytes.
    ///
    /// default: 16 * 1024 // 16 kb
    pub chunk_size: usize,

    /// The number of chunks held by the underlying async channel.
    ///
    /// When the subscriber (if present) is not fast enough to consume chunks equally fast or faster
    /// than them getting read, this acts as a buffer to hold not-yet processed messages.
    /// The bigger, the better, in terms of system resilience to write-spikes.
    /// Multiply with `chunk_size` to obtain the amount of system resources this will consume at
    /// max.
    pub channel_capacity: usize,
}

impl Default for FromStreamOptions {
    fn default() -> Self {
        Self {
            read_buffer_size: 32 * 1024, // 32 kb
            chunk_size: 16 * 1024,       // 16 kb
            channel_capacity: 128,       // => 16 kb * 128 = 2 mb (max memory usage consumption)
        }
    }
}

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
/// Note: If the underlying stream is of lower buffer size, chunks of length `chunk_size` may
/// never be observed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chunk(bytes::Bytes);

impl AsRef<[u8]> for Chunk {
    fn as_ref(&self) -> &[u8] {
        self.0.chunk()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureControl {
    /// ...
    DropLatestIncomingIfBufferFull,

    /// Will not lead to "lagging" (and dropping frames in the process).
    /// But this lowers our speed at which we consume output and may affect the application
    /// captured, as their pipe buffer may get full, requiring the application /
    /// relying on the application to drop data instead of writing to stdout/stderr in order
    /// to not block.
    BlockUntilBufferHasSpace,
}

/// Represents the type of the stream (stdout or stderr)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamType {
    StdOut,
    StdErr,

    Other(Cow<'static, str>),
}

/// Control flag to indicate whether processing should continue or break.
///
/// Returning `Break` from an `Inspector` will let that inspector stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Continue,
    Break,
}

/// Conceptually, this iterator appends the given byte slice to the current line buffer, which may
/// already hold some previously written data.
/// The resulting view of data is split by newlines (`\n`). Every completed line is yielded.
/// The remainder of the chunk, not completed with a newline character, will become the new content
/// of `line_buffer`.
///
/// The implementation tries to allocate as little as possible.
///
/// # Members
/// * `chunk` - New slice of bytes to process.
/// * `line_buffer` - Buffer for reading one line.
///   May hold previously seen, not-yet-closed, line-data.
pub(crate) struct LineReader<'c, 'b> {
    chunk: &'c [u8],
    line_buffer: &'b mut String,
}

impl Iterator for LineReader<'_, '_> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.chunk.is_empty() {
            return None;
        }

        match self.chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - consume the remaining chunk.
                self.line_buffer
                    .push_str(String::from_utf8_lossy(self.chunk).as_ref());
                self.chunk = &[];
                None
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = self.chunk.split_at(pos);
                self.line_buffer
                    .push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                let to_return = self.line_buffer.clone();

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                self.line_buffer.clear();
                self.line_buffer.shrink_to(2048);
                self.chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                Some(to_return)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::output_stream::LineReader;
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::io::{AsyncWrite, AsyncWriteExt};

    pub(crate) async fn write_test_data(mut write: impl AsyncWrite + Unpin) {
        write.write_all("Cargo.lock\n".as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write.write_all("Cargo.toml\n".as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write.write_all("README.md\n".as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write.write_all("src\n".as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        write.write_all("target\n".as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // TODO: also test async variant
    #[test]
    fn test_process_lines_in_chunk() {
        // Helper function to reduce duplication in test cases
        fn run_test_case(
            test_name: &str,
            chunk: &[u8],
            initial_line_buffer: &str,
            expected_result: &str,
            expected_lines: &[&str],
        ) {
            let mut line_buffer = String::from(initial_line_buffer);
            let mut collected_lines: Vec<String> = Vec::new();

            let lr = LineReader {
                chunk: chunk,
                line_buffer: &mut line_buffer,
            };
            for line in lr {
                collected_lines.push(line);
            }

            assert_that(line_buffer)
                .with_detail_message(format!("Test case: {test_name}"))
                .is_equal_to(expected_result);

            let expected_lines: Vec<String> =
                expected_lines.iter().map(|s| s.to_string()).collect();

            assert_that(collected_lines)
                .with_detail_message(format!("Test case: {test_name}"))
                .is_equal_to(expected_lines);
        }

        // Test case 1: Empty chunk
        run_test_case(
            "Empty chunk",
            b"",
            "existing line: ",
            "existing line: ",
            &[],
        );

        // Test case 2: Chunk with no newlines
        run_test_case(
            "Chunk with no newlines",
            b"no newlines here",
            "existing line: ",
            "existing line: no newlines here",
            &[],
        );

        // Test case 3: Single complete line
        run_test_case("Single complete line", b"one line\n", "", "", &["one line"]);

        // Test case 4: Multiple complete lines
        run_test_case(
            "Multiple complete lines",
            b"first line\nsecond line\nthird line\n",
            "",
            "",
            &["first line", "second line", "third line"],
        );

        // Test case 5: Partial line at the end
        run_test_case(
            "Partial line at the end",
            b"complete line\npartial",
            "",
            "partial",
            &["complete line"],
        );

        // Test case 6: Initial line with multiple newlines
        run_test_case(
            "Initial line with multiple newlines",
            b"continuation\nmore lines\n",
            "initial part of line: ",
            "",
            &["initial part of line: continuation", "more lines"],
        );

        // Test case 7: Non-UTF8 data
        {
            // This test case needs special handling due to its assertions
            let chunk = b"valid utf8\xF0\x28\x8C\xBC invalid utf8\n";
            let mut line_buffer = String::from("");
            let mut collected_lines = Vec::new();

            let lr = LineReader {
                chunk: chunk,
                line_buffer: &mut line_buffer,
            };
            for line in lr {
                collected_lines.push(line);
            }

            assert_that(line_buffer).is_equal_to("");
            assert_that(collected_lines[0].as_str()).contains("valid utf8");
            assert_that(collected_lines[0].as_str()).contains("invalid utf8");
        }
    }
}
