use super::collect_owned_final_line;
use crate::collector::{Collector, CollectorTaskError, Sink};
use crate::output_stream::collection::{
    SinkWriteError, SinkWriteErrorAction, SinkWriteErrorHandler, SinkWriteOperation,
    WriteCollectionOptions,
};
use crate::output_stream::subscription::EventSubscription;
use crate::output_stream::{
    Chunk, LineParserState, LineParsingOptions, LineWriteMode, StreamEvent,
};
use std::borrow::Cow;
use std::io;
use tokio::io::AsyncWriteExt;

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
