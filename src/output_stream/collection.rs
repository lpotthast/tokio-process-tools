use crate::output_stream::options::NumBytes;
use std::collections::VecDeque;
use std::io;
use typed_builder::TypedBuilder;

/// Controls which output is retained once a bounded in-memory collection reaches its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollectionOverflowBehavior {
    /// Keep the first retained output and discard additional output.
    #[default]
    DropAdditionalData,

    /// Keep the newest retained output by evicting older retained output.
    DropOldestData,
}

/// Action to take after an async writer sink rejects collected output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkWriteErrorAction {
    /// Stop collection and return [`crate::CollectorError::SinkWrite`] from the collector.
    Stop,

    /// Accept the individual write failure and keep collecting later stream output.
    Continue,
}

/// The write operation that failed while forwarding collected output into an async writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkWriteOperation {
    /// A raw output chunk failed to write.
    Chunk,

    /// Parsed line bytes failed to write.
    Line,

    /// The line delimiter requested by [`crate::LineWriteMode::AppendLf`] failed to write.
    LineDelimiter,
}

/// Details about a failed async write into a collector sink.
#[derive(Debug)]
pub struct SinkWriteError {
    stream_name: &'static str,
    operation: SinkWriteOperation,
    attempted_len: usize,
    source: io::Error,
}

impl SinkWriteError {
    pub(crate) fn new(
        stream_name: &'static str,
        operation: SinkWriteOperation,
        attempted_len: usize,
        source: io::Error,
    ) -> Self {
        Self {
            stream_name,
            operation,
            attempted_len,
            source,
        }
    }

    /// The name of the stream this collector operates on.
    #[must_use]
    pub fn stream_name(&self) -> &'static str {
        self.stream_name
    }

    /// The write operation that failed.
    #[must_use]
    pub fn operation(&self) -> SinkWriteOperation {
        self.operation
    }

    /// Number of bytes passed to the failed `write_all` call.
    #[must_use]
    pub fn attempted_len(&self) -> usize {
        self.attempted_len
    }

    /// The underlying async writer error.
    #[must_use]
    pub fn source(&self) -> &io::Error {
        &self.source
    }

    pub(crate) fn into_source(self) -> io::Error {
        self.source
    }
}

/// Handles async writer sink failures observed by writer collectors.
pub trait SinkWriteErrorHandler: Send + 'static {
    /// Decide whether collection should continue after a sink write failure.
    fn handle(&mut self, error: &SinkWriteError) -> SinkWriteErrorAction;
}

impl<F> SinkWriteErrorHandler for F
where
    F: FnMut(&SinkWriteError) -> SinkWriteErrorAction + Send + 'static,
{
    fn handle(&mut self, error: &SinkWriteError) -> SinkWriteErrorAction {
        self(error)
    }
}

/// Default sink write error handler that stops collection on the first write failure.
#[derive(Debug, Default, Clone, Copy)]
pub struct FailOnSinkWriteError;

impl SinkWriteErrorHandler for FailOnSinkWriteError {
    fn handle(&mut self, _error: &SinkWriteError) -> SinkWriteErrorAction {
        SinkWriteErrorAction::Stop
    }
}

/// Sink write error handler that logs every failure and keeps collecting.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogAndContinueSinkWriteErrors;

impl SinkWriteErrorHandler for LogAndContinueSinkWriteErrors {
    fn handle(&mut self, error: &SinkWriteError) -> SinkWriteErrorAction {
        tracing::warn!(
            stream = error.stream_name(),
            operation = ?error.operation(),
            attempted_len = error.attempted_len(),
            source = %error.source(),
            "Could not write collected output to write sink; continuing"
        );
        SinkWriteErrorAction::Continue
    }
}

/// Options for forwarding collected stream output into an async writer.
///
/// Use [`WriteCollectionOptions::fail_fast`] to stop on the first sink write failure,
/// [`WriteCollectionOptions::log_and_continue`] to preserve best-effort logging behavior, or
/// [`WriteCollectionOptions::with_error_handler`] to make a custom per-error decision.
///
/// The handler type is part of this options type so custom handlers remain statically dispatched
/// and allocation-free.
#[derive(Debug, Clone, Copy)]
pub struct WriteCollectionOptions<H = FailOnSinkWriteError> {
    error_handler: H,
}

impl WriteCollectionOptions<FailOnSinkWriteError> {
    /// Creates writer collection options that fail on the first sink write error.
    #[must_use]
    pub fn fail_fast() -> Self {
        Self {
            error_handler: FailOnSinkWriteError,
        }
    }

    /// Creates writer collection options that log sink write errors and keep collecting.
    #[must_use]
    pub fn log_and_continue() -> WriteCollectionOptions<LogAndContinueSinkWriteErrors> {
        WriteCollectionOptions {
            error_handler: LogAndContinueSinkWriteErrors,
        }
    }

