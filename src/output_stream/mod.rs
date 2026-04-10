use bytes::{Buf, BytesMut};
use std::borrow::Cow;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamEvent {
    Chunk(Chunk),
    Gap,
    Eof,
}

/// Controls how a single-subscriber stream reacts when its in-memory buffer fills up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureControl {
    /// Drop newly read chunks whenever the in-memory buffer is full.
    ///
    /// This keeps the stream reader moving and avoids backpressuring the child process, but a slow
    /// consumer may miss output.
    DropLatestIncomingIfBufferFull,

    /// Wait for buffer space instead of dropping chunks.
    ///
    /// This avoids losing output inside the library, but it can slow down stream consumption. If
    /// the child process writes faster than the consumer can keep up, the OS pipe may fill and
    /// backpressure the child process itself.
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
    ///
    /// The discard state persists across chunk boundaries. Once the limit is reached, subsequent
    /// bytes are ignored until a real newline is observed.
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

/// Controls how line-based write helpers delimit successive lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineWriteMode {
    /// Write lines exactly as parsed, without appending any delimiter.
    ///
    /// Use this when your mapper already includes delimiters or when the downstream format does
    /// not want line separators reintroduced.
    AsIs,

    /// Append a trailing `\n` after each emitted line.
    ///
    /// This reconstructs conventional line-oriented output after parsing removed the original
    /// newline byte.
    AppendLf,
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
    ///
    /// When lossy buffering drops chunks before they reach the parser, line-based consumers
    /// conservatively discard any partial line and resynchronize at the next newline instead of
    /// joining bytes across the gap.
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

/// Stateful parser for turning arbitrary byte chunks into lines.
pub(crate) struct LineParserState {
    line_buffer: BytesMut,
    discard_until_newline: bool,
}

impl LineParserState {
    pub fn new() -> Self {
        Self {
            line_buffer: BytesMut::new(),
            discard_until_newline: false,
        }
    }

    pub fn on_gap(&mut self) {
        self.line_buffer.clear();
        self.discard_until_newline = true;
    }

