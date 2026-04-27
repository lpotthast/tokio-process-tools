use super::{collect_owned_final_line, visit_lines};
use crate::collector::{AsyncChunkCollector, AsyncLineCollector, Collector, Sink};
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::options::NumBytes;
use crate::output_stream::{Next, Subscription};
use std::borrow::Cow;
use std::collections::VecDeque;

/// Controls which output is retained once a bounded in-memory collection reaches its limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollectionOverflowBehavior {
    /// Keep the first retained output and discard additional output.
    #[default]
    DropAdditionalData,

    /// Keep the newest retained output by evicting older retained output.
    DropOldestData,
}

/// Options for collecting raw output bytes into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawCollectionOptions {
    /// Retain at most `max_bytes` bytes in memory.
    Bounded {
        /// Maximum number of bytes retained in memory.
        max_bytes: NumBytes,

        /// Which retained bytes to keep when more output is observed.
        overflow_behavior: CollectionOverflowBehavior,
    },

    /// Retain all observed bytes in memory without a total output cap.
    ///
    /// Use only when the output source and its output volume are trusted.
    TrustedUnbounded,
}

/// Options for collecting parsed output lines into memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineCollectionOptions {
    /// Retain at most `max_bytes` total line bytes and at most `max_lines` lines in memory.
    Bounded {
        /// Maximum total bytes retained across all collected lines.
        max_bytes: NumBytes,

        /// Maximum number of lines retained in memory.
        max_lines: usize,

        /// Which retained lines to keep when more output is observed.
        overflow_behavior: CollectionOverflowBehavior,
    },

    /// Retain all observed lines in memory without a total output cap.
    ///
    /// Use only when the output source and its output volume are trusted.
    TrustedUnbounded,
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
        match options {
            RawCollectionOptions::TrustedUnbounded => self.bytes.extend_from_slice(chunk),
            RawCollectionOptions::Bounded {
                max_bytes,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            } => {
                let max_bytes = max_bytes.bytes();
                let remaining = max_bytes.saturating_sub(self.bytes.len());
                if chunk.len() > remaining {
                    self.truncated = true;
                }
                self.bytes
                    .extend_from_slice(&chunk[..remaining.min(chunk.len())]);
            }
            RawCollectionOptions::Bounded {
                max_bytes,
                overflow_behavior: CollectionOverflowBehavior::DropOldestData,
            } => {
                let max_bytes = max_bytes.bytes();
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
        match options {
            LineCollectionOptions::TrustedUnbounded => self.push_back(line),
            LineCollectionOptions::Bounded {
                max_bytes,
                max_lines,
                overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
            } => {
                let line_len = line.len();
                let max_bytes = max_bytes.bytes();
                if self.lines.len() >= max_lines
                    || line_len > max_bytes
                    || line_len > max_bytes.saturating_sub(self.retained_bytes)
                {
                    self.truncated = true;
                    return;
                }
                self.push_back(line);
            }
            LineCollectionOptions::Bounded {
                max_bytes,
                max_lines,
                overflow_behavior: CollectionOverflowBehavior::DropOldestData,
            } => {
                let line_len = line.len();
                let max_bytes = max_bytes.bytes();
                if max_lines == 0 {
                    self.truncated = true;
                    return;
                }
                if line_len > max_bytes {
                    self.truncated = true;
                    return;
                }

                while self.lines.len() >= max_lines
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

pub(crate) fn collect_chunks<S, T, F>(
    stream_name: &'static str,
    mut subscription: S,
    into: T,
    mut collect: F,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    F: FnMut(Chunk, &mut T) + Send + 'static,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut sink = into;
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => collect(chunk, &mut sink),
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(sink)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_chunks_async<S, T, C>(
    stream_name: &'static str,
    mut subscription: S,
    into: T,
    mut collect: C,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    C: AsyncChunkCollector<T>,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut sink = into;
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                if collect.collect(chunk, &mut sink).await == Next::Break {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(sink)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines<S, T, F>(
    stream_name: &'static str,
    mut subscription: S,
    into: T,
    mut collect: F,
    options: LineParsingOptions,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    F: FnMut(Cow<'_, str>, &mut T) -> Next + Send + 'static,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut sink = into;
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                if visit_lines(chunk.as_ref(), &mut parser, options, |line| {
                                    collect(line, &mut sink)
                                }) == Next::Break
                                {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    let _next = collect(Cow::Owned(line), &mut sink);
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(sink)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_async<S, T, C>(
    stream_name: &'static str,
    mut subscription: S,
    into: T,
    mut collect: C,
    options: LineParsingOptions,
) -> Collector<T>
where
    S: Subscription,
    T: Sink,
    C: AsyncLineCollector<T>,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut sink = into;
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                let mut should_break = false;
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    if collect.collect(Cow::Owned(line), &mut sink).await
                                        == Next::Break
                                    {
                                        should_break = true;
                                        break;
                                    }
                                }
                                if should_break {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    let _next =
                                        collect.collect(Cow::Owned(line), &mut sink).await;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(sink)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CollectorError;
    use crate::output_stream::options::NumBytesExt;
    use crate::{AsyncChunkCollector, AsyncLineCollector};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::borrow::Cow;
    use std::io;
    use tokio::sync::mpsc;

    async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        rx
    }

    fn drop_oldest_options(max_bytes: usize, max_lines: usize) -> LineCollectionOptions {
        LineCollectionOptions::Bounded {
            max_bytes: max_bytes.bytes(),
            max_lines,
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        }
    }

    fn assert_retained_bytes_match_lines(collected: &CollectedLines) {
        assert_that!(collected.retained_bytes)
            .is_equal_to(collected.lines.iter().map(String::len).sum::<usize>());
    }

    #[test]
    fn raw_collection_keeps_expected_bytes_when_truncated() {
        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::Bounded {
            max_bytes: 5.bytes(),
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"abcde".as_slice());
        assert_that!(collected.truncated).is_true();

        let mut collected = CollectedBytes::new();
        let options = RawCollectionOptions::Bounded {
            max_bytes: 5.bytes(),
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        };

        collected.push_chunk(b"abc", options);
        collected.push_chunk(b"def", options);

        assert_that!(collected.bytes.as_slice()).is_equal_to(b"bcdef".as_slice());
        assert_that!(collected.truncated).is_true();
    }

    #[test]
    fn basic_line_collection_limit_modes() {
        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::Bounded {
            max_bytes: 7.bytes(),
            max_lines: 2,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };

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

        let mut collected = CollectedLines::new();
        let options = LineCollectionOptions::Bounded {
            max_bytes: 6.bytes(),
            max_lines: 2,
            overflow_behavior: CollectionOverflowBehavior::DropOldestData,
        };

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
        let options = LineCollectionOptions::Bounded {
            max_bytes: 100.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
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
        let options = LineCollectionOptions::Bounded {
            max_bytes: 10.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
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
        let options = LineCollectionOptions::Bounded {
            max_bytes: 6.bytes(),
            max_lines: 100,
            overflow_behavior: CollectionOverflowBehavior::DropAdditionalData,
        };
        let mut collected = CollectedLines::new();

        collected.push_line("aaa".to_string(), options);
        collected.push_line("bbbb".to_string(), options);

        assert_that!(collected.lines()).is_equal_to(VecDeque::from(["aaa".to_string()]));
        assert_that!(collected.retained_bytes).is_equal_to(3);
        assert_retained_bytes_match_lines(&collected);
        assert_that!(collected.truncated()).is_true();
    }

    #[tokio::test]
    async fn collectors_return_stream_read_error() {
        let error =
            crate::StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"complete\npartial"))),
                StreamEvent::ReadError(error),
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        match collector.wait().await {
            Err(CollectorError::StreamRead { source, .. }) => {
                assert_that!(source.stream_name()).is_equal_to("custom");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected collector stream read error, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn collectors_skip_gaps_and_keep_final_unterminated_line() {
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\npar"))),
                StreamEvent::Gap,
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"\ntwo\nfinal"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let lines = collector.wait().await.unwrap();
        assert_that!(lines).contains_exactly(["one", "two", "final"]);
    }

    struct ExtendChunks;

    impl AsyncChunkCollector<Vec<u8>> for ExtendChunks {
        async fn collect<'a>(&'a mut self, chunk: Chunk, seen: &'a mut Vec<u8>) -> Next {
            seen.extend_from_slice(chunk.as_ref());
            Next::Continue
        }
    }

    #[tokio::test]
    async fn chunk_collector_async_extends_sink_until_eof() {
        let collector = collect_chunks_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            ExtendChunks,
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).is_equal_to(b"abcdef".to_vec());
    }

    #[tokio::test]
    async fn chunk_collector_accepts_stateful_callback() {
        let mut chunk_index = 0;
        let collector = collect_chunks(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |chunk, indexed_chunks| {
                chunk_index += 1;
                indexed_chunks.push((chunk_index, chunk.as_ref().to_vec()));
            },
        );

        let indexed_chunks = collector.wait().await.unwrap();
        assert_that!(indexed_chunks).is_equal_to(vec![
            (1, b"ab".to_vec()),
            (2, b"cd".to_vec()),
            (3, b"ef".to_vec()),
        ]);
    }

    #[tokio::test]
    async fn line_collector_accepts_stateful_callback() {
        let mut line_index = 0;
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"alpha\nbeta\ngamma\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |line, indexed_lines| {
                line_index += 1;
                indexed_lines.push(format!("{line_index}:{line}"));
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let indexed_lines = collector.wait().await.unwrap();
        assert_that!(indexed_lines).is_equal_to(vec![
            "1:alpha".to_string(),
            "2:beta".to_string(),
            "3:gamma".to_string(),
        ]);
    }

    struct BreakOnLine;

    impl AsyncLineCollector<Vec<String>> for BreakOnLine {
        async fn collect<'a>(&'a mut self, line: Cow<'a, str>, seen: &'a mut Vec<String>) -> Next {
            if line == "break" {
                seen.push(line.into_owned());
                Next::Break
            } else {
                seen.push(line.into_owned());
                Next::Continue
            }
        }
    }

    #[tokio::test]
    async fn line_collector_async_break_stops_after_requested_line() {
        let collector = collect_lines_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"start\nbreak\nend\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            BreakOnLine,
            LineParsingOptions::default(),
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).contains_exactly(["start", "break"]);
    }
}