    /// Creates writer collection options with a custom sink write error handler.
    #[must_use]
    pub fn with_error_handler<H>(handler: H) -> WriteCollectionOptions<H>
    where
        H: FnMut(&SinkWriteError) -> SinkWriteErrorAction + Send + 'static,
    {
        WriteCollectionOptions {
            error_handler: handler,
        }
    }
}

impl<H> WriteCollectionOptions<H> {
    pub(crate) fn into_error_handler(self) -> H {
        self.error_handler
    }
}

/// Options for collecting raw output bytes into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawCollectionOptions {
    /// Maximum number of bytes retained in memory.
    pub max_bytes: NumBytes,

    /// Which retained bytes to keep when more output is observed.
    pub overflow_behavior: CollectionOverflowBehavior,
}

impl RawCollectionOptions {
    /// Creates bounded raw collection options that retain at most `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: NumBytes) -> Self {
        Self {
            max_bytes,
            overflow_behavior: CollectionOverflowBehavior::default(),
        }
    }

    /// Returns these options with a custom overflow behavior.
    #[must_use]
    pub fn with_overflow_behavior(mut self, overflow_behavior: CollectionOverflowBehavior) -> Self {
        self.overflow_behavior = overflow_behavior;
        self
    }
}

/// Options for collecting parsed output lines into memory.
///
/// The builder requires all fields, including the overflow behavior:
///
/// ```compile_fail
/// use tokio_process_tools::{LineCollectionOptions, NumBytesExt};
///
/// let _ = LineCollectionOptions::builder()
///     .max_bytes(1.megabytes())
///     .max_lines(1024)
///     .build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct LineCollectionOptions {
    /// Maximum total bytes retained across all collected lines.
    pub max_bytes: NumBytes,

    /// Maximum number of lines retained in memory.
    pub max_lines: usize,

    /// Which retained lines to keep when more output is observed.
    pub overflow_behavior: CollectionOverflowBehavior,
}

/// Raw bytes collected from an output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedBytes {
    /// Retained output bytes.
    pub bytes: Vec<u8>,

    /// Whether any bytes were discarded because the configured limit was exceeded.
    pub truncated: bool,
}

impl CollectedBytes {
    /// Creates an empty collected byte buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }

    pub(crate) fn push_chunk(&mut self, chunk: &[u8], options: RawCollectionOptions) {
        let max_bytes = options.max_bytes.bytes();
        match options.overflow_behavior {
            CollectionOverflowBehavior::DropAdditionalData => {
                let remaining = max_bytes.saturating_sub(self.bytes.len());
                if chunk.len() > remaining {
                    self.truncated = true;
                }
                self.bytes
                    .extend_from_slice(&chunk[..remaining.min(chunk.len())]);
            }
            CollectionOverflowBehavior::DropOldestData => {
                if chunk.len() > max_bytes {
                    self.bytes.clear();
                    self.bytes
                        .extend_from_slice(&chunk[chunk.len().saturating_sub(max_bytes)..]);
                    self.truncated = true;
                    return;
                }

                let required = self.bytes.len() + chunk.len();
                if required > max_bytes {
                    self.bytes.drain(0..required - max_bytes);
                    self.truncated = true;
                }
                self.bytes.extend_from_slice(chunk);
            }
        }
    }
}

impl Default for CollectedBytes {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for CollectedBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

/// Parsed lines collected from an output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedLines {
    lines: VecDeque<String>,
    truncated: bool,
    retained_bytes: usize,
}

impl CollectedLines {
    /// Creates an empty collected line buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            truncated: false,
            retained_bytes: 0,
        }
    }

    /// Retained output lines.
    #[must_use]
    pub fn lines(&self) -> &VecDeque<String> {
        &self.lines
    }

    /// Whether any lines were discarded because the configured limit was exceeded.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Converts this collection into its retained output lines.
    #[must_use]
    pub fn into_lines(self) -> VecDeque<String> {
        self.lines
    }

    /// Converts this collection into its retained output lines and truncation flag.
    #[must_use]
    pub fn into_parts(self) -> (VecDeque<String>, bool) {
        (self.lines, self.truncated)
    }

    pub(crate) fn push_line(&mut self, line: String, options: LineCollectionOptions) {
        let line_len = line.len();
        let max_bytes = options.max_bytes.bytes();

        match options.overflow_behavior {
            CollectionOverflowBehavior::DropAdditionalData => {
                if self.lines.len() >= options.max_lines
                    || line_len > max_bytes
                    || line_len > max_bytes.saturating_sub(self.retained_bytes)
                {
                    self.truncated = true;
                    return;
                }
                self.push_back(line);
            }
            CollectionOverflowBehavior::DropOldestData => {
                if options.max_lines == 0 {
                    self.truncated = true;
                    return;
                }
                if line_len > max_bytes {
                    self.truncated = true;
                    return;
                }

                while self.lines.len() >= options.max_lines
                    || line_len > max_bytes.saturating_sub(self.retained_bytes)
                {
                    self.pop_front()
                        .expect("line buffer to contain an evictable line");
                    self.truncated = true;
                }
                self.push_back(line);
            }
        }
    }

    fn push_back(&mut self, line: String) {
        self.retained_bytes += line.len();
        self.lines.push_back(line);
    }

    fn pop_front(&mut self) -> Option<String> {
        let line = self.lines.pop_front()?;
        self.retained_bytes -= line.len();
        Some(line)
    }
}

