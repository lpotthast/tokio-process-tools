use bytes::{Buf, BytesMut};
use std::io::BufRead;

/// Default chunk size read from the source stream. 16 kilobytes.
pub const DEFAULT_CHUNK_SIZE: NumBytes = NumBytes(16 * 1024); // 16 kb

/// Default channel capacity for stdout and stderr streams. 128 slots.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 128;

/// Broadcast output stream implementation supporting multiple concurrent consumers.
pub mod broadcast;

pub(crate) mod impls;

/// Single subscriber output stream implementation for efficient single-consumer scenarios.
pub mod single_subscriber;

/// We support the following implementations:
///
/// - [broadcast::BroadcastOutputStream]
/// - [single_subscriber::SingleSubscriberOutputStream]
pub trait OutputStream {
    /// The maximum size of every chunk read by the backing `stream_reader`.
    fn chunk_size(&self) -> NumBytes;

    /// The number of chunks held by the underlying async channel.
    fn channel_capacity(&self) -> usize;

    /// Type of stream. Can be "stdout" or "stderr". But we do not guarantee this output.
    /// It should only be used for logging/debugging.
    fn name(&self) -> &'static str;
}

/// NOTE: The maximum possible memory consumption is: `chunk_size * channel_capacity`.
/// Although reaching that level requires:
/// 1. A receiver to listen for chunks.
/// 2. The channel getting full.
pub struct FromStreamOptions {
    /// The size of an individual chunk read from the read buffer in bytes.
    ///
    /// default: 16 * 1024 // 16 kb
    pub chunk_size: NumBytes,

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
            chunk_size: DEFAULT_CHUNK_SIZE,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY, // => 16 kb * 128 = 2 mb (max memory usage consumption)
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
/// Note: If the underlying stream is of lower buffer size, chunks of full `chunk_size` length may
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

/// Control flag to indicate whether processing should continue or break.
///
/// Returning `Break` from an `Inspector`/`Collector` will let that instance stop visiting any
/// more data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Interested in receiving additional data.
    Continue,

    /// Not interested in receiving additional data. Will let the `inspector`/`collector` stop.
    Break,
}

/// What should happen when a line is too long?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineOverflowBehavior {
    /// Drop any additional data received after the current line was considered too long until
    /// the next newline character is observed, which then starts a new line.
    #[default]
    DropAdditionalData,

    /// Emit the current line when the maximum allowed length is reached.
    /// Any additional data received is immediately taken as the content of the next line.
    ///
    /// This option really just adds intermediate line breaks to not let any emitted line exceed the
    /// length limit.
    ///
    /// No data is dropped with this behavior.
    EmitAdditionalAsNewLines,
}

/// Configuration options for parsing lines from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineParsingOptions {
    /// Maximum length of a single line in bytes.
    /// When reached, further data won't be appended to the current line.
    /// The line will be emitted in its current state.
    ///
    /// A value of `0` means that "no limit" is imposed.
    ///
    /// Only set this to `0` when you absolutely trust the input stream! Remember that an observed
    /// stream maliciously writing endless amounts of data without ever writing a line break
    /// would starve this system from ever emitting a line and will lead to an infinite amount of
    /// memory being allocated to hold the line data, letting this process running out of memory!
    ///
    /// Defaults to 16 kilobytes.
    pub max_line_length: NumBytes,

    /// What should happen when a line is too long?
    pub overflow_behavior: LineOverflowBehavior,
}

impl Default for LineParsingOptions {
    fn default() -> Self {
        Self {
            max_line_length: 16.kilobytes(),
            overflow_behavior: LineOverflowBehavior::default(),
        }
    }
}

/// A wrapper type representing a number of bytes.
///
/// Use the [`NumBytesExt`] trait to conveniently create instances:
/// ```
/// use tokio_process_tools::NumBytesExt;
/// let kb = 16.kilobytes();
/// let mb = 2.megabytes();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumBytes(usize);

impl NumBytes {
    /// Creates a NumBytes value of zero.
    pub fn zero() -> Self {
        Self(0)
    }

    /// The amount of bytes represented by this instance.
    pub fn bytes(&self) -> usize {
        self.0
    }
}

