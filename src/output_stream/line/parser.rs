//! Stateful line parser that splits arbitrary byte chunks into lines.
//!
//! The parser exposes one primitive — [`LineParser::next_line`] — that both the sync and async
//! sides of [`LineAdapter`](super::adapter::LineAdapter) drive. A single state machine handles
//! the chunk-spanning, max-line-length, and gap cases for both paths.

use super::options::{LineOverflowBehavior, LineParsingOptions};
use bytes::BytesMut;
use memchr::memchr;
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineParserMode {
    ReadingLine,
    DiscardUntilNewline,
    PendingLimitDelimiter,
}

/// Converts bytes to text with a fast path for proper UTF-8 text.
fn decode_line_lossy(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

/// Stateful parser for turning arbitrary byte chunks into lines.
///
/// Drive it by calling [`Self::next_line`] in a loop with a slice cursor that you advance
/// across calls; on EOF call [`Self::finish`] once to flush any unterminated trailing line.
/// On a stream gap (chunks dropped between deliveries) call [`Self::on_gap`] to discard the
/// in-progress partial line and resynchronize at the next newline.
pub struct LineParser {
    /// Bytes accumulated for the current in-progress line. Cleared (via `split`) when the line
    /// is emitted.
    line_buffer: BytesMut,

    /// Holds the most-recently emitted line so its bytes outlive the call that produced them.
    /// Each emission overwrites this slot via `BytesMut::split`, and the returned `Cow` borrows
    /// from here when the line did not fit entirely in a single chunk. The borrow checker
    /// enforces that the previous line is dropped before the next `next_line` call.
    emitted: BytesMut,

    mode: LineParserMode,
}

impl LineParser {
    /// Creates a new parser in `ReadingLine` mode with empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self {
            line_buffer: BytesMut::new(),
            emitted: BytesMut::new(),
            mode: LineParserMode::ReadingLine,
        }
    }

    /// Notifies the parser that the upstream delivery dropped chunks. Discards any partial
    /// line in progress and resynchronizes at the next newline instead of joining bytes
    /// across the gap.
    pub fn on_gap(&mut self) {
        self.line_buffer.clear();
        self.mode = LineParserMode::DiscardUntilNewline;
    }

    /// Advances through `chunk` and yields the next parsed line, if any.
    ///
    /// `chunk` is mutated in place to advance past the consumed prefix. Call repeatedly,
    /// reusing the same slice cursor, until this returns `None`; at that point the chunk is
    /// exhausted and any partial line is buffered for the next chunk.
    ///
    /// The returned [`Cow`] borrows from the chunk slice when the line fits entirely in this
    /// call and no partial line was already buffered (zero-allocation fast path), and borrows
    /// from the parser's internal emitted-line slot otherwise. Either way, drop the `Cow`
    /// before the next call — the borrow checker enforces this through the `&'a mut self`
    /// signature.
    pub fn next_line<'a, 'b>(
        &'a mut self,
        chunk: &mut &'b [u8],
        options: LineParsingOptions,
    ) -> Option<Cow<'a, str>>
    where
        'b: 'a,
    {
        while !chunk.is_empty() {
            match self.mode {
                LineParserMode::DiscardUntilNewline => {
                    if let Some(pos) = memchr(b'\n', chunk) {
                        self.mode = LineParserMode::ReadingLine;
                        *chunk = &chunk[pos + 1..];
                    } else {
                        *chunk = &[];
                        return None;
                    }
                    continue;
                }
                LineParserMode::PendingLimitDelimiter => {
                    self.mode = LineParserMode::ReadingLine;
                    if chunk.first() == Some(&b'\n') {
                        *chunk = &chunk[1..];
                        continue;
                    }
                }
                LineParserMode::ReadingLine => {}
            }

            if options.max_line_length.0 != 0
                && self.line_buffer.len() == options.max_line_length.0
            {
                // Mutate `self.mode` BEFORE the emit, so the returned `Cow`'s borrow on
                // `self` is the only outstanding borrow when we return.
                self.mode = match options.overflow_behavior {
                    LineOverflowBehavior::DropAdditionalData => {
                        LineParserMode::DiscardUntilNewline
                    }
                    LineOverflowBehavior::EmitAdditionalAsNewLines => {
                        LineParserMode::PendingLimitDelimiter
                    }
                };
                return Some(self.emit_buffered_line());
            }

            let remaining_line_length = if options.max_line_length.0 == 0 {
                chunk.len()
            } else {
                options.max_line_length.0 - self.line_buffer.len()
            };
            let scan_len = remaining_line_length.min(chunk.len());
            let scan = &chunk[..scan_len];

            if let Some(pos) = memchr(b'\n', scan) {
                if self.line_buffer.is_empty() {
                    // Fast path: the whole line fits in this chunk and no prefix was buffered;
                    // borrow directly from the chunk slice without copying.
                    let line = decode_line_lossy(&scan[..pos]);
                    *chunk = &chunk[pos + 1..];
                    return Some(line);
                }
                self.line_buffer.extend_from_slice(&scan[..pos]);
                *chunk = &chunk[pos + 1..];
                return Some(self.emit_buffered_line());
            }

            self.line_buffer.extend_from_slice(scan);
            *chunk = &chunk[scan_len..];

            if options.max_line_length.0 != 0
                && self.line_buffer.len() == options.max_line_length.0
                && matches!(
                    options.overflow_behavior,
                    LineOverflowBehavior::EmitAdditionalAsNewLines
                )
            {
                self.mode = LineParserMode::PendingLimitDelimiter;
                return Some(self.emit_buffered_line());
            }
        }

        None
    }

    /// Flushes any unterminated trailing line at EOF.
    ///
    /// Returns `None` when there is nothing to flush — the buffer is empty, or the parser is
    /// in `DiscardUntilNewline` mode (a gap or overflow truncation is still draining and the
    /// buffered remainder is conservatively dropped). Otherwise returns the buffered line as a
    /// [`Cow`] borrowing from the parser's emitted-line slot.
    pub fn finish(&mut self) -> Option<Cow<'_, str>> {
        if self.mode == LineParserMode::DiscardUntilNewline || self.line_buffer.is_empty() {
            None
        } else {
            Some(self.emit_buffered_line())
        }
    }

    /// Moves the in-progress line bytes into the emitted slot and decodes them.
    ///
    /// Uses swap-and-clear instead of `split()`: the in-progress buffer becomes the new
    /// `emitted`, the previous `emitted`'s allocation moves into `line_buffer` and gets
    /// cleared (length to 0, capacity retained) so the next line accumulates without
    /// allocating. Both buffers therefore behave as high-water-mark caches: each can grow up
    /// to `LineParsingOptions::max_line_length` and stays at that size for the parser's
    /// lifetime — no per-line allocator churn after the warm-up.
    ///
    /// The bytes live in `self.emitted` until the next emission swaps them out, which is
    /// exactly long enough for the returned `Cow` to remain valid until the caller drops it.
    fn emit_buffered_line(&mut self) -> Cow<'_, str> {
        std::mem::swap(&mut self.line_buffer, &mut self.emitted);
        self.line_buffer.clear();
        decode_line_lossy(&self.emitted)
    }
}

