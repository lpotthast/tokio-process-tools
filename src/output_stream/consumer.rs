use crate::collector::{
    AsyncChunkCollector, AsyncLineCollector, Collector, CollectorTaskError, Sink,
};
use crate::inspector::Inspector;
use crate::output_stream::collection::{
    SinkWriteError, SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation,
    WriteCollectionOptions,
};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    Chunk, LineParserState, LineParsingOptions, LineWriteMode, Next, StreamEvent,
};
use crate::{StreamReadError, WaitForLineResult};
use std::borrow::Cow;
use std::future::Future;
use std::io;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub(crate) fn visit_lines(
    chunk: &[u8],
    parser: &mut LineParserState,
    options: LineParsingOptions,
    f: impl FnMut(Cow<'_, str>) -> Next,
) -> Next {
    parser.visit_chunk(chunk, options, f)
}

pub(crate) fn visit_final_line(
    parser: &LineParserState,
    f: impl FnOnce(Cow<'_, str>) -> Next,
) -> Next {
    parser.finish(f)
}

pub(crate) fn collect_owned_final_line(parser: &LineParserState) -> Option<String> {
    parser.finish_owned()
}

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

pub(crate) fn collect_chunks_into_write<S, W, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: EventSubscription,
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                handle_write_result(
                                    stream_name,
                                    &mut error_handler,
                                    SinkWriteOperation::Chunk,
                                    chunk.as_ref().len(),
                                    write.write_all(chunk.as_ref()).await,
                                )?;
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_chunks_into_write_mapped<S, W, B, F, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    mapper: F,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: EventSubscription,
    W: Sink + AsyncWriteExt + Unpin,
    B: AsRef<[u8]> + Send,
    F: Fn(Chunk) -> B + Send + Sync + Copy + 'static,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                let mapped_output = mapper(chunk);
                                handle_write_result(
                                    stream_name,
                                    &mut error_handler,
                                    SinkWriteOperation::Chunk,
                                    mapped_output.as_ref().len(),
                                    write.write_all(mapped_output.as_ref()).await,
                                )?;
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_into_write<S, W, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    options: LineParsingOptions,
    mode: LineWriteMode,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: EventSubscription,
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        line.as_bytes(),
                                        mode,
                                    )
                                    .await?;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        line.as_bytes(),
                                        mode,
                                    )
                                    .await?;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_into_write_mapped<S, W, B, F, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    mapper: F,
    options: LineParsingOptions,
    mode: LineWriteMode,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: EventSubscription,
    W: Sink + AsyncWriteExt + Unpin,
    B: AsRef<[u8]> + Send,
    F: Fn(Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    let mapped_output = mapper(Cow::Owned(line));
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        mapped_output.as_ref(),
                                        mode,
                                    )
                                    .await?;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    let mapped_output = mapper(Cow::Owned(line));
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        mapped_output.as_ref(),
                                        mode,
                                    )
                                    .await?;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

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

async fn write_line<W, H>(
    stream_name: &'static str,
    write: &mut W,
    error_handler: &mut H,
    line: &[u8],
    mode: LineWriteMode,
) -> Result<(), CollectorTaskError>
where
    W: AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let line_write = write.write_all(line).await;
    let line_written = handle_write_result(
        stream_name,
        error_handler,
        SinkWriteOperation::Line,
        line.len(),
        line_write,
    )?;
    if !line_written || !matches!(mode, LineWriteMode::AppendLf) {
        return Ok(());
    }

    handle_write_result(
        stream_name,
        error_handler,
        SinkWriteOperation::LineDelimiter,
        1,
        write.write_all(b"\n").await,
    )?;

    Ok(())
}

fn handle_write_result<H>(
    stream_name: &'static str,
    error_handler: &mut H,
    operation: SinkWriteOperation,
    attempted_len: usize,
    result: io::Result<()>,
) -> Result<bool, CollectorTaskError>
where
    H: SinkWriteErrorHandler,
{
    match result {
        Ok(()) => Ok(true),
        Err(source) => {
            let error = SinkWriteError::new(stream_name, operation, attempted_len, source);
            match error_handler.handle(&error) {
                SinkWriteErrorAction::Stop => {
                    Err(CollectorTaskError::SinkWrite(error.into_source()))
                }
                SinkWriteErrorAction::Continue => Ok(false),
            }
        }
    }
}
