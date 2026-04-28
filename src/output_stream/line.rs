use crate::output_stream::Next;
use crate::output_stream::num_bytes::{NumBytes, NumBytesExt};
use bytes::BytesMut;
use memchr::memchr;
use std::borrow::Cow;
use typed_builder::TypedBuilder;

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

/// Configuration options for parsing lines from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
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
pub(crate) struct LineParserState {
    line_buffer: BytesMut,
    mode: LineParserMode,
}

impl LineParserState {
    pub fn new() -> Self {
        Self {
            line_buffer: BytesMut::new(),
            mode: LineParserMode::ReadingLine,
        }
    }

    pub fn on_gap(&mut self) {
        self.line_buffer.clear();
        self.mode = LineParserMode::DiscardUntilNewline;
    }

    pub fn visit_chunk(
        &mut self,
        mut chunk: &[u8],
        options: LineParsingOptions,
        mut f: impl FnMut(Cow<'_, str>) -> Next,
    ) -> Next {
        while !chunk.is_empty() {
            match self.mode {
                LineParserMode::DiscardUntilNewline => {
                    match memchr(b'\n', chunk) {
                        Some(pos) => {
                            self.mode = LineParserMode::ReadingLine;
                            chunk = &chunk[pos + 1..];
                        }
                        None => return Next::Continue,
                    }
                    continue;
                }
                LineParserMode::PendingLimitDelimiter => {
                    self.mode = LineParserMode::ReadingLine;
                    if chunk.first() == Some(&b'\n') {
                        chunk = &chunk[1..];
                        continue;
                    }
                }
                LineParserMode::ReadingLine => {}
            }

            if options.max_line_length.0 != 0 && self.line_buffer.len() == options.max_line_length.0
            {
                match options.overflow_behavior {
                    LineOverflowBehavior::DropAdditionalData => {
                        if self.emit_line(&mut f) == Next::Break {
                            return Next::Break;
                        }
                        self.mode = LineParserMode::DiscardUntilNewline;
                    }
                    LineOverflowBehavior::EmitAdditionalAsNewLines => {
                        if self.emit_line(&mut f) == Next::Break {
                            return Next::Break;
                        }
                        self.mode = LineParserMode::PendingLimitDelimiter;
                    }
                }
                continue;
            }

            let remaining_line_length = if options.max_line_length.0 == 0 {
                chunk.len()
            } else {
                options.max_line_length.0 - self.line_buffer.len()
            };
            let scan_len = remaining_line_length.min(chunk.len());
            let scan = &chunk[..scan_len];

            if let Some(pos) = memchr(b'\n', scan) {
                // Optimization: Complete line in chunk? Then do not copy into BytesMut first.
                let result = if self.line_buffer.is_empty() {
                    f(decode_line_lossy(&scan[..pos]))
                } else {
                    self.line_buffer.extend_from_slice(&scan[..pos]);
                    self.emit_line(&mut f)
                };

                if result == Next::Break {
                    return Next::Break;
                }
                chunk = &chunk[pos + 1..];
                continue;
            }

            self.line_buffer.extend_from_slice(scan);
            chunk = &chunk[scan_len..];

            if options.max_line_length.0 != 0
                && self.line_buffer.len() == options.max_line_length.0
                && matches!(
                    options.overflow_behavior,
                    LineOverflowBehavior::EmitAdditionalAsNewLines
                )
            {
                let next = self.emit_line(&mut f);
                self.mode = LineParserMode::PendingLimitDelimiter;
                if next == Next::Break {
                    return Next::Break;
                }
            }
        }

        Next::Continue
    }

    pub(crate) fn owned_lines<'a>(
        &'a mut self,
        chunk: &'a [u8],
        options: LineParsingOptions,
    ) -> OwnedLineReader<'a> {
        OwnedLineReader {
            parser: self,
            chunk,
            options,
        }
    }

    pub fn finish(&self, f: impl FnOnce(Cow<'_, str>) -> Next) -> Next {
        if self.mode == LineParserMode::DiscardUntilNewline || self.line_buffer.is_empty() {
            Next::Continue
        } else {
            f(decode_line_lossy(&self.line_buffer))
        }
    }

    pub(crate) fn finish_owned(&self) -> Option<String> {
        if self.mode == LineParserMode::DiscardUntilNewline || self.line_buffer.is_empty() {
            None
        } else {
            Some(decode_line_lossy(&self.line_buffer).into_owned())
        }
    }

    fn emit_line(&mut self, f: &mut impl FnMut(Cow<'_, str>) -> Next) -> Next {
        let line = self.line_buffer.split().freeze();
        f(decode_line_lossy(&line))
    }

    fn emit_owned_line(&mut self) -> String {
        let line = self.line_buffer.split().freeze();
        decode_line_lossy(&line).into_owned()
    }
}

pub(crate) struct OwnedLineReader<'a> {
    parser: &'a mut LineParserState,
    chunk: &'a [u8],
    options: LineParsingOptions,
}

