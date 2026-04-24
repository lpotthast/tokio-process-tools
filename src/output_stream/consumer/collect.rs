use super::{collect_owned_final_line, visit_lines};
use crate::collector::{AsyncChunkCollector, AsyncLineCollector, Collector, Sink};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{Chunk, LineParserState, LineParsingOptions, Next, StreamEvent};
use std::borrow::Cow;

pub(crate) fn collect_chunks<S, T, F>(
    stream_name: &'static str,
    mut subscription: S,
    into: T,
    mut collect: F,
) -> Collector<T>
where
    S: EventSubscription,
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
    S: EventSubscription,
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
    S: EventSubscription,
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
    S: EventSubscription,
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
