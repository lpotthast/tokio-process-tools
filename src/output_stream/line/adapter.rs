//! Adapter that turns a chunk-level [`StreamVisitor`] / [`AsyncStreamVisitor`] into a
//! line-level [`LineVisitor`] / [`AsyncLineVisitor`].
//!
//! Each line-aware visitor ([`InspectLines`], [`CollectLines`],
//! [`crate::visitors::wait::WaitForLine`], [`crate::visitors::write::WriteLines`],
//! plus their async twins) collapses to an inner  [`LineVisitor`] / [`AsyncLineVisitor`] that only
//! describes the per-line action. Adding a new line-aware visitor is now "implement [`LineVisitor`]
//! (or [`AsyncLineVisitor`]) and compose with [`ParseLines`]," not "re-derive the parser plumbing
//! for the seventh time."
//!
//! A single [`ParseLines<S>`] struct serves both the sync and async paths: it carries two
//! trait impls ([`StreamVisitor`] when `S: LineVisitor`, [`AsyncStreamVisitor`] when
//! `S: AsyncLineVisitor`), and Rust selects the right one based on which trait the inner sink
//! implements. Implementing both [`LineVisitor`] and [`AsyncLineVisitor`] for the same sink is
//! supported but creates ambiguity at the [`ParseLines`] call site: the caller has to make
//! the trait selection explicit (e.g., by which consumer driver they hand the adapter to).
//! In practice, you implement whichever trait matches the work the sink does: sync if
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
use crate::output_stream::consumer::Sink;
use crate::output_stream::event::Chunk;
use crate::output_stream::visitor::{AsyncStreamVisitor, StreamVisitor};
use crate::output_stream::visitors::collect::{CollectLines, CollectLinesAsync};
use crate::output_stream::visitors::inspect::{InspectLines, InspectLinesAsync};
use std::borrow::Cow;
use std::future::Future;

/// Per-line action selected by the [`StreamVisitor`] impl of [`ParseLines`].
///
/// Implementors only describe what should happen for each parsed line; chunk parsing, gap
/// handling, and EOF flushing are carried by the adapter.
pub trait LineVisitor: Send + 'static {
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
    #[must_use]
    fn into_output(self) -> Self::Output;
}

/// Async per-line action selected by the [`AsyncStreamVisitor`] impl of [`ParseLines`].
///
/// `on_line` is async because async per-line work (writing to an async sink, awaiting a
/// channel) needs `.await`. `on_gap` stays synchronous because gap notification carries no
/// payload to await on, mirroring [`AsyncStreamVisitor::on_gap`].
///
/// # Allocation note
///
/// The async path materializes every parsed line as an owned `String` before handing it to
/// `on_line`. The synchronous [`LineVisitor`] path can instead pass a `Cow::Borrowed` straight out
/// of the chunk on the fast path, so it avoids a per-line allocation when the line fits in the
/// current chunk. The allocation is the cost of holding the parser's borrow across an `.await`,
/// which Rust does not allow because the next iteration re-borrows the parser. Prefer
/// [`LineVisitor`] (composed via [`ParseLines::inspect`] / [`ParseLines::collect`]) when
/// per-line work is non-blocking, and you want the zero-copy fast path; reach for
/// [`AsyncLineVisitor`] only when the per-line work genuinely needs to await.
pub trait AsyncLineVisitor: Send + 'static {
    /// Final value produced once the adapter is finished.
    type Output: Send + 'static;

    /// Asynchronously observes a single parsed line. Return [`Next::Break`] to stop further
    /// parsing.
    fn on_line<'a>(&'a mut self, line: Cow<'a, str>) -> impl Future<Output = Next> + Send + 'a;

    /// Synchronous gap hook; default no-op. See [`LineVisitor::on_gap`].
    fn on_gap(&mut self) {}

    /// Asynchronous EOF hook; default no-op. Invoked after the adapter has flushed any
    /// trailing line through `on_line`.
    fn on_eof(&mut self) -> impl Future<Output = ()> + Send + '_ {
        async {}
    }

    /// Consumes the sink and returns its final output.
    #[must_use]
    fn into_output(self) -> Self::Output;
}

/// Adapter that drives a [`LineParser`] over chunk events and dispatches every emitted line to
/// the `inner` [`LineVisitor`] (sync) or [`AsyncLineVisitor`] (async).
///
/// Implements [`StreamVisitor`] when `S: LineVisitor` and [`AsyncStreamVisitor`] when
/// `S: AsyncLineVisitor`.
pub struct ParseLines<S> {
    parser: LineParser,
    options: LineParsingOptions,
    inner: S,
}

impl<S> ParseLines<S> {
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

impl<F> ParseLines<InspectLines<F>>
where
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    /// Convenience constructor: wraps `f` in an [`InspectLines`] and composes it with this
    /// adapter. Equivalent to `ParseLines::new(options, InspectLines::new(f))`.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn inspect(options: LineParsingOptions, f: F) -> Self {
        Self::new(options, InspectLines::new(f))
    }
}

impl<F, Fut> ParseLines<InspectLinesAsync<F, Fut>>
where
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    /// Convenience constructor: wraps `f` in an [`InspectLinesAsync`] and composes it with this
    /// adapter. Equivalent to `ParseLines::new(options, InspectLinesAsync::new(f))`.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn inspect_async(options: LineParsingOptions, f: F) -> Self {
        Self::new(options, InspectLinesAsync::new(f))
    }
}

impl<T, F> ParseLines<CollectLines<T, F>>
where
    T: Sink,
    F: FnMut(Cow<'_, str>, &mut T) -> Next + Send + 'static,
{
    /// Convenience constructor: wraps `sink` and `f` in a [`CollectLines`] and composes it with
    /// this adapter.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn collect(options: LineParsingOptions, sink: T, f: F) -> Self {
        Self::new(options, CollectLines::new(sink, f))
    }
}

impl<T, F> ParseLines<CollectLinesAsync<T, F>>
where
    T: Sink,
    F: for<'a> FnMut(
            Cow<'a, str>,
            &'a mut T,
        ) -> std::pin::Pin<Box<dyn Future<Output = Next> + Send + 'a>>
        + Send
        + 'static,
{
    /// Convenience constructor: wraps `sink` and `f` in a [`CollectLinesAsync`] and composes it
    /// with this adapter. The closure must wrap its async body in `Box::pin(async move { ... })`.
    ///
    /// # Panics
    ///
    /// Panics if `options.max_line_length` is zero.
    pub fn collect_async(options: LineParsingOptions, sink: T, f: F) -> Self {
        Self::new(options, CollectLinesAsync::new(sink, f))
    }
}

impl<S: LineVisitor> StreamVisitor for ParseLines<S> {
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

impl<S: AsyncLineVisitor> AsyncStreamVisitor for ParseLines<S> {
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

    impl LineVisitor for CollectingSink {
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

    impl AsyncLineVisitor for CollectingAsyncSink {
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
            let _ = ParseLines::new(
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
                ParseLines::new(
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
                ParseLines::new(
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

            impl LineVisitor for StopAtSecondLine {
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
                ParseLines::new(
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
            let _ = ParseLines::new(
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
                ParseLines::new(
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