impl Iterator for OwnedLineReader<'_> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.chunk.is_empty() {
            match self.parser.mode {
                LineParserMode::DiscardUntilNewline => {
                    if let Some(pos) = memchr(b'\n', self.chunk) {
                        self.parser.mode = LineParserMode::ReadingLine;
                        self.chunk = &self.chunk[pos + 1..];
                    } else {
                        self.chunk = &[];
                        return None;
                    }
                    continue;
                }
                LineParserMode::PendingLimitDelimiter => {
                    self.parser.mode = LineParserMode::ReadingLine;
                    if self.chunk.first() == Some(&b'\n') {
                        self.chunk = &self.chunk[1..];
                        continue;
                    }
                }
                LineParserMode::ReadingLine => {}
            }

            if self.options.max_line_length.0 != 0
                && self.parser.line_buffer.len() == self.options.max_line_length.0
            {
                return match self.options.overflow_behavior {
                    LineOverflowBehavior::DropAdditionalData => {
                        self.parser.mode = LineParserMode::DiscardUntilNewline;
                        Some(self.parser.emit_owned_line())
                    }
                    LineOverflowBehavior::EmitAdditionalAsNewLines => {
                        let line = self.parser.emit_owned_line();
                        self.parser.mode = LineParserMode::PendingLimitDelimiter;
                        Some(line)
                    }
                };
            }

            let remaining_line_length = if self.options.max_line_length.0 == 0 {
                self.chunk.len()
            } else {
                self.options.max_line_length.0 - self.parser.line_buffer.len()
            };
            let scan_len = remaining_line_length.min(self.chunk.len());
            let scan = &self.chunk[..scan_len];

            if let Some(pos) = memchr(b'\n', scan) {
                self.chunk = &self.chunk[pos + 1..];
                if self.parser.line_buffer.is_empty() {
                    return Some(decode_line_lossy(&scan[..pos]).into_owned());
                }
                self.parser.line_buffer.extend_from_slice(&scan[..pos]);
                return Some(self.parser.emit_owned_line());
            }

            self.parser.line_buffer.extend_from_slice(scan);
            self.chunk = &self.chunk[scan_len..];

            if self.options.max_line_length.0 != 0
                && self.parser.line_buffer.len() == self.options.max_line_length.0
                && matches!(
                    self.options.overflow_behavior,
                    LineOverflowBehavior::EmitAdditionalAsNewLines
                )
            {
                let line = self.parser.emit_owned_line();
                self.parser.mode = LineParserMode::PendingLimitDelimiter;
                return Some(line);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::{LineOverflowBehavior, LineParserState, LineParsingOptions};
    use crate::output_stream::Next;
    use crate::{NumBytes, NumBytesExt};
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

        let expected_lines: Vec<String> = expected_lines.iter().map(ToString::to_string).collect();
        assert_that!(collected_lines).is_equal_to(expected_lines);
    }

    fn run_owned_test_case(
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

            for line in parser.owned_lines(chunk, options) {
                collected_lines.push(line);
            }
        }

        if let Some(line) = parser.finish_owned() {
            collected_lines.push(line);
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

        run_owned_test_case(&[b"1234\n"], None, &["1234"], options);
        run_owned_test_case(&[b"1234", b"\n"], None, &["1234"], options);
        run_owned_test_case(&[b"12345678\n"], None, &["1234", "5678"], options);
        run_owned_test_case(&[b"1234\n\n"], None, &["1234", ""], options);
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