impl Default for CollectedLines {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for CollectedLines {
    type Target = VecDeque<String>;

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::options::NumBytesExt;
    use assertr::prelude::*;

    fn drop_oldest_options(max_bytes: usize, max_lines: usize) -> LineCollectionOptions {
        LineCollectionOptions::builder()
            .max_bytes(max_bytes.bytes())
            .max_lines(max_lines)
            .overflow_behavior(CollectionOverflowBehavior::DropOldestData)
            .build()
    }

    fn assert_retained_bytes_match_lines(collected: &CollectedLines) {
        assert_that!(collected.retained_bytes)
            .is_equal_to(collected.lines.iter().map(String::len).sum::<usize>());
    }

    #[test]
    fn raw_collection_keeps_prefix_when_dropping_additional_data() {
        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::new(5.bytes());

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"abcde".as_slice());
        assert_that!(collected.truncated).is_true();
    }

    #[test]
    fn raw_collection_keeps_suffix_when_dropping_oldest_data() {
        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::new(5.bytes())
            .with_overflow_behavior(CollectionOverflowBehavior::DropOldestData);

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"bcdef".as_slice());
        assert_that!(collected.truncated).is_true();
    }

    #[test]
    fn line_collection_enforces_line_and_byte_limits() {
        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::builder()
            .max_bytes(7.bytes())
            .max_lines(2)
            .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
            .build();

        collected.push_line("one".to_string(), options);
        collected.push_line("two".to_string(), options);
        collected.push_line("three".to_string(), options);

        assert_that!(
            collected
                .lines()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["one", "two"]);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn line_collection_can_keep_latest_lines() {
        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::builder()
            .max_bytes(6.bytes())
            .max_lines(2)
            .overflow_behavior(CollectionOverflowBehavior::DropOldestData)
            .build();

        collected.push_line("one".to_string(), options);
        collected.push_line("two".to_string(), options);
        collected.push_line("six".to_string(), options);

        assert_that!(
            collected
                .lines()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        )
        .is_equal_to(vec!["two", "six"]);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn retained_bytes_tracks_appended_lines() {
        let options = LineCollectionOptions::builder()
            .max_bytes(100.bytes())
            .max_lines(100)
            .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
            .build();
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);

        assert_that!(collected.retained_bytes).is_equal_to(7);
        assert_retained_bytes_match_lines(&collected);
    }

    #[test]
    fn drop_oldest_preserves_retained_lines_when_oversized_line_arrives() {
        let options = drop_oldest_options(10, 100);
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbb".to_string(), options);
        collected.push_line("x".repeat(13), options);

        assert_that!(collected.lines())
            .with_detail_message(
                "previously-retained lines must survive an oversized incoming line",
            )
            .is_equal_to(VecDeque::from(["aaa".to_string(), "bbb".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(6);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_evicts_old_lines_when_new_line_fits_but_budget_is_exceeded() {
        let options = drop_oldest_options(10, 100);
        let mut collected = CollectedLines::new();

        collected.push_line("aaaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);
        collected.push_line("cccc".to_string(), options);

        assert_that!(collected.lines())
            .is_equal_to(VecDeque::from(["bbbb".to_string(), "cccc".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(8);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_updates_retained_bytes_when_evicting_by_line_count() {
        let options = drop_oldest_options(100, 2);
        let mut collected = CollectedLines::new();

        collected.push_line("a".to_string(), options);
        collected.push_line("bb".to_string(), options);
        collected.push_line("ccc".to_string(), options);

        assert_that!(collected.lines())
            .is_equal_to(VecDeque::from(["bb".to_string(), "ccc".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(5);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_oldest_with_zero_max_lines_retains_nothing() {
        let options = drop_oldest_options(100, 0);
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);

        assert_that!(collected.lines().is_empty()).is_true();
        assert_that!(collected.retained_bytes).is_equal_to(0);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_additional_preserves_retained_lines_when_oversized_line_arrives() {
        let options = LineCollectionOptions::builder()
            .max_bytes(10.bytes())
            .max_lines(100)
            .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
            .build();
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("x".repeat(13), options);

        assert_that!(collected.lines()).is_equal_to(VecDeque::from(["aaa".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(3);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[test]
    fn drop_additional_preserves_retained_bytes_when_limit_rejects_line() {
        let options = LineCollectionOptions::builder()
            .max_bytes(6.bytes())
            .max_lines(100)
            .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
            .build();
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);

        assert_that!(collected.lines()).is_equal_to(VecDeque::from(["aaa".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(3);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }
}
