use super::{collect_owned_final_line, visit_final_line, visit_lines};
use crate::inspector::Inspector;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::{Next, Subscription};
use std::borrow::Cow;
use std::future::Future;

pub(crate) fn inspect_chunks<S, F>(
    stream_name: &'static str,
    mut subscription: S,
    mut f: F,
) -> Inspector
where
    S: Subscription,
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
    S: Subscription,
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
    S: Subscription,
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
    S: Subscription,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InspectorError;
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    mod inspect_lines {
        use super::*;

        async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for event in events {
                tx.send(event).await.unwrap();
            }
            drop(tx);
            rx
        }

        #[tokio::test]
        async fn inspectors_return_stream_read_error() {
            let error =
                crate::StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
            let inspector = inspect_lines(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"complete\npartial"))),
                    StreamEvent::ReadError(error),
                ])
                .await,
                |_line| Next::Continue,
                LineParsingOptions::default(),
            );

            match inspector.wait().await {
                Err(InspectorError::StreamRead { source, .. }) => {
                    assert_that!(source.stream_name()).is_equal_to("custom");
                    assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
                }
                other => {
                    assert_that!(&other).fail(format_args!(
                        "expected inspector stream read error, got {other:?}"
                    ));
                }
            }
        }

        #[tokio::test]
        async fn inspectors_skip_gaps_and_visit_final_unterminated_line() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_in_task = Arc::clone(&seen);
            let inspector = inspect_lines(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\npar"))),
                    StreamEvent::Gap,
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"\ntwo\nfinal"))),
                    StreamEvent::Eof,
                ])
                .await,
                move |line| {
                    seen_in_task.lock().unwrap().push(line.into_owned());
                    Next::Continue
                },
                LineParsingOptions::default(),
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).contains_exactly(["one", "two", "final"]);
        }
    }

    mod inspect_chunks {
        use super::*;

        async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for event in events {
                tx.send(event).await.unwrap();
            }
            drop(tx);
            rx
        }

        #[tokio::test]
        async fn accepts_stateful_callback() {
            let (count_tx, count_rx) = tokio::sync::oneshot::channel();
            let mut chunk_count = 0;
            let mut count_tx = Some(count_tx);
            let inspector = inspect_chunks(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"gh"))),
                    StreamEvent::Eof,
                ])
                .await,
                move |_chunk| {
                    chunk_count += 1;
                    if chunk_count == 3 {
                        count_tx.take().unwrap().send(chunk_count).unwrap();
                        Next::Break
                    } else {
                        Next::Continue
                    }
                },
            );

            inspector.wait().await.unwrap();
            let chunk_count = count_rx.await.unwrap();
            assert_that!(chunk_count).is_equal_to(3);
        }
    }

    mod inspect_chunks_async {
        use super::*;

        async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for event in events {
                tx.send(event).await.unwrap();
            }
            drop(tx);
            rx
        }

        #[tokio::test]
        async fn accepts_stateful_callback() {
            let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let seen_in_task = Arc::clone(&seen);
            let mut chunk_count = 0;
            let inspector = inspect_chunks_async(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"gh"))),
                    StreamEvent::Eof,
                ])
                .await,
                move |chunk| {
                    chunk_count += 1;
                    let seen = Arc::clone(&seen_in_task);
                    let bytes = chunk.as_ref().to_vec();
                    let should_break = chunk_count == 3;
                    async move {
                        seen.lock().unwrap().push(bytes);
                        if should_break {
                            Next::Break
                        } else {
                            Next::Continue
                        }
                    }
                },
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).is_equal_to(vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()]);
        }
    }

    mod inspect_lines_async {
        use super::*;

        async fn event_receiver(events: Vec<StreamEvent>) -> mpsc::Receiver<StreamEvent> {
            let (tx, rx) = mpsc::channel(events.len().max(1));
            for event in events {
                tx.send(event).await.unwrap();
            }
            drop(tx);
            rx
        }

        #[tokio::test]
        async fn preserves_unterminated_final_line() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_in_task = Arc::clone(&seen);
            let inspector = inspect_lines_async(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"tail"))),
                    StreamEvent::Eof,
                ])
                .await,
                move |line| {
                    let seen = Arc::clone(&seen_in_task);
                    let line = line.into_owned();
                    async move {
                        seen.lock().unwrap().push(line);
                        Next::Continue
                    }
                },
                LineParsingOptions::default(),
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).contains_exactly(["tail"]);
        }
    }
}
