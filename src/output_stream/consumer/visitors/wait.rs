use super::super::visitor::StreamVisitor;
use super::inspect::assert_max_line_length_non_zero;
use crate::output_stream::Next;
use crate::output_stream::event::Chunk;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use std::borrow::Cow;
use typed_builder::TypedBuilder;

#[derive(TypedBuilder)]
pub(crate) struct WaitForLine<P>
where
    P: Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
{
    pub parser: LineParserState,
    #[builder(setter(transform = |options: LineParsingOptions| {
        assert_max_line_length_non_zero(&options);
        options
    }))]
    pub options: LineParsingOptions,
    pub predicate: P,
    pub matched: bool,
}

impl<P> StreamVisitor for WaitForLine<P>
where
    P: Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
{
    type Output = bool;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            predicate,
            matched,
        } = self;
        parser.visit_chunk(chunk.as_ref(), *options, |line| {
            if predicate(line) {
                *matched = true;
                Next::Break
            } else {
                Next::Continue
            }
        })
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
    }

    fn on_eof(&mut self) {
        let Self {
            parser,
            predicate,
            matched,
            ..
        } = self;
        let _ = parser.finish(|line| {
            if predicate(line) {
                *matched = true;
                Next::Break
            } else {
                Next::Continue
            }
        });
    }

    fn into_output(self) -> Self::Output {
        self.matched
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::visitor::consume_sync;
    use super::*;
    use crate::output_stream::event::{Chunk, StreamEvent};
    use crate::{LineOverflowBehavior, NumBytesExt, StreamReadError, WaitForLineResult};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};

    /// Drive a `WaitForLine` visitor over the supplied events and translate the visitor's
    /// `bool` output into [`WaitForLineResult`]. Mirrors what the deleted `wait_for_line`
    /// factory used to do; lives in tests because production code now drives the visitor
    /// straight from the backend method.
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
        let visitor = WaitForLine::builder()
            .parser(LineParserState::new())
            .options(options)
            .predicate(predicate)
            .matched(false)
            .build();
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
            let _visitor = WaitForLine::builder()
                .parser(LineParserState::new())
                .options(LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: LineOverflowBehavior::default(),
                })
                .predicate(|_line| true)
                .matched(false)
                .build();
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
            let visitor = WaitForLine::builder()
                .parser(LineParserState::new())
                .options(LineParsingOptions::default())
                .predicate(|line| line == "ready")
                .matched(false)
                .build();
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
