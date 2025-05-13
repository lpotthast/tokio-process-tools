use std::borrow::Cow;

pub mod broadcast;
pub mod single_subscriber;

/// We support the following implementations:
///
/// - [BroadcastOutputStream]
/// - [crate::output_stream::broadcast::SingleOutputStream]
pub trait OutputStream {}

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

/// Processes a byte chunk, handling line breaks and applying a callback function to complete lines.
///
/// This function takes a byte slice and appends it to the current line buffer. When it encounters
/// newline characters, it calls the callback function with the completed line and continues
/// processing the remainder of the chunk.
///
/// # Parameters
/// * `chunk` - Byte slice to process
/// * `line_buffer` - Current accumulated line content
/// * `process_line` - Callback function to apply to completed lines
///
/// # Returns
/// The updated line buffer with any remaining content after the last newline
pub(crate) struct LineReader<'c, 'b> {
    remaining_chunk: &'c [u8],
    line_buffer: &'b mut String,
}

impl Iterator for LineReader<'_, '_> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_chunk.is_empty() {
            return None;
        }

        match self.remaining_chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - consume the remaining chunk.
                self.line_buffer
                    .push_str(String::from_utf8_lossy(self.remaining_chunk).as_ref());
                self.remaining_chunk = &[];
                None
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = self.remaining_chunk.split_at(pos);
                self.line_buffer
                    .push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                let to_return = self.line_buffer.clone();

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                self.line_buffer.clear();
                self.line_buffer.shrink_to(2048);
                self.remaining_chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

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
                remaining_chunk: chunk,
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
                remaining_chunk: chunk,
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
