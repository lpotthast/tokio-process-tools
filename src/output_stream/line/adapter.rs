//! Adapter that turns a chunk-level [`StreamVisitor`] / [`AsyncStreamVisitor`] into a
//! line-level sink.
//!
//! Six visitors used to drive the same five-step dance independently:
//!
//! 1. Hold a [`LineParser`] + [`LineParsingOptions`].
//! 2. On every chunk, feed bytes into the parser and dispatch each emitted line to the
//!    visitor-specific per-line callback.
//! 3. On a gap, reset the parser's partial-line buffer.
//! 4. On EOF, flush any unterminated trailing line through the same callback.
//! 5. Assert at construction time that `options.max_line_length` is non-zero.
//!
//! [`LineAdapter`] owns steps 1–5 in one place; each line-aware visitor (`InspectLines`,
//! `CollectLines`, `WaitForLine`, `WriteLines`, plus their async twins) collapses to an inner
//! [`LineSink`] / [`AsyncLineSink`] that only describes the per-line action. Adding a new
//! line-aware visitor is now "implement [`LineSink`] (or [`AsyncLineSink`]) and compose with
//! [`LineAdapter`]," not "re-derive the parser plumbing for the seventh time."
//!
//! A single [`LineAdapter<S>`] struct serves both the sync and async paths: it carries two
//! trait impls — [`StreamVisitor`] when `S: LineSink`, [`AsyncStreamVisitor`] when
//! `S: AsyncLineSink` — and Rust selects the right one based on which trait the inner sink
//! implements. Implementing both [`LineSink`] and [`AsyncLineSink`] for the same sink is
//! supported but creates ambiguity at the [`LineAdapter`] call site: the caller has to make
//! the trait selection explicit (e.g., by which consumer driver they hand the adapter to).
//! In practice you implement whichever trait matches the work the sink does — sync if
//! `on_line` is non-blocking, async if it `.await`s.
//!
//! Encoding the `max_line_length > 0` invariant as a `NonZero*` newtype was rejected on
//! ergonomic grounds: every caller of `LineParsingOptions` would inherit the wrapper, even
//! when they construct it from a known-non-zero literal. The runtime assert lives in
//! [`super::options`] as the single source of that check; the constraint is documented on
//! [`LineParsingOptions::max_line_length`].

use super::options::{LineParsingOptions, assert_max_line_length_non_zero};
use super::parser::LineParser;
use crate::output_stream::Next;
use crate::output_stream::event::Chunk;
use crate::output_stream::visitor::{AsyncStreamVisitor, StreamVisitor};
use std::borrow::Cow;
use std::future::Future;

/// Per-line action selected by the [`StreamVisitor`] impl of [`LineAdapter`].
///
/// Implementors only describe what should happen for each parsed line; chunk parsing, gap
/// handling, and EOF flushing are carried by the adapter.
pub trait LineSink: Send + 'static {
    /// Final value produced once the adapter is finished. Returned via
    /// [`Consumer::wait`](crate::Consumer::wait).
    type Output: Send + 'static;

    /// Invoked for every parsed line. Return [`Next::Break`] to stop further parsing.
    fn on_line(&mut self, line: Cow<'_, str>) -> Next;

    /// Invoked after the adapter resets the parser following a stream gap. The default does
    /// nothing; override when the inner sink keeps line-spanning state that the gap
    /// invalidates.
    fn on_gap(&mut self) {}

    /// Invoked once the adapter finishes flushing any trailing line at EOF. The default does
    /// nothing; override for sinks that want a finalization hook beyond the last `on_line`
    /// call.
    fn on_eof(&mut self) {}

    /// Consumes the sink and returns its final output.
    fn into_output(self) -> Self::Output;
}