    pub fn visit_chunk(
        &mut self,
        chunk: &[u8],
        options: LineParsingOptions,
        mut f: impl FnMut(Cow<'_, str>) -> Next,
    ) -> Next {
        for byte in chunk.iter().copied() {
            if self.discard_until_newline {
                if byte == b'\n' {
                    self.discard_until_newline = false;
                }
                continue;
            }

            if byte == b'\n' {
                if self.emit_line(&mut f) == Next::Break {
                    return Next::Break;
                }
                continue;
            }

            if options.max_line_length.0 != 0 && self.line_buffer.len() == options.max_line_length.0
            {
                match options.overflow_behavior {
                    LineOverflowBehavior::DropAdditionalData => {
                        if self.emit_line(&mut f) == Next::Break {
                            return Next::Break;
                        }
                        self.discard_until_newline = true;
                        continue;
                    }
                    LineOverflowBehavior::EmitAdditionalAsNewLines => {
                        if self.emit_line(&mut f) == Next::Break {
                            return Next::Break;
                        }
                    }
                }
            }

            self.line_buffer.extend_from_slice(&[byte]);

            if options.max_line_length.0 != 0
                && self.line_buffer.len() == options.max_line_length.0
                && matches!(
                    options.overflow_behavior,
                    LineOverflowBehavior::EmitAdditionalAsNewLines
                )
                && self.emit_line(&mut f) == Next::Break
            {
                return Next::Break;
            }
        }

        Next::Continue
    }

    pub fn finish(&self, f: impl FnOnce(Cow<'_, str>) -> Next) -> Next {
        if self.discard_until_newline || self.line_buffer.is_empty() {
            Next::Continue
        } else {
            f(String::from_utf8_lossy(&self.line_buffer))
        }
    }

    fn emit_line(&mut self, f: &mut impl FnMut(Cow<'_, str>) -> Next) -> Next {
        let line = self.line_buffer.split().freeze();
        f(String::from_utf8_lossy(&line))
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

    mod line_parser_state {
        use crate::output_stream::LineParserState;
        use crate::{LineOverflowBehavior, LineParsingOptions, Next, NumBytes, NumBytesExt};
        use assertr::prelude::*;

        fn run_test_case(
            chunks: &[&[u8]],
            mark_gap_before_chunk: Option<usize>,
            expected_lines: &[&str],
            options: LineParsingOptions,
        ) {
            let mut parser = LineParserState::new();
            let mut collected_lines = Vec::<String>::new();

            for (index, chunk) in chunks.iter().enumerate() {
                if mark_gap_before_chunk == Some(index) {
                    parser.on_gap();
                }

                assert_that!(parser.visit_chunk(chunk, options, |line| {
                    collected_lines.push(line.into_owned());
                    Next::Continue
                }))
                .is_equal_to(Next::Continue);
            }

            let _ = parser.finish(|line| {
                collected_lines.push(line.into_owned());
                Next::Continue
            });

            let expected_lines: Vec<String> =
                expected_lines.iter().map(|s| s.to_string()).collect();
            assert_that!(collected_lines).is_equal_to(expected_lines);
        }

        fn as_single_byte_chunks(data: &str) -> Vec<&[u8]> {
            data.as_bytes().iter().map(std::slice::from_ref).collect()
        }

        #[test]
        fn empty_chunk() {
            run_test_case(&[b""], None, &[], LineParsingOptions::default());
        }

        #[test]
        fn chunk_without_any_newlines() {
            run_test_case(
                &[b"no newlines here"],
                None,
                &["no newlines here"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn single_completed_line() {
            run_test_case(
                &[b"one line\n"],
                None,
                &["one line"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn multiple_completed_lines() {
            run_test_case(
                &[b"first line\nsecond line\nthird line\n"],
                None,
                &["first line", "second line", "third line"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn partial_line_at_the_end() {
            run_test_case(
                &[b"complete line\npartial"],
                None,
                &["complete line", "partial"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn initial_line_with_multiple_newlines() {
            run_test_case(
                &[b"previous: continuation\nmore lines\n"],
                None,
                &["previous: continuation", "more lines"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn invalid_utf8_data() {
            run_test_case(
                &[b"valid utf8\xF0\x28\x8C\xBC invalid utf8\n"],
                None,
                &["valid utf8�(�� invalid utf8"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn rest_of_too_long_line_is_dropped() {
            run_test_case(
                &[b"123456789\nabcdefghi\n"],
                None,
                &["1234", "abcd"],
                LineParsingOptions {
                    max_line_length: 4.bytes(),
                    overflow_behavior: LineOverflowBehavior::DropAdditionalData,
                },
            );
        }

        #[test]
        fn rest_of_too_long_line_is_returned_as_additional_lines() {
            run_test_case(
                &[b"123456789\nabcdefghi\n"],
                None,
                &["1234", "5678", "9", "abcd", "efgh", "i"],
                LineParsingOptions {
                    max_line_length: 4.bytes(),
                    overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
                },
            );
        }

        #[test]
        fn max_line_length_of_0_disables_line_length_checks_test1() {
            run_test_case(
                &[b"123456789\nabcdefghi\n"],
                None,
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
                &[b"123456789\nabcdefghi\n"],
                None,
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
                &[b"   123456789     \n    abcdefghi        \n"],
                None,
                &["   123456789     ", "    abcdefghi        "],
                LineParsingOptions {
                    max_line_length: NumBytes::zero(),
                    overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
                },
            );
        }

        #[test]
        fn multi_byte_utf_8_characters_are_preserved_even_when_parsing_multiple_one_byte_chunks() {
            let chunks = as_single_byte_chunks("❤️❤️❤️\n👍\n");
            run_test_case(
                &chunks,
                None,
                &["❤️❤️❤️", "👍"],
                LineParsingOptions::default(),
            );
        }

        #[test]
        fn overflow_drop_additional_data_persists_across_chunks() {
            run_test_case(
                &[b"1234", b"5678", b"9\nok\n"],
                None,
                &["1234", "ok"],
                LineParsingOptions {
                    max_line_length: 4.bytes(),
                    overflow_behavior: LineOverflowBehavior::DropAdditionalData,
                },
            );
        }

        #[test]
        fn gap_discards_partial_line_until_next_newline() {
            run_test_case(
                &[b"rea", b"dy\nnext\n"],
                Some(1),
                &["next"],
                LineParsingOptions::default(),
            );
        }
    }
}
