use std::error::Error;
use std::future::Future;

pub mod broadcast;
pub mod single_subscriber;

pub trait OutputStream {}

/// Represents the type of output stream (stdout or stderr)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    StdOut,
    StdErr,
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
fn process_lines_in_chunk(
    chunk: &[u8],
    line_buffer: &mut String,
    process_line: &mut impl FnMut(String) -> Next,
) -> Next {
    let mut current_chunk = chunk;
    let mut next = Next::Continue;

    while !current_chunk.is_empty() {
        match current_chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - append remaining chunk and exit loop.
                line_buffer.push_str(String::from_utf8_lossy(current_chunk).as_ref());
                break;
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = current_chunk.split_at(pos);
                line_buffer.push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                next = process_line(line_buffer.clone());

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                line_buffer.clear();
                line_buffer.shrink_to(2048);
                current_chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                if next == Next::Break {
                    break;
                }
            }
        }
    }

    next
}

async fn process_lines_in_chunk_async<
    Fut: Future<Output = Result<Next, Box<dyn Error + Send + Sync>>> + Send,
>(
    chunk: &[u8],
    line_buffer: &mut String,
    process_line: &mut impl FnMut(String) -> Fut,
) -> Next {
    let mut current_chunk = chunk;
    let mut next = Next::Continue;

    while !current_chunk.is_empty() {
        match current_chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - append remaining chunk and exit loop.
                line_buffer.push_str(String::from_utf8_lossy(current_chunk).as_ref());
                break;
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = current_chunk.split_at(pos);
                line_buffer.push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                let fut = process_line(line_buffer.clone());
                let result = fut.await;
                match result {
                    Ok(ok) => next = ok,
                    Err(err) => {
                        // TODO: Should this break the inspector?
                        tracing::warn!(?err, "Inspection failed")
                    }
                }

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                line_buffer.clear();
                line_buffer.shrink_to(2048);
                current_chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                if next == Next::Break {
                    break;
                }
            }
        }
    }

    next
}

#[cfg(test)]
mod tests {
    use crate::output_stream::{process_lines_in_chunk, Next};
    use assertr::prelude::*;

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

            let _next = process_lines_in_chunk(chunk, &mut line_buffer, &mut |line: String| {
                collected_lines.push(line);
                Next::Continue
            });

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

            let _next = process_lines_in_chunk(chunk, &mut line_buffer, &mut |line: String| {
                collected_lines.push(line);
                Next::Continue
            });

            assert_that(line_buffer).is_equal_to("");
            assert_that(collected_lines[0].as_str()).contains("valid utf8");
            assert_that(collected_lines[0].as_str()).contains("invalid utf8");
        }
    }
}