/// Async per-line action selected by the [`AsyncStreamVisitor`] impl of [`LineAdapter`].
///
/// `on_line` is async because the line-aware async visitors (`inspect_lines_async`,
/// `collect_lines_async`, `collect_lines_into_write*`) all need to `.await` per-line work.
/// `on_gap` stays synchronous because gap notification carries no payload to await on,
/// mirroring [`AsyncStreamVisitor::on_gap`].
pub trait AsyncLineSink: Send + 'static {
    /// Final value produced once the adapter is finished.
    type Output: Send + 'static;

    /// Asynchronously observes a single parsed line. Return [`Next::Break`] to stop further
    /// parsing.
    fn on_line<'a>(&'a mut self, line: Cow<'a, str>) -> impl Future<Output = Next> + Send + 'a;

    /// Synchronous gap hook; default no-op. See [`LineSink::on_gap`].
    fn on_gap(&mut self) {}

    /// Asynchronous EOF hook; default no-op. Invoked after the adapter has flushed any
    /// trailing line through `on_line`.
    fn on_eof(&mut self) -> impl Future<Output = ()> + Send + '_ {
        async {}
    }

    /// Consumes the sink and returns its final output.
    fn into_output(self) -> Self::Output;
}

/// Adapter that drives [`LineParser`] over chunk events and dispatches every emitted line to
/// the inner [`LineSink`] (sync) or [`AsyncLineSink`] (async).
///
/// One struct, two trait impls: [`StreamVisitor`] when `S: LineSink`, [`AsyncStreamVisitor`]
/// when `S: AsyncLineSink`. Rust selects the right impl from the inner sink's type at the
/// call site.
pub struct LineAdapter<S> {
    parser: LineParser,
    options: LineParsingOptions,
    inner: S,
}

impl<S> LineAdapter<S> {
    /// Creates a new line adapter.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero. See
    /// [`LineParsingOptions::max_line_length`] for the rationale; pass
    /// [`crate::NumBytes::MAX`] for effectively-unbounded line parsing.
    pub fn new(options: LineParsingOptions, inner: S) -> Self {
        assert_max_line_length_non_zero(&options);
        Self {
            parser: LineParser::new(),
            options,
            inner,
        }
    }
}

impl<S: LineSink> StreamVisitor for LineAdapter<S> {
    type Output = S::Output;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            inner,
        } = self;
        let mut bytes: &[u8] = chunk.as_ref();
        while let Some(line) = parser.next_line(&mut bytes, *options) {
            if inner.on_line(line) == Next::Break {
                return Next::Break;
            }
        }
        Next::Continue
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
        self.inner.on_gap();
    }

    fn on_eof(&mut self) {
        if let Some(line) = self.parser.finish() {
            let _ = self.inner.on_line(line);
        }
        self.inner.on_eof();
    }

    fn into_output(self) -> Self::Output {
        self.inner.into_output()
    }
}

/// Async impl of the same [`LineAdapter`] struct.
///
/// Each per-line iteration calls `next_line` synchronously, materializes the line as a fresh
/// `String` via `Cow::into_owned`, drops the parser borrow, then awaits the inner sink. The
/// allocation per line is the price of supporting async per-line callbacks on stable Rust —
/// holding a parser borrow across an `.await` is forbidden because the next iteration
/// re-borrows the parser.
impl<S: AsyncLineSink> AsyncStreamVisitor for LineAdapter<S> {
    type Output = S::Output;

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            inner,
        } = self;
        let mut bytes: &[u8] = chunk.as_ref();
        loop {
            let line = match parser.next_line(&mut bytes, *options) {
                Some(line) => line.into_owned(),
                None => return Next::Continue,
            };
            if inner.on_line(Cow::Owned(line)).await == Next::Break {
                return Next::Break;
            }
        }
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
        self.inner.on_gap();
    }

    async fn on_eof(&mut self) {
        let trailing = self.parser.finish().map(Cow::into_owned);
        if let Some(line) = trailing {
            let _ = self.inner.on_line(Cow::Owned(line)).await;
        }
        self.inner.on_eof().await;
    }

    fn into_output(self) -> Self::Output {
        self.inner.into_output()
    }
}