/// Extension trait providing convenience-functions for creation of [`NumBytes`] of certain sizes.
pub trait NumBytesExt {
    /// Interprets the value as literal bytes.
    fn bytes(self) -> NumBytes;

    /// Interprets the value as kilobytes (value * 1024).
    fn kilobytes(self) -> NumBytes;

    /// Interprets the value as megabytes (value * 1024 * 1024).
    fn megabytes(self) -> NumBytes;
}

impl NumBytesExt for usize {
    fn bytes(self) -> NumBytes {
        NumBytes(self)
    }

    fn kilobytes(self) -> NumBytes {
        NumBytes(self * 1024)
    }

    fn megabytes(self) -> NumBytes {
        NumBytes(self * 1024 * 1024)
    }
}

/// Conceptually, this iterator appends the given byte slice to the current line buffer, which may
/// already hold some previously written data.
/// The resulting view of data is split by newlines (`\n`). Every completed line is yielded.
/// The remainder of the chunk, not completed with a newline character, will become the new content
/// of `line_buffer`.
///
/// The implementation tries to allocate as little as possible.
///
/// It can be expected that `line_buffer` does not grow beyond `options.max_line_length` bytes
/// **IF** any yielded line is dropped or cloned and **NOT** stored long term.
/// Only then can the underlying storage, used to capture that line, be reused to capture the
/// next line.
///
/// # Members
/// * `chunk` - New slice of bytes to process.
/// * `line_buffer` - Buffer for reading one line.
///   May hold previously seen, not-yet-closed, line-data.
pub(crate) struct LineReader<'c, 'b> {
    chunk: &'c [u8],
    line_buffer: &'b mut BytesMut,
    last_line_length: Option<usize>,
    options: LineParsingOptions,
}

impl<'c, 'b> LineReader<'c, 'b> {
    pub fn new(
        chunk: &'c [u8],
        line_buffer: &'b mut BytesMut,
        options: LineParsingOptions,
    ) -> Self {
        Self {
            chunk,
            line_buffer,
            last_line_length: None,
            options,
        }
    }

    fn append_to_line_buffer(&mut self, chunk: &[u8]) {
        self.line_buffer.extend_from_slice(chunk)
    }

    fn _take_line(&mut self) -> bytes::Bytes {
        self.last_line_length = Some(self.line_buffer.len());
        self.line_buffer.split().freeze()
    }

    fn take_line(&mut self, full_line_buffer: bool) -> bytes::Bytes {
        if full_line_buffer {
            match self.options.overflow_behavior {
                LineOverflowBehavior::DropAdditionalData => {
                    // Drop any additional (until the next newline character!)
                    // and return the current (not regularly finished) line.
                    let _ = self.chunk.skip_until(b'\n');
                    self._take_line()
                }
                LineOverflowBehavior::EmitAdditionalAsNewLines => {
                    // Do NOT drop any additional and return the current (not regularly finished)
                    // line. This will lead to all additional data starting a new line in the
                    // next iteration.
                    self._take_line()
                }
            }
        } else {
            self._take_line()
        }
    }
}

impl Iterator for LineReader<'_, '_> {
    type Item = bytes::Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        // Ensure we never go out of bounds with our line buffer.
        // This also ensures that no-one creates a `LineReader` with a line buffer that is already
        // too large for our current `options.max_line_length`.
        if self.options.max_line_length.0 != 0 {
            assert!(self.line_buffer.len() <= self.options.max_line_length.0);
        }

        // Note: This will always be seen, even when the processed chunk ends with `\n`, as
        // every iterator must once return `None` to signal that it has finished!
        // And this, we only do later.
        if let Some(last_line_length) = self.last_line_length.take() {
            // The previous iteration yielded line of this length!
            let reclaimed = self.line_buffer.try_reclaim(last_line_length);
            if !reclaimed {
                tracing::warn!(
                    "Could not reclaim {last_line_length} bytes of line_buffer space. DO NOT store a yielded line (of type `bytes::Bytes`) long term. If you need to, clone it instead, to prevent the `line_buffer` from growing indefinitely (for any additional line processed). Also, make sure to set an appropriate `options.max_line_length`."
                );
            }
        }

        // Code would work without this early-return. But this lets us skip a lot of actions on
        // empty slices.
        if self.chunk.is_empty() {
            return None;
        }

