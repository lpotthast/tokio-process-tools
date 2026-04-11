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

pub(crate) fn collect_owned_final_line(parser: &LineParserState) -> Option<String> {
    parser.finish_owned()
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
                            for line in parser.owned_lines(chunk.as_ref(), $options) {
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
                let mut sink = $sink;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            $collect(chunk, &mut sink);
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            // EOF reached.
                            break 'outer;
                        }
                    }
                });
                sink
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
                let mut sink = $sink;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            if visit_lines(chunk.as_ref(), &mut parser, $options, |line| {
                                $collect(line, &mut sink)
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
                                if $collect(Cow::Owned(line), &mut sink) == Next::Break {
                                    break 'outer;
                                }
                            }
                            break 'outer;
                        }
                    }
                });
                sink
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
                let mut collect = $collect;
                let mut sink = $sink;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let result = crate::collector::AsyncChunkCollector::collect(
                                &mut collect,
                                chunk,
                                &mut sink,
                            )
                            .await;
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
                sink
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
                let mut collect = $collect;
                let mut sink = $sink;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            for line in parser.owned_lines(chunk.as_ref(), $options) {
                                match crate::collector::AsyncLineCollector::collect(
                                    &mut collect,
                                    Cow::Owned(line),
                                    &mut sink,
                                )
                                .await
                                {
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
                                match crate::collector::AsyncLineCollector::collect(
                                    &mut collect,
                                    Cow::Owned(line),
                                    &mut sink,
                                )
                                .await
                                {
                                    Next::Continue | Next::Break => {}
                                }
                            }
                            break 'outer;
                        }
                    }
                });
                sink
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines_async;

macro_rules! impl_collect_chunks_into_write {
    ($stream_name:expr, $receiver:expr, $write:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut write = $write;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            if let Err(err) = write.write_all(chunk.as_ref()).await {
                                tracing::warn!("Could not write chunk to write sink: {err:#?}");
                            };
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            break 'outer;
                        }
                    }
                });
                write
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks_into_write;

macro_rules! impl_collect_chunks_into_write_mapped {
    ($stream_name:expr, $receiver:expr, $write:ident, $mapper:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut write = $write;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            let mapped = $mapper(chunk);
                            let mapped = mapped.as_ref();
                            if let Err(err) = write.write_all(mapped).await {
                                tracing::warn!("Could not write chunk to write sink: {err:#?}");
                            };
                        }
                        crate::output_stream::StreamEvent::Gap => {}
                        crate::output_stream::StreamEvent::Eof => {
                            break 'outer;
                        }
                    }
                });
                write
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_chunks_into_write_mapped;

macro_rules! impl_collect_lines_into_write {
    ($stream_name:expr, $receiver:expr, $write:ident, $options:ident, $mode:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                let mut write = $write;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            for line in parser.owned_lines(chunk.as_ref(), $options) {
                                if let Err(err) = write.write_all(line.as_bytes()).await {
                                    tracing::warn!("Could not write line to write sink: {err:#?}");
                                } else if matches!($mode, crate::output_stream::LineWriteMode::AppendLf)
                                    && let Err(err) = write.write_all(b"\n").await
                                {
                                    tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                                };
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            if let Some(line) =
                                crate::output_stream::impls::collect_owned_final_line(&parser)
                            {
                                if let Err(err) = write.write_all(line.as_bytes()).await {
                                    tracing::warn!("Could not write line to write sink: {err:#?}");
                                } else if matches!($mode, crate::output_stream::LineWriteMode::AppendLf)
                                    && let Err(err) = write.write_all(b"\n").await
                                {
                                    tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                                };
                            }
                            break 'outer;
                        }
                    }
                });
                write
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines_into_write;

macro_rules! impl_collect_lines_into_write_mapped {
    ($stream_name:expr, $receiver:expr, $write:ident, $mapper:ident, $options:ident, $mode:ident, $handler:ident) => {{
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        Collector {
            stream_name: $stream_name,
            task: Some(tokio::spawn(async move {
                let mut parser = crate::output_stream::LineParserState::new();
                let mut write = $write;
                $handler!('outer, $receiver, term_sig_rx, |maybe_chunk| {
                    match maybe_chunk {
                        crate::output_stream::StreamEvent::Chunk(chunk) => {
                            for line in parser.owned_lines(chunk.as_ref(), $options) {
                                let mapped = $mapper(Cow::Owned(line));
                                let mapped = mapped.as_ref();
                                if let Err(err) = write.write_all(mapped).await {
                                    tracing::warn!("Could not write line to write sink: {err:#?}");
                                } else if matches!($mode, crate::output_stream::LineWriteMode::AppendLf)
                                    && let Err(err) = write.write_all(b"\n").await
                                {
                                    tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                                };
                            }
                        }
                        crate::output_stream::StreamEvent::Gap => {
                            parser.on_gap();
                        }
                        crate::output_stream::StreamEvent::Eof => {
                            if let Some(line) =
                                crate::output_stream::impls::collect_owned_final_line(&parser)
                            {
                                let mapped = $mapper(Cow::Owned(line));
                                let mapped = mapped.as_ref();
                                if let Err(err) = write.write_all(mapped).await {
                                    tracing::warn!("Could not write line to write sink: {err:#?}");
                                } else if matches!($mode, crate::output_stream::LineWriteMode::AppendLf)
                                    && let Err(err) = write.write_all(b"\n").await
                                {
                                    tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                                };
                            }
                            break 'outer;
                        }
                    }
                });
                write
            })),
            task_termination_sender: Some(term_sig_tx),
        }
    }};
}
pub(crate) use impl_collect_lines_into_write_mapped;
