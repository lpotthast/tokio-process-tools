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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CollectorError;
    use crate::{AsyncChunkCollector, AsyncLineCollector};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::borrow::Cow;
    use std::io;
    use tokio::sync::mpsc;

    async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
        let (tx, rx) = mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.unwrap();
        }
        drop(tx);
        rx
    }

    #[tokio::test]
    async fn collectors_return_stream_read_error() {
        let error =
            crate::StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"complete\npartial"))),
                StreamEvent::ReadError(error),
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        match collector.wait().await {
            Err(CollectorError::StreamRead { source, .. }) => {
                assert_that!(source.stream_name()).is_equal_to("custom");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected collector stream read error, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn collectors_skip_gaps_and_keep_final_unterminated_line() {
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\npar"))),
                StreamEvent::Gap,
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"\ntwo\nfinal"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::<String>::new(),
            |line, lines| {
                lines.push(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let lines = collector.wait().await.unwrap();
        assert_that!(lines).contains_exactly(["one", "two", "final"]);
    }

    struct ExtendChunks;

    impl AsyncChunkCollector<Vec<u8>> for ExtendChunks {
        async fn collect<'a>(&'a mut self, chunk: Chunk, seen: &'a mut Vec<u8>) -> Next {
            seen.extend_from_slice(chunk.as_ref());
            Next::Continue
        }
    }

    #[tokio::test]
    async fn chunk_collector_async_extends_sink_until_eof() {
        let collector = collect_chunks_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            ExtendChunks,
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).is_equal_to(b"abcdef".to_vec());
    }

    #[tokio::test]
    async fn chunk_collector_accepts_stateful_callback() {
        let mut chunk_index = 0;
        let collector = collect_chunks(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |chunk, indexed_chunks| {
                chunk_index += 1;
                indexed_chunks.push((chunk_index, chunk.as_ref().to_vec()));
            },
        );

        let indexed_chunks = collector.wait().await.unwrap();
        assert_that!(indexed_chunks).is_equal_to(vec![
            (1, b"ab".to_vec()),
            (2, b"cd".to_vec()),
            (3, b"ef".to_vec()),
        ]);
    }

    #[tokio::test]
    async fn line_collector_accepts_stateful_callback() {
        let mut line_index = 0;
        let collector = collect_lines(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"alpha\nbeta\ngamma\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            move |line, indexed_lines| {
                line_index += 1;
                indexed_lines.push(format!("{line_index}:{line}"));
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        let indexed_lines = collector.wait().await.unwrap();
        assert_that!(indexed_lines).is_equal_to(vec![
            "1:alpha".to_string(),
            "2:beta".to_string(),
            "3:gamma".to_string(),
        ]);
    }

    struct BreakOnLine;

    impl AsyncLineCollector<Vec<String>> for BreakOnLine {
        async fn collect<'a>(&'a mut self, line: Cow<'a, str>, seen: &'a mut Vec<String>) -> Next {
            if line == "break" {
                seen.push(line.into_owned());
                Next::Break
            } else {
                seen.push(line.into_owned());
                Next::Continue
            }
        }
    }

    #[tokio::test]
    async fn line_collector_async_break_stops_after_requested_line() {
        let collector = collect_lines_async(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"start\nbreak\nend\n"))),
                StreamEvent::Eof,
            ])
            .await,
            Vec::new(),
            BreakOnLine,
            LineParsingOptions::default(),
        );

        let seen = collector.wait().await.unwrap();
        assert_that!(seen).contains_exactly(["start", "break"]);
    }
}