        // Through our assert above, the first operand will always be bigger!
        let remaining_line_length = if self.options.max_line_length.0 == 0 {
            usize::MAX
        } else {
            self.options.max_line_length.0 - self.line_buffer.len()
        };

        // The previous iteration might have filled the line buffer completely.
        // Apply overflow behavior.
        if remaining_line_length == 0 {
            return Some(self.take_line(true));
        }

        // We have space remaining in our line buffer.
        // Split the chunk into two a usable portion (which would not "overflow" the line buffer)
        // and the rest.
        let (usable, rest) = self
            .chunk
            .split_at(usize::min(self.chunk.len(), remaining_line_length));

        // Search for the next newline character in the usable portion of our current chunk.
        match usable.iter().position(|b| *b == b'\n') {
            None => {
                // No line break found! Consume the whole usable chunk portion.
                self.append_to_line_buffer(usable);
                self.chunk = rest;

                if rest.is_empty() {
                    // Return None, as we have no more data to process.
                    // Leftover data in `line_buffer` must be taken care of externally!
                    None
                } else {
                    // Line now full. Would overflow using rest. Return the current line!
                    assert_eq!(self.line_buffer.len(), self.options.max_line_length.0);
                    Some(self.take_line(true))
                }
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (usable_until_line_break, _usable_rest) = usable.split_at(pos);
                self.append_to_line_buffer(usable_until_line_break);

                // We did split our chunk into `let (usable, rest) = ...` earlier.
                // We then split usable into `let (usable_until_line_break, _usable_rest) = ...`.
                // We know that `_usable_rest` and `rest` are consecutive in `chunk`!
                // This is the combination of `_usable_rest` and `rest` expressed through `chunk`
                // to get to the "real"/"complete" rest of data.
                let rest = &self.chunk[usable_until_line_break.len()..];

                // Skip the `\n` byte!
                self.chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                // Return the completed line.
                Some(self.take_line(false))
            }
        }
    }
}

#[cfg(test)]
mod tests {
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

    mod line_reader {
        use crate::output_stream::LineReader;
        use crate::{LineOverflowBehavior, LineParsingOptions, NumBytes, NumBytesExt};
        use assertr::prelude::*;
        use bytes::{Bytes, BytesMut};
        use tracing_test::traced_test;

        #[test]
        #[traced_test]
        fn multi_byte_utf_8_characters_are_preserved_even_when_parsing_multiple_one_byte_chunks() {
            let mut line_buffer = BytesMut::new();
            let mut collected_lines: Vec<String> = Vec::new();

            let data = "❤️❤️❤️\n👍\n";
            for byte in data.as_bytes() {
                let lr = LineReader {
                    chunk: &[*byte],
                    line_buffer: &mut line_buffer,
                    last_line_length: None,
                    options: LineParsingOptions::default(),
                };
                for line in lr {
                    collected_lines.push(String::from_utf8_lossy(&line).to_string());
                }
            }

            assert_that(collected_lines).contains_exactly(["❤️❤️❤️", "👍"]);
        }

        #[test]
        #[traced_test]
        fn reclaims_line_buffer_space_before_collecting_new_line() {
            let mut line_buffer = BytesMut::new();
            let mut collected_lines: Vec<String> = Vec::new();
            let mut bytes: Vec<Bytes> = Vec::new();

            let data = "❤️❤️❤️\n❤️❤️❤️\n";
            for byte in data.as_bytes() {
                let lr = LineReader {
                    chunk: &[*byte],
                    line_buffer: &mut line_buffer,
                    last_line_length: None,
                    options: LineParsingOptions::default(),
                };
                for line in lr {
                    collected_lines.push(String::from_utf8_lossy(&line).to_string());
                    bytes.push(line);
                }
            }

            let data = "❤️❤️❤️\n";
            let lr = LineReader {
                chunk: data.as_bytes(),
                line_buffer: &mut line_buffer,
                last_line_length: None,
                options: LineParsingOptions::default(),
            };
            for line in lr {
                collected_lines.push(String::from_utf8_lossy(&line).to_string());
                bytes.push(line);
            }

            assert_that(collected_lines).contains_exactly(["❤️❤️❤️", "❤️❤️❤️", "❤️❤️❤️"]);

            logs_assert(|lines: &[&str]| {
                match lines
                    .iter()
                    .filter(|line| line.contains("Could not reclaim 18 bytes of line_buffer space. DO NOT store a yielded line (of type `bytes::Bytes`) long term. If you need to, clone it instead, to prevent the `line_buffer` from growing indefinitely (for any additional line processed). Also, make sure to set an appropriate `options.max_line_length`."))
                    .count()
                {
                    3 => {}
                    n => return Err(format!("Expected exactly one log, but found {n}")),
                };
                Ok(())
            });
        }

