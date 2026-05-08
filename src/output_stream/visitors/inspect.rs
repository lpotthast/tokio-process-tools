use crate::output_stream::Next;
use crate::output_stream::event::Chunk;
use crate::output_stream::line::adapter::{AsyncLineVisitor, LineVisitor};
use crate::output_stream::visitor::{AsyncStreamVisitor, StreamVisitor};
use std::borrow::Cow;
use std::future::Future;
use std::marker::PhantomData;
use typed_builder::TypedBuilder;

/// Synchronous [`StreamVisitor`] that calls `f` once per observed chunk and discards the
/// chunk afterwards. Construct via [`builder`](Self::builder) and pass to
/// [`Consumable::consume`](crate::Consumable::consume).
#[derive(TypedBuilder)]
pub struct InspectChunks<F>
where
    F: FnMut(Chunk) -> Next + Send + 'static,
{
    /// Per-chunk callback. Return [`Next::Break`] to stop the consumer early.
    pub f: F,
}

impl<F> StreamVisitor for InspectChunks<F>
where
    F: FnMut(Chunk) -> Next + Send + 'static,
{
    type Output = ();

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        (self.f)(chunk)
    }

    fn into_output(self) -> Self::Output {}
}

/// Asynchronous counterpart to [`InspectChunks`]. Construct via [`builder`](Self::builder) and
/// pass to [`Consumable::consume_async`](crate::Consumable::consume_async).
#[derive(TypedBuilder)]
pub struct InspectChunksAsync<F, Fut>
where
    F: FnMut(Chunk) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    /// Per-chunk async callback. The returned future yields [`Next::Break`] to stop early.
    pub f: F,
    /// Phantom marker so the `Fut` bound lives on the struct (and on the derived builder)
    /// rather than only on the impl block. The closure's return type carries `Fut`, so the
    /// builder infers it from `f`. Users never spell `Fut` out.
    #[builder(default, setter(skip))]
    pub _fut: PhantomData<fn() -> Fut>,
}

impl<F, Fut> AsyncStreamVisitor for InspectChunksAsync<F, Fut>
where
    F: FnMut(Chunk) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    type Output = ();

    fn on_chunk(&mut self, chunk: Chunk) -> impl Future<Output = Next> + Send + '_ {
        (self.f)(chunk)
    }

    fn into_output(self) -> Self::Output {}
}

/// [`LineVisitor`] wrapping a per-line closure. Compose with
/// [`ParseLines`](crate::output_stream::line::adapter::ParseLines) (most easily via
/// [`ParseLines::inspect`](crate::output_stream::line::adapter::ParseLines::inspect)) to
/// build a line-by-line inspector consumer.
pub struct InspectLines<F> {
    f: F,
}

impl<F> InspectLines<F>
where
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    /// Creates a new sink that calls `f` once for each parsed line.
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F> LineVisitor for InspectLines<F>
where
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    type Output = ();

    fn on_line(&mut self, line: Cow<'_, str>) -> Next {
        (self.f)(line)
    }

    fn into_output(self) -> Self::Output {}
}

/// [`AsyncLineVisitor`] wrapping a per-line async closure. Compose with
/// [`ParseLines`](crate::output_stream::line::adapter::ParseLines) (most easily via
/// [`ParseLines::inspect_async`](crate::output_stream::line::adapter::ParseLines::inspect_async))
/// to build an async line-by-line inspector consumer. The `PhantomData<fn() -> Fut>` carries
/// the future's type onto the struct so callers never name `Fut` explicitly.
pub struct InspectLinesAsync<F, Fut> {
    f: F,
    _fut: PhantomData<fn() -> Fut>,
}

impl<F, Fut> InspectLinesAsync<F, Fut>
where
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    /// Creates a new sink that awaits `f` once for each parsed line.
    pub fn new(f: F) -> Self {
        Self {
            f,
            _fut: PhantomData,
        }
    }
}

