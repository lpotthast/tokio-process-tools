use crate::output_stream::line::LineSink;
use crate::output_stream::Next;
use std::borrow::Cow;

/// [`LineSink`] that breaks the moment a predicate accepts a line and remembers whether it
/// has matched yet. Compose with
/// [`LineAdapter`](crate::output_stream::line::LineAdapter) to drive `wait_for_line`, or to
/// build your own custom predicate-driven consumer outside the built-in factory methods.
pub struct WaitForLineSink<P> {
    predicate: P,
    matched: bool,
}

impl<P> WaitForLineSink<P>
where
    P: Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
{
    /// Creates a new sink that breaks the parser the first time `predicate` returns `true`.
    pub fn new(predicate: P) -> Self {
        Self {
            predicate,
            matched: false,
        }
    }
}

impl<P> LineSink for WaitForLineSink<P>
where
    P: Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
{
    type Output = bool;

    fn on_line(&mut self, line: Cow<'_, str>) -> Next {
        if (self.predicate)(line) {
            self.matched = true;
            Next::Break
        } else {
            Next::Continue
        }
    }

    fn into_output(self) -> Self::Output {
        self.matched
    }
}

#[cfg(test)]
mod tests {
    use crate::output_stream::line::LineAdapter;
    use super::super::super::visitor::consume_sync;
    use super::*;
    use crate::output_stream::event::{Chunk, StreamEvent};
    use crate::output_stream::line::LineParsingOptions;
    use crate::{LineOverflowBehavior, NumBytesExt, StreamReadError, WaitForLineResult};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};

    /// Drive a `WaitForLineSink` over the supplied events and translate the visitor's `bool`
    /// output into [`WaitForLineResult`]. Mirrors what the deleted `wait_for_line` factory
    /// used to do; lives in tests because production code now drives the visitor straight from
    /// the backend method.
    async fn drive_wait_for_line(
        events: Vec<StreamEvent>,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> Result<WaitForLineResult, StreamReadError> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);

        let (_term_sig_tx, term_sig_rx) = oneshot::channel::<()>();
        let visitor = LineAdapter::new(options, WaitForLineSink::new(predicate));
        let matched = consume_sync(rx, visitor, term_sig_rx).await?;
        if matched {
            Ok(WaitForLineResult::Matched)
        } else {
            Ok(WaitForLineResult::StreamClosed)
        }
    }

    async fn wait_for_ready(
        events: Vec<StreamEvent>,
    ) -> Result<WaitForLineResult, StreamReadError> {
        drive_wait_for_line(
            events,
            |line| line == "ready",
            LineParsingOptions::default(),
        )
        .await
    }

    mod wait_for_line {
        use super::*;

        #[tokio::test]
        async fn matches_intermediary_line() {
            let result = wait_for_ready(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"booting\nready\n"))),
                StreamEvent::Eof,
            ])
            .await;
            assert_that!(result)
                .is_ok()
                .is_equal_to(WaitForLineResult::Matched);
        }

        #[tokio::test]
        async fn matches_final_line() {
            let result = wait_for_ready(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"booting\nready"))),
                StreamEvent::Eof,
            ])
            .await;
            assert_that!(result)
                .is_ok()
                .is_equal_to(WaitForLineResult::Matched);
        }

        #[tokio::test]
        async fn returns_stream_closed_when_expected_is_not_matched_before_eof() {
            let result = wait_for_ready(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"booting\nstill starting\n"))),
                StreamEvent::Eof,
            ])
            .await;
            assert_that!(result)
                .is_ok()
                .is_equal_to(WaitForLineResult::StreamClosed);
        }

        #[tokio::test]
        async fn gap_does_not_join_lines() {
            let result = wait_for_ready(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"rea"))),
                StreamEvent::Gap,
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"dy\n"))),
                StreamEvent::Eof,
            ])
            .await;
            assert_that!(result)
                .is_ok()
                .is_equal_to(WaitForLineResult::StreamClosed);
        }

        #[tokio::test]
        async fn reports_read_error() {
            let result = wait_for_ready(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"booting\npartial"))),
                StreamEvent::ReadError(StreamReadError::new(
                    "custom",
                    io::Error::from(io::ErrorKind::BrokenPipe),
                )),
            ])
            .await;

            let err = result.expect_err("read failure should be surfaced");
            assert_that!(err.stream_name()).is_equal_to("custom");
            assert_that!(err.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
        }

        #[test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        fn panics_when_max_line_length_is_zero() {
            let _visitor = LineAdapter::new(
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: LineOverflowBehavior::default(),
                    buffer_compaction_threshold: None,
                },
                WaitForLineSink::new(|_line| true),
            );
        }

        #[tokio::test]
        async fn honors_line_parsing_options() {
            let result = drive_wait_for_line(
                vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"readiness\n"))),
                    StreamEvent::Eof,
                ],
                |line| line == "read",
                LineParsingOptions {
                    max_line_length: 4.bytes(),
                    overflow_behavior: LineOverflowBehavior::DropAdditionalData,
                    buffer_compaction_threshold: None,
                },
            )
            .await;

            assert_that!(result)
                .is_ok()
                .is_equal_to(WaitForLineResult::Matched);
        }
    }

    mod wait_for_line_bounded {
        use super::*;

        #[tokio::test]
        async fn times_out_with_timeout_error() {
            let (_tx, rx) = mpsc::channel::<StreamEvent>(1);
            let (_term_sig_tx, term_sig_rx) = oneshot::channel::<()>();
            let visitor = LineAdapter::new(
                LineParsingOptions::default(),
                WaitForLineSink::new(|line| line == "ready"),
            );
            let timeout = tokio::time::timeout(
                Duration::from_millis(25),
                consume_sync(rx, visitor, term_sig_rx),
            )
            .await
            .map_or(Ok(WaitForLineResult::Timeout), |inner| {
                inner.map(|matched| {
                    if matched {
                        WaitForLineResult::Matched
                    } else {
                        WaitForLineResult::StreamClosed
                    }
                })
            });
            assert_that!(timeout).is_equal_to(Ok(WaitForLineResult::Timeout));
        }
    }
}
