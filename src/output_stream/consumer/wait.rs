use super::{visit_final_line, visit_lines};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{LineParserState, LineParsingOptions, Next, StreamEvent};
use crate::{StreamReadError, WaitForLineResult};
use std::borrow::Cow;
use std::time::Duration;

pub(crate) async fn wait_for_line<S>(
    mut subscription: S,
    predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
    options: LineParsingOptions,
) -> Result<WaitForLineResult, StreamReadError>
where
    S: EventSubscription,
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
    S: EventSubscription,
{
    match timeout {
        None => wait_for_line(subscription, predicate, options).await,
        Some(timeout) => {
            match tokio::time::timeout(timeout, wait_for_line(subscription, predicate, options))
                .await
            {
                Ok(result) => result,
                Err(_elapsed) => Ok(WaitForLineResult::Timeout),
            }
        }
    }
}
