//! Configuration for the line parser: maximum line length and how to handle overflow.

use crate::output_stream::num_bytes::{NumBytes, NumBytesExt};
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
    /// **Must be greater than zero** for any line-consuming visitor (`inspect_lines`,
    /// `collect_lines`, `wait_for_line`, `collect_lines_into_write`, `collect_lines_into_vec`).
    /// Constructing such a consumer with `max_line_length = 0` panics. If you want effectively
    /// unbounded line parsing — i.e. accept arbitrarily long lines from a trusted source —
    /// pass [`NumBytes::MAX`] explicitly instead of zero. Remember that a malicious or
    /// misbehaving stream that writes endless data without a line break would otherwise hold
    /// memory until the process runs out: the explicit `MAX` makes that decision visible at
    /// the call site.
    ///
    /// Defaults to 16 kilobytes.
    pub max_line_length: NumBytes,

    /// What should happen when a line is too long?
    ///
    /// When lossy buffering drops chunks before they reach the parser, line-based consumers
    /// conservatively discard any partial line and resynchronizes at the next newline instead of
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

/// Asserts that `options.max_line_length` is non-zero. Funneled through one location so the
/// invariant is checked once per visitor at construction time, not on every chunk.
pub(crate) fn assert_max_line_length_non_zero(options: &LineParsingOptions) {
    assert!(
        options.max_line_length.bytes() > 0,
        "LineParsingOptions::max_line_length must be greater than zero. \
         If you want effectively unbounded line parsing, pass `NumBytes::MAX` (or another \
         large explicit value) — zero is never a valid configuration for line-consuming \
         visitors."
    );
}