impl<F, Fut> AsyncLineVisitor for InspectLinesAsync<F, Fut>
where
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    type Output = ();

    fn on_line<'a>(&'a mut self, line: Cow<'a, str>) -> impl Future<Output = Next> + Send + 'a {
        (self.f)(line)
    }

    fn into_output(self) -> Self::Output {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::consumer::Consumer;
    use crate::output_stream::consumer::driver::spawn_consumer_sync;
    use crate::output_stream::event::StreamEvent;
    use crate::output_stream::event::tests::event_receiver;
    use crate::output_stream::line::adapter::ParseLines;
    use crate::output_stream::line::options::LineParsingOptions;
    use crate::{ConsumerCancelOutcome, ConsumerError, StreamReadError};
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn cancel_returns_cancelled_when_cooperative() {
        let (task_termination_sender, task_termination_receiver) = oneshot::channel();
        let inspector: Consumer<()> = Consumer {
            stream_name: "custom",
            task: Some(tokio::spawn(async move {
                let _res = task_termination_receiver.await;
                Ok(())
            })),
            task_termination_sender: Some(task_termination_sender),
        };

        let outcome = inspector.cancel(Duration::from_secs(1)).await.unwrap();

        assert_that!(matches!(outcome, ConsumerCancelOutcome::Cancelled(()))).is_true();
    }

    mod inspect_lines {
        use super::*;
        use crate::NumBytesExt;

        #[test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        fn panics_when_max_line_length_is_zero() {
            let _visitor = ParseLines::new(
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: crate::LineOverflowBehavior::default(),
                    buffer_compaction_threshold: None,
                },
                InspectLines::new(|_line| Next::Continue),
            );
        }

        #[tokio::test]
        async fn inspectors_return_stream_read_error() {
            let error = StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
            let inspector = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"complete\npartial"))),
                    StreamEvent::ReadError(error),
                ])
                .await,
                ParseLines::inspect(LineParsingOptions::default(), |_line| Next::Continue),
            );

            match inspector.wait().await {
                Err(ConsumerError::StreamRead { source }) => {
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
            let inspector = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\npar"))),
                    StreamEvent::Gap,
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"\ntwo\nfinal"))),
                    StreamEvent::Eof,
                ])
                .await,
                ParseLines::inspect(LineParsingOptions::default(), move |line| {
                    seen_in_task.lock().unwrap().push(line.into_owned());
                    Next::Continue
                }),
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).contains_exactly(["one", "two", "final"]);
        }
    }

    mod inspect_chunks {
        use super::*;

        #[tokio::test]
        async fn accepts_stateful_callback() {
            let (count_tx, count_rx) = oneshot::channel();
            let mut chunk_count = 0;
            let mut count_tx = Some(count_tx);
            let inspector = spawn_consumer_sync(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"gh"))),
                    StreamEvent::Eof,
                ])
                .await,
                InspectChunks::builder()
                    .f(move |_chunk| {
                        chunk_count += 1;
                        if chunk_count == 3 {
                            count_tx.take().unwrap().send(chunk_count).unwrap();
                            Next::Break
                        } else {
                            Next::Continue
                        }
                    })
                    .build(),
            );

            inspector.wait().await.unwrap();
            let chunk_count = count_rx.await.unwrap();
            assert_that!(chunk_count).is_equal_to(3);
        }
    }

    mod inspect_chunks_async {
        use super::*;
        use crate::output_stream::consumer::driver::spawn_consumer_async;

        #[tokio::test]
        async fn accepts_stateful_callback() {
            let seen = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
            let seen_in_task = Arc::clone(&seen);
            let mut chunk_count = 0;
            let inspector = spawn_consumer_async(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ab"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"cd"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"ef"))),
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"gh"))),
                    StreamEvent::Eof,
                ])
                .await,
                InspectChunksAsync::builder()
                    .f(move |chunk| {
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
                    })
                    .build(),
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).is_equal_to(vec![b"ab".to_vec(), b"cd".to_vec(), b"ef".to_vec()]);
        }
    }

    mod inspect_lines_async {
        use super::*;
        use crate::NumBytesExt;
        use crate::output_stream::consumer::driver::spawn_consumer_async;

        #[test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        fn panics_when_max_line_length_is_zero() {
            let _visitor = ParseLines::new(
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: crate::LineOverflowBehavior::default(),
                    buffer_compaction_threshold: None,
                },
                InspectLinesAsync::new(|_line| async { Next::Continue }),
            );
        }

        #[tokio::test]
        async fn preserves_unterminated_final_line() {
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_in_task = Arc::clone(&seen);
            let inspector = spawn_consumer_async(
                "custom",
                event_receiver(vec![
                    StreamEvent::Chunk(Chunk(Bytes::from_static(b"tail"))),
                    StreamEvent::Eof,
                ])
                .await,
                ParseLines::inspect_async(LineParsingOptions::default(), move |line| {
                    let seen = Arc::clone(&seen_in_task);
                    let line = line.into_owned();
                    async move {
                        seen.lock().unwrap().push(line);
                        Next::Continue
                    }
                }),
            );

            inspector.wait().await.unwrap();

            let seen = seen.lock().unwrap().clone();
            assert_that!(seen).contains_exactly(["tail"]);
        }
    }
}