        // Helper function to reduce duplication in test cases
        fn run_test_case(
            chunk: &[u8],
            line_buffer_before: &str,
            line_buffer_after: &str,
            expected_lines: &[&str],
            options: LineParsingOptions,
        ) {
            let mut line_buffer = BytesMut::from(line_buffer_before);
            let mut collected_lines: Vec<String> = Vec::new();

            let lr = LineReader {
                chunk,
                line_buffer: &mut line_buffer,
                last_line_length: None,
                options,
            };
            for line in lr {
                collected_lines.push(String::from_utf8_lossy(&line).to_string());
            }

            assert_that(line_buffer).is_equal_to(line_buffer_after);

            let expected_lines: Vec<String> =
                expected_lines.iter().map(|s| s.to_string()).collect();

            assert_that(collected_lines).is_equal_to(expected_lines);
        }

        #[test]
        fn empty_chunk() {
            run_test_case(
                b"",
                "previous: ",
                "previous: ",
                &[],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn chunk_without_any_newlines() {
            run_test_case(
                b"no newlines here",
                "previous: ",
                "previous: no newlines here",
                &[],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn single_completed_line() {
            run_test_case(
                b"one line\n",
                "",
                "",
                &["one line"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn multiple_completed_lines() {
            run_test_case(
                b"first line\nsecond line\nthird line\n",
                "",
                "",
                &["first line", "second line", "third line"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn partial_line_at_the_end() {
            run_test_case(
                b"complete line\npartial",
                "",
                "partial",
                &["complete line"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn initial_line_with_multiple_newlines() {
            run_test_case(
                b"continuation\nmore lines\n",
                "previous: ",
                "",
                &["previous: continuation", "more lines"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn invalid_utf8_data() {
            run_test_case(
                b"valid utf8\xF0\x28\x8C\xBC invalid utf8\n",
                "",
                "",
                &["valid utf8�(�� invalid utf8"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn rest_of_too_long_line_is_dropped() {
            run_test_case(
                b"123456789\nabcdefghi\n",
                "",
                "",
                &["1234", "abcd"],
                LineParsingOptions {
                    max_line_length: 4.bytes(), // Only allow lines with 4 ascii chars (or equiv.) max.
                    overflow_behavior: LineOverflowBehavior::DropAdditionalData,
                },
            );
        }

        #[test]
        fn rest_of_too_long_line_is_returned_as_additional_lines() {
            run_test_case(
                b"123456789\nabcdefghi\n",
                "",
                "",
                &["1234", "5678", "9", "abcd", "efgh", "i"],
                LineParsingOptions {
                    max_line_length: 4.bytes(), // Only allow lines with 4 ascii chars (or equiv.) max.
                    overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
                },
            );
        }

        #[test]
        fn max_line_length_of_0_disables_line_length_checks_test1() {
            run_test_case(
                b"123456789\nabcdefghi\n",
                "",
                "",
                &["123456789", "abcdefghi"],
                LineParsingOptions {
                    max_line_length: NumBytes::zero(),
                    overflow_behavior: LineOverflowBehavior::DropAdditionalData,
                },
            );
        }

        #[test]
        fn max_line_length_of_0_disables_line_length_checks_test2() {
            run_test_case(
                b"123456789\nabcdefghi\n",
                "",
                "",
                &["123456789", "abcdefghi"],
                LineParsingOptions {
                    max_line_length: NumBytes::zero(),
                    overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
                },
            );
        }

        #[test]
        fn leading_and_trailing_whitespace_is_preserved() {
            run_test_case(
                b"   123456789     \n    abcdefghi        \n",
                "",
                "",
                &["   123456789     ", "    abcdefghi        "],
                LineParsingOptions {
                    max_line_length: NumBytes::zero(),
                    overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
                },
            );
        }
    }
}
