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
    ///
    /// Defaults to `LineOverflowBehavior::DropAdditionalData`.
    pub overflow_behavior: LineOverflowBehavior,

    /// Optional cap on each parser's long-term-retained buffer capacity.
    ///
    /// The parser keeps two `BytesMut` buffers (the in-progress line and the most-recently emitted
    /// line): each retains capacity for the parser's lifetime, growing to fit the largest line they
    /// have ever held. For most workloads this is fine. The worst case is roughly
    /// `2 × `[`max_line_length`](Self::max_line_length) memory used per parser.
    ///
    /// Set this to `Some(n)` when a stream has mostly small lines but occasional large outliers
    /// (especially under [`NumBytes::MAX`] / "trusted unbounded" line parsing) and you want the
    /// buffers to release their allocations after each outlier instead of staying at outlier-size
    /// forever. At the start of each [`LineParser::next_line`](crate::LineParser::next_line) call,
    /// any buffer whose capacity exceeds `n` is dropped and replaced with an empty buffer. The next
    /// line re-grows it from zero.
    ///
    /// `None` (the default) preserves the "no compaction" behavior. Buffers stay at their largest
    /// observed size. A sensible enabled value could be `1.5 × typical_line_size`. Setting it to
    /// close to (or below) typical line sizes will trigger reallocation almost every line and slow
    /// the parser unnecessarily. When your `max_line_length` is already small, you may ignore this
    /// setting if max consumption is not an issue on your system.
    ///
    /// Compaction reduces the parser's *steady-state* memory after outliers; it does not change
    /// the peak. Peak memory is bounded by [`max_line_length`](Self::max_line_length) regardless
    /// of this setting. Compaction is also best-effort: a partially-buffered line that has not
    /// yet finished may briefly retain over-threshold capacity until the line completes.
    ///
    /// Defaults to `None`.
    pub buffer_compaction_threshold: Option<NumBytes>,
}

impl Default for LineParsingOptions {
    fn default() -> Self {
        Self {
            max_line_length: 16.kilobytes(),
            overflow_behavior: LineOverflowBehavior::default(),
            buffer_compaction_threshold: None,
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