impl Default for LineParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NumBytes, NumBytesExt};
    use assertr::prelude::*;

    /// Drives the parser across all chunks and collects every emitted line, plus the trailing
    /// flush at EOF. Used by every test case below.
    fn run_test_case(
        chunks: &[&[u8]],
        mark_gap_before_chunk: Option<usize>,
        expected_lines: &[&str],
        options: LineParsingOptions,
    ) {
        let mut parser = LineParser::new();
        let mut collected_lines = Vec::<String>::new();

        for (index, chunk) in chunks.iter().enumerate() {
            if mark_gap_before_chunk == Some(index) {
                parser.on_gap();
            }

            let mut bytes: &[u8] = chunk;
            while let Some(line) = parser.next_line(&mut bytes, options) {
                collected_lines.push(line.into_owned());
            }
        }

        if let Some(line) = parser.finish() {
            collected_lines.push(line.into_owned());
        }

        let expected_lines: Vec<String> = expected_lines.iter().map(ToString::to_string).collect();
        assert_that!(collected_lines).is_equal_to(expected_lines);
    }

    fn emit_additional_options() -> LineParsingOptions {
        LineParsingOptions {
            max_line_length: 4.bytes(),
            overflow_behavior: LineOverflowBehavior::EmitAdditionalAsNewLines,
        }
    }

    fn as_single_byte_chunks(data: &str) -> Vec<&[u8]> {
        data.as_bytes().iter().map(std::slice::from_ref).collect()
    }

    #[test]
    fn basic_line_parsing_cases() {
        let default_options = LineParsingOptions::default();
        let drop_additional_options = LineParsingOptions {
            max_line_length: 4.bytes(),
            overflow_behavior: LineOverflowBehavior::DropAdditionalData,
        };

        run_test_case(&[b""], None, &[], default_options);
        run_test_case(
            &[b"no newlines here"],
            None,
            &["no newlines here"],
            default_options,
        );
        run_test_case(&[b"one line\n"], None, &["one line"], default_options);
        run_test_case(
            &[b"first line\nsecond line\nthird line\n"],
            None,
            &["first line", "second line", "third line"],
            default_options,
        );
        run_test_case(
            &[b"complete line\npartial"],
            None,
            &["complete line", "partial"],
            default_options,
        );
        run_test_case(
            &[b"previous: continuation\nmore lines\n"],
            None,
            &["previous: continuation", "more lines"],
            default_options,
        );
        run_test_case(&[b"1234\n\n"], None, &["1234", ""], drop_additional_options);
        run_test_case(
            &[b"ok\n123456789\nnext\n"],
            None,
            &["ok", "1234", "next"],
            drop_additional_options,
        );
    }

    #[test]
    fn invalid_utf8_data() {
        run_test_case(
            &[b"valid utf8\xF0\x28\x8C\xBC invalid utf8\n"],
            None,
            &["valid utf8\u{FFFD}(\u{FFFD}\u{FFFD} invalid utf8"],
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
            emit_additional_options(),
        );
    }

    #[test]
    fn emit_additional_as_new_lines_does_not_emit_synthetic_empty_lines() {
        let options = emit_additional_options();

        run_test_case(&[b"1234\n"], None, &["1234"], options);
        run_test_case(&[b"1234", b"\n"], None, &["1234"], options);
        run_test_case(&[b"12345678\n"], None, &["1234", "5678"], options);
        run_test_case(&[b"1234\n\n"], None, &["1234", ""], options);
    }

    #[test]
    fn max_line_length_of_0_disables_line_length_checks() {
        run_test_case(
            &[b"123456789\nabcdefghi\n"],
            None,
            &["123456789", "abcdefghi"],
            LineParsingOptions {
                max_line_length: NumBytes::zero(),
                overflow_behavior: LineOverflowBehavior::DropAdditionalData,
            },
        );
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
        let chunks = as_single_byte_chunks("\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}\n\u{1F44D}\n");
        run_test_case(
            &chunks,
            None,
            &["\u{2764}\u{FE0F}\u{2764}\u{FE0F}\u{2764}\u{FE0F}", "\u{1F44D}"],
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

    #[test]
    fn fast_path_borrows_when_line_fits_in_chunk_with_empty_buffer() {
        // When the entire line is in this chunk and the buffer is empty, the parser hands back
        // a `Cow::Borrowed` referencing the chunk slice. We can't observe Borrowed-vs-Owned
        // directly through `into_owned`, so check the variant before consuming.
        let mut parser = LineParser::new();
        let chunk: &[u8] = b"hello\nworld\n";
        let mut bytes = chunk;
        let line = parser
            .next_line(&mut bytes, LineParsingOptions::default())
            .expect("first line is yielded");
        assert_that!(matches!(line, Cow::Borrowed(_))).is_true();
        drop(line);
        let line = parser
            .next_line(&mut bytes, LineParsingOptions::default())
            .expect("second line is yielded");
        assert_that!(matches!(line, Cow::Borrowed(_))).is_true();
    }
}