#[cfg(test)]
mod tests {
    use super::super::options::LineOverflowBehavior;
    use super::*;
    use crate::NumBytesExt;
    use crate::output_stream::consumer::{spawn_consumer_async, spawn_consumer_sync};
    use crate::output_stream::event::StreamEvent;
    use crate::output_stream::event::tests::event_receiver;
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    struct CollectingSink {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl LineSink for CollectingSink {
        type Output = ();

        fn on_line(&mut self, line: Cow<'_, str>) -> Next {
            self.seen.lock().unwrap().push(line.into_owned());
            Next::Continue
        }

        fn into_output(self) -> Self::Output {}
    }

    struct CollectingAsyncSink {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl AsyncLineSink for CollectingAsyncSink {
        type Output = ();

        async fn on_line(&mut self, line: Cow<'_, str>) -> Next {
            self.seen.lock().unwrap().push(line.into_owned());
            Next::Continue
        }

        fn into_output(self) -> Self::Output {}
    }

    mod sync {
        use super::*;

        #[test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        fn new_panics_when_max_line_length_is_zero() {
            let _ = LineAdapter::new(
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: LineOverflowBehavior::default(),
                    buffer_compaction_threshold: None,
                },
                CollectingSink {
                    seen: Arc::new(Mutex::new(Vec::new())),
                },
            );
        }

        #[tokio::test]
        async fn flushes_trailing_unterminated_line_at_eof() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let consumer = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"first\nsec"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ond\nthird"))),
                    StreamEvent::Eof,
                ])
                .await,
                LineAdapter::new(
                    LineParsingOptions::default(),
                    CollectingSink {
                        seen: Arc::clone(&seen),
                    },
                ),
            );

            consumer.wait().await.unwrap();
            assert_that!(seen.lock().unwrap().clone())
                .contains_exactly(["first", "second", "third"]);
        }

        #[tokio::test]
        async fn gap_discards_partial_line() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let consumer = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"par"))),
                    StreamEvent::Gap,
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"tial\nclean\n"))),
                    StreamEvent::Eof,
                ])
                .await,
                LineAdapter::new(
                    LineParsingOptions::default(),
                    CollectingSink {
                        seen: Arc::clone(&seen),
                    },
                ),
            );

            consumer.wait().await.unwrap();
            assert_that!(seen.lock().unwrap().clone()).contains_exactly(["clean"]);
        }

        #[tokio::test]
        async fn break_from_inner_stops_parsing_immediately() {
            struct StopAtSecondLine {
                seen: Arc<Mutex<Vec<String>>>,
                count: usize,
            }

            impl LineSink for StopAtSecondLine {
                type Output = ();
                fn on_line(&mut self, line: Cow<'_, str>) -> Next {
                    self.count += 1;
                    self.seen.lock().unwrap().push(line.into_owned());
                    if self.count == 2 {
                        Next::Break
                    } else {
                        Next::Continue
                    }
                }
                fn into_output(self) -> Self::Output {}
            }

            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let consumer = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"a\nb\nc\nd\n"))),
                    StreamEvent::Eof,
                ])
                .await,
                LineAdapter::new(
                    LineParsingOptions::default(),
                    StopAtSecondLine {
                        seen: Arc::clone(&seen),
                        count: 0,
                    },
                ),
            );

            consumer.wait().await.unwrap();
            assert_that!(seen.lock().unwrap().clone()).contains_exactly(["a", "b"]);
        }
    }

    mod r#async {
        use super::*;

        #[test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        fn new_panics_when_max_line_length_is_zero() {
            let _ = LineAdapter::new(
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: LineOverflowBehavior::default(),
                    buffer_compaction_threshold: None,
                },
                CollectingAsyncSink {
                    seen: Arc::new(Mutex::new(Vec::new())),
                },
            );
        }

        #[tokio::test]
        async fn flushes_trailing_unterminated_line_at_eof() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let consumer = spawn_consumer_async(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"first\ntail"))),
                    StreamEvent::Eof,
                ])
                .await,
                LineAdapter::new(
                    LineParsingOptions::default(),
                    CollectingAsyncSink {
                        seen: Arc::clone(&seen),
                    },
                ),
            );

            consumer.wait().await.unwrap();
            assert_that!(seen.lock().unwrap().clone()).contains_exactly(["first", "tail"]);
        }
    }
}
