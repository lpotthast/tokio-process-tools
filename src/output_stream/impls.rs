use crate::output_stream::{LineParserState, LineParsingOptions, Next};
use std::borrow::Cow;

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

pub(crate) fn collect_owned_lines(
    chunk: &[u8],
    parser: &mut LineParserState,
    options: LineParsingOptions,
) -> Vec<String> {
    let mut lines = Vec::new();
    let _ = visit_lines(chunk, parser, options, |line| {
        lines.push(line.into_owned());
        Next::Continue
    });
    lines
}

pub(crate) fn collect_owned_final_line(parser: &LineParserState) -> Option<String> {
    let mut final_line = None;
    let _ = visit_final_line(parser, |line| {
        final_line = Some(line.into_owned());
        Next::Continue
    });
    final_line
}

macro_rules! impl_inspect_chunks {
    ($stream_name:expr, $receiver:expr, $f:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            if let Next::Break = $f(chunk) {
                                break 'outer;
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_chunks;

macro_rules! impl_inspect_lines {
    ($stream_name:expr, $receiver:expr, $f:ident, $options:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            if visit_lines(chunk.as_ref(), &mut parser, $options, &mut $f)
                                == Next::Break
                            {
                                break 'outer;
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            if visit_final_line(&parser, $f) == Next::Break {
                                break 'outer;
                            }
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_lines;

macro_rules! impl_inspect_lines_async {
    ($stream_name:expr, $receiver:expr, $f:ident, $options:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Inspector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            for line in crate::output_stream::impls::collect_owned_lines(
                                chunk.as_ref(),
                                &mut parser,
                                $options,
                            ) {
                                match $f(Cow::Owned(line)).await {
                                    Next::Continue => {}
                                    Next::Break => break 'outer,
                                }
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            if let Some(line) =
                                crate::output_stream::impls::collect_owned_final_line(&parser)
                            {
                                if $f(Cow::Owned(line)).await == Next::Break {
                                    break 'outer;
                                }
                            }
                            break 'outer;
                        }
                    }
                });
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_inspect_lines_async;

macro_rules! impl_collect_chunks {
    ($stream_name:expr, $receiver:expr, $collect:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let mut write_guard = $sink.write().await;
                            $collect(chunk, &mut (*write_guard));
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks;

macro_rules! impl_collect_lines {
    ($stream_name:expr, $receiver:expr, $collect:ident, $options:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let mut write_guard = $sink.write().await;
                            let sink = &mut *write_guard;
                            if visit_lines(chunk.as_ref(), &mut parser, $options, |line| {
                                $collect(line, sink)
                            }) == Next::Break
                            {
                                break 'outer;
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            if let Some(line) =
                                crate::output_stream::impls::collect_owned_final_line(&parser)
                            {
                                let mut write_guard = $sink.write().await;
                                let sink = &mut *write_guard;
                                if $collect(Cow::Owned(line), sink) == Next::Break {
                                    break 'outer;
                                }
                            }
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines;

macro_rules! impl_collect_chunks_async {
    ($stream_name:expr, $receiver:expr, $collect:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let result = {
                                let mut write_guard = $sink.write().await;
                                let fut = $collect(chunk, &mut *write_guard);
                                fut.await
                            };
                            match result {
                                Next::Continue => {},
                                Next::Break => break 'outer,
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks_async;

macro_rules! impl_collect_lines_async {
    ($stream_name:expr, $receiver:expr, $collect:ident, $options:ident, $sink:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let mut write_guard = $sink.write().await;
                            let sink = &mut *write_guard;
                            for line in crate::output_stream::impls::collect_owned_lines(
                                chunk.as_ref(),
                                &mut parser,
                                $options,
                            ) {
                                match $collect(Cow::Owned(line), sink).await {
                                    Next::Continue => {}
                                    Next::Break => break 'outer,
                                }
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            if let Some(line) =
                                crate::output_stream::impls::collect_owned_final_line(&parser)
                            {
                                let mut write_guard = $sink.write().await;
                                let sink = &mut *write_guard;
                                match $collect(Cow::Owned(line), sink).await {
                                    Next::Continue | Next::Break => {}
                                }
                            }
                            break 'outer;
                        }
                    }
                });
                Arc::try_unwrap($sink).expect("single owner").into_inner()
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines_async;
