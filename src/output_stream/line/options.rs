//! Configuration for the line parser: maximum line length and how to handle overflow.

use crate::output_stream::num_bytes::NumBytes;
use typed_builder::TypedBuilder;

/// Default maximum line length used by [`LineParsingOptions::default`]. 16 kilobytes.
pub const DEFAULT_MAX_LINE_LENGTH: NumBytes = NumBytes(16 * 1024);

/// Behavior applied when a parsed line reaches the configured maximum length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineOverflowBehavior {
    /// Emit the current line truncated at the limit and drop all further data until the next
    /// newline character is observed, which then starts a new line.
    ///
    /// The discard state persists across chunk boundaries. Once the limit is reached, subsequent
    /// bytes are ignored until a real newline is observed.
    #[default]
    DropAdditionalData,

    /// Emit the current line when the maximum allowed length is reached and treat any additional
    /// data as the content of the next line.
    ///
    /// In effect, this inserts artificial line breaks so that no emitted line exceeds the length
    /// limit. No data is dropped with this behavior.
    EmitAdditionalAsNewLines,
}

/// Configuration options for parsing lines from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct LineParsingOptions {
    /// Maximum length of a single line in bytes.
    ///
    /// When reached, the current line is emitted at the limit and any further data is handled
    /// according to [`overflow_behavior`](Self::overflow_behavior).
    ///
    /// **Must be greater than zero** for any line-consuming visitor built via
    /// [`ParseLines`](crate::ParseLines) (and the backends' `wait_for_line` helpers).
    /// Constructing such a consumer with `max_line_length = 0` panics. If you want effectively
    /// unbounded line parsing, i.e. accept arbitrarily long lines from a trusted source, pass
    /// [`NumBytes::MAX`] explicitly instead of zero. Remember that a malicious or misbehaving
    /// stream that writes endless data without a line break would otherwise hold memory until
    /// the process runs out: the explicit `MAX` makes that decision visible at the call site.
    ///
    /// Defaults to [`DEFAULT_MAX_LINE_LENGTH`].
    pub max_line_length: NumBytes,

    /// How to handle a line that reaches [`max_line_length`](Self::max_line_length): drop the
    /// overflowing data, or emit it as additional lines. See [`LineOverflowBehavior`].
    ///
    /// Defaults to [`LineOverflowBehavior::DropAdditionalData`].
    pub overflow_behavior: LineOverflowBehavior,

    /// Optional cap on each parser's long-term-retained buffer capacity.
    ///
    /// The parser keeps two `BytesMut` buffers (the in-progress line and the most-recently emitted
    /// line). Each retains capacity for the parser's lifetime, growing to fit the largest line it
    /// has ever held. For most workloads this is fine. The worst case is roughly
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
    /// Compaction reduces the parser's *steady-state* memory after outliers. It does not change
    /// the peak: peak per-parser memory stays at roughly
    /// `2 × `[`max_line_length`](Self::max_line_length) regardless of this setting. Compaction is
    /// also best-effort: a partially-buffered line that has not yet finished may briefly retain
    /// over-threshold capacity until the line completes.
    ///
    /// Defaults to `None`.
    pub buffer_compaction_threshold: Option<NumBytes>,
}

impl Default for LineParsingOptions {
    fn default() -> Self {
        Self {
            max_line_length: DEFAULT_MAX_LINE_LENGTH,
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
        "LineParsingOptions::max_line_length must be greater than zero. If you want effectively \
        unbounded line parsing, pass `NumBytes::MAX` (or another large explicit value). Zero is \
        never a valid configuration for line-consuming visitors."
    );
}
