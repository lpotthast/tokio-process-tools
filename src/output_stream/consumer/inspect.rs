use super::{collect_owned_final_line, visit_final_line, visit_lines};
use crate::inspector::Inspector;
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{Chunk, LineParserState, LineParsingOptions, Next, StreamEvent};
use std::borrow::Cow;
use std::future::Future;

pub(crate) fn inspect_chunks<S, F>(
    stream_name: &'static str,
    mut subscription: S,
    mut f: F,
) -> Inspector
where
    S: EventSubscription,
    F: FnMut(Chunk) -> Next + Send + 'static,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Inspector {
        stream_name,
        task: Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                if f(chunk) == Next::Break {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(())
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn inspect_chunks_async<S, F, Fut>(
    stream_name: &'static str,
    mut subscription: S,
    mut f: F,
) -> Inspector
where
    S: EventSubscription,
    F: FnMut(Chunk) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Inspector {
        stream_name,
        task: Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                if f(chunk).await == Next::Break {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(())
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn inspect_lines<S, F>(
    stream_name: &'static str,
    mut subscription: S,
    mut f: F,
    options: LineParsingOptions,
) -> Inspector
where
    S: EventSubscription,
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Inspector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                if visit_lines(chunk.as_ref(), &mut parser, options, &mut f)
                                    == Next::Break
                                {
                                    break;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                let _next = visit_final_line(&parser, f);
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(())
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn inspect_lines_async<S, F, Fut>(
    stream_name: &'static str,
    mut subscription: S,
    mut f: F,
    options: LineParsingOptions,
) -> Inspector
where
    S: EventSubscription,
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Inspector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                let mut should_break = false;
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    if f(Cow::Owned(line)).await == Next::Break {
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
                                    let _next = f(Cow::Owned(line)).await;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(())
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}
