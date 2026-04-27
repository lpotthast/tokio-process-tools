use super::{visit_final_line, visit_lines};
use crate::output_stream::event::StreamEvent;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::{Next, Subscription};
use crate::{StreamReadError, WaitForLineResult};
use std::borrow::Cow;
use std::time::Duration;

pub(crate) async fn wait_for_line<S>(
    mut subscription: S,
    predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
    options: LineParsingOptions,
) -> Result<WaitForLineResult, StreamReadError>
where
    S: Subscription,
{
    let mut parser = LineParserState::new();

    loop {
        match subscription.next_event().await {
            Some(StreamEvent::Chunk(chunk)) => {
                if visit_lines(chunk.as_ref(), &mut parser, options, |line| {
                    if predicate(line) {
                        Next::Break
                    } else {
                        Next::Continue
                    }
                }) == Next::Break
                {
                    return Ok(WaitForLineResult::Matched);
                }
            }
            Some(StreamEvent::Gap) => parser.on_gap(),
            Some(StreamEvent::Eof) | None => {
                if visit_final_line(&parser, |line| {
                    if predicate(line) {
                        Next::Break
                    } else {
                        Next::Continue
                    }
                }) == Next::Break
                {
                    return Ok(WaitForLineResult::Matched);
                }
                return Ok(WaitForLineResult::StreamClosed);
            }
            Some(StreamEvent::ReadError(err)) => return Err(err),
        }
    }
}

pub(crate) async fn wait_for_line_with_optional_timeout<S>(
    subscription: S,
    predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
    options: LineParsingOptions,
    timeout: Option<Duration>,
) -> Result<WaitForLineResult, StreamReadError>
where
    S: Subscription,
{
    match timeout {
        None => wait_for_line(subscription, predicate, options).await,
        Some(timeout) => {
            tokio::time::timeout(timeout, wait_for_line(subscription, predicate, options))
                .await
                .unwrap_or(Ok(WaitForLineResult::Timeout))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::event::Chunk;
    use crate::{LineOverflowBehavior, NumBytesExt};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use tokio::sync::mpsc;

    async fn wait_for_ready(
        events: Vec<StreamEvent>,
    ) -> Result<WaitForLineResult, StreamReadError> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);

        wait_for_line(rx, |line| line == "ready", LineParsingOptions::default()).await
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

        #[tokio::test]
        async fn honors_line_parsing_options() {
            let (tx, rx) = mpsc::channel(2);
            tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(
                b"readiness\n",
            ))))
            .await
            .unwrap();
            tx.send(StreamEvent::Eof).await.unwrap();
            drop(tx);

            let result = wait_for_line(
                rx,
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

    mod wait_for_line_with_optional_timeout {
        use super::*;

        #[tokio::test]
        async fn times_out_with_timeout_error() {
            let (_tx, rx) = mpsc::channel(1);
            let timeout = wait_for_line_with_optional_timeout(
                rx,
                |line| line == "ready",
                LineParsingOptions::default(),
                Some(Duration::from_millis(25)),
            )
            .await;
            assert_that!(timeout).is_equal_to(Ok(WaitForLineResult::Timeout));
        }
    }
}
