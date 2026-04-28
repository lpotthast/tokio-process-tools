use super::super::consumer::{Consumer, ConsumerCancelOutcome, ConsumerError, ConsumerTaskError};
use super::super::visitor::{AsyncStreamVisitor, StreamVisitor, consume_async, consume_sync};
use crate::StreamReadError;
use crate::output_stream::event::Chunk;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::{Next, Subscription};
use std::borrow::Cow;
use std::future::Future;
use std::time::Duration;
use thiserror::Error;

pub(crate) struct InspectChunks<F> {
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

pub(crate) struct InspectChunksAsync<F> {
    pub f: F,
}

impl<F, Fut> AsyncStreamVisitor for InspectChunksAsync<F>
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

pub(crate) struct InspectLines<F> {
    pub parser: LineParserState,
    pub options: LineParsingOptions,
    pub f: F,
}

impl<F> StreamVisitor for InspectLines<F>
where
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    type Output = ();

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        self.parser
            .visit_chunk(chunk.as_ref(), self.options, &mut self.f)
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
    }

    fn on_eof(&mut self) {
        let _ = self.parser.finish(&mut self.f);
    }

    fn into_output(self) -> Self::Output {}
}

pub(crate) struct InspectLinesAsync<F> {
    pub parser: LineParserState,
    pub options: LineParsingOptions,
    pub f: F,
}

impl<F, Fut> AsyncStreamVisitor for InspectLinesAsync<F>
where
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    type Output = ();

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        for line in self.parser.owned_lines(chunk.as_ref(), self.options) {
            if (self.f)(Cow::Owned(line)).await == Next::Break {
                return Next::Break;
            }
        }
        Next::Continue
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
    }

    async fn on_eof(&mut self) {
        if let Some(line) = self.parser.finish_owned() {
            let _ = (self.f)(Cow::Owned(line)).await;
        }
    }

    fn into_output(self) -> Self::Output {}
}

pub(crate) fn inspect_chunks<S, F>(stream_name: &'static str, subscription: S, f: F) -> Inspector
where
    S: Subscription,
    F: FnMut(Chunk) -> Next + Send + 'static,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = consume_sync(subscription, InspectChunks { f }, term_sig_rx);
    Inspector(Consumer {
        stream_name,
        task: Some(tokio::spawn(async move {
            driver.await.map_err(ConsumerTaskError::from)
        })),
        task_termination_sender: Some(term_sig_tx),
    })
}

pub(crate) fn inspect_chunks_async<S, F, Fut>(
    stream_name: &'static str,
    subscription: S,
    f: F,
) -> Inspector
where
    S: Subscription,
    F: FnMut(Chunk) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = consume_async(subscription, InspectChunksAsync { f }, term_sig_rx);
    Inspector(Consumer {
        stream_name,
        task: Some(tokio::spawn(async move {
            driver.await.map_err(ConsumerTaskError::from)
        })),
        task_termination_sender: Some(term_sig_tx),
    })
}

pub(crate) fn inspect_lines<S, F>(
    stream_name: &'static str,
    subscription: S,
    f: F,
    options: LineParsingOptions,
) -> Inspector
where
    S: Subscription,
    F: FnMut(Cow<'_, str>) -> Next + Send + 'static,
{
    assert!(
        options.max_line_length.bytes() > 0,
        "LineParsingOptions::max_line_length must be greater than zero"
    );
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = consume_sync(
        subscription,
        InspectLines {
            parser: LineParserState::new(),
            options,
            f,
        },
        term_sig_rx,
    );
    Inspector(Consumer {
        stream_name,
        task: Some(tokio::spawn(async move {
            driver.await.map_err(ConsumerTaskError::from)
        })),
        task_termination_sender: Some(term_sig_tx),
    })
}

pub(crate) fn inspect_lines_async<S, F, Fut>(
    stream_name: &'static str,
    subscription: S,
    f: F,
    options: LineParsingOptions,
) -> Inspector
where
    S: Subscription,
    F: FnMut(Cow<'_, str>) -> Fut + Send + 'static,
    Fut: Future<Output = Next> + Send + 'static,
{
    assert!(
        options.max_line_length.bytes() > 0,
        "LineParsingOptions::max_line_length must be greater than zero"
    );
    let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    let driver = consume_async(
        subscription,
        InspectLinesAsync {
            parser: LineParserState::new(),
            options,
            f,
        },
        term_sig_rx,
    );
    Inspector(Consumer {
        stream_name,
        task: Some(tokio::spawn(async move {
            driver.await.map_err(ConsumerTaskError::from)
        })),
        task_termination_sender: Some(term_sig_tx),
    })
}

/// Errors that can occur when inspecting stream data.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InspectorError {
    /// The inspector task could not be joined/terminated.
    #[error("Failed to join/terminate the inspector task over stream '{stream_name}': {source}")]
    TaskJoin {
        /// The name of the stream this inspector operates on.
        stream_name: &'static str,

        /// The source error.
        #[source]
        source: tokio::task::JoinError,
    },

    /// The underlying stream failed while being read.
    #[error("{source}")]
    StreamRead {
        /// The source error.
        #[source]
        source: StreamReadError,
    },
}

/// The result of [`Inspector::cancel_or_abort_after`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorCancelOutcome {
    /// The inspector observed cooperative cancellation before the timeout.
    Cancelled,

    /// The timeout elapsed, so the inspector task was aborted.
    Aborted,
}

/// An inspector for stream data, inspecting it chunk by chunk.
///
/// See the `inspect_*` functions on `BroadcastOutputStream` and `SingleSubscriberOutputStream`.
///
/// For proper cleanup, call
/// - `wait()`, which waits for the collection task to complete.
/// - `cancel()`, which asks the inspector to stop and then waits for cooperative completion.
/// - `abort()`, which forcefully aborts the inspector task.
/// - `cancel_or_abort_after()`, which tries cooperative cancellation first and aborts on timeout.
///
/// If not cleaned up, the termination signal will be sent when dropping this inspector,
/// but the task will be aborted (forceful, not waiting for its regular completion).
pub struct Inspector(pub(crate) Consumer<()>);

fn into_inspector_error(err: ConsumerError) -> InspectorError {
    match err {
        ConsumerError::TaskJoin {
            stream_name,
            source,
        } => InspectorError::TaskJoin {
            stream_name,
            source,
        },
        ConsumerError::StreamRead { source } => InspectorError::StreamRead { source },
        // SinkWrite is unreachable: an Inspector wraps Consumer<()>, whose task body never calls
        // a writer sink. The variant exists on ConsumerError because writer-backed consumers
        // share the same type.
        ConsumerError::SinkWrite { .. } => {
            unreachable!("Inspector tasks never produce SinkWrite errors")
        }
    }
}

impl Inspector {
    /// Returns whether the inspector task has finished.
    ///
    /// This is a non-blocking task-state check. A finished inspector still owns its task result
    /// until [`wait`](Self::wait), [`cancel`](Self::cancel), [`abort`](Self::abort), or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after) consumes it.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.0.is_finished()
    }

    /// Waits for the inspector to terminate naturally.
    ///
    /// An inspector will automatically terminate when either:
    ///
    /// 1. The underlying write-side of the stream is dropped.
    /// 2. The underlying stream is closed (by receiving an EOF / final read of 0 bytes).
    /// 3. The first `Next::Break` is observed.
    ///
    /// If none of these may occur in your case, this can hang forever. `wait` also waits for any
    /// in-flight async inspector callback to complete.
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined, or
    /// [`InspectorError::StreamRead`] if the underlying stream fails while being read.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal task has already been taken.
    pub async fn wait(self) -> Result<(), InspectorError> {
        self.0.wait().await.map_err(into_inspector_error)
    }

    /// Sends a cooperative cancellation event to the inspector and waits for it to stop.
    ///
    /// Cancellation is observed only between stream events. If an async inspector callback is
    /// already in progress, `cancel` waits for that callback to finish before the inspector can
    /// observe cancellation. As a result, `cancel` can hang if the in-flight callback future
    /// hangs.
    ///
    /// For bounded cleanup of a potentially stuck inspector, use [`abort`](Self::abort) or
    /// [`cancel_or_abort_after`](Self::cancel_or_abort_after).
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined, or
    /// [`InspectorError::StreamRead`] if the underlying stream fails while being read before the
    /// cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal cancellation sender has already been taken.
    pub async fn cancel(self) -> Result<(), InspectorError> {
        self.0.cancel().await.map_err(into_inspector_error)
    }

    /// Forcefully aborts the inspector task.
    ///
    /// This drops any pending async inspector callback future and releases the stream
    /// subscription. It cannot preempt blocking synchronous code that never yields to the async
    /// runtime.
    ///
    /// For single-subscriber streams, the consumer claim is released after the aborted task has
    /// been joined during this method.
    pub async fn abort(self) {
        self.0.abort().await;
    }

    /// Cooperatively cancels the inspector, aborting it if `timeout` elapses first.
    ///
    /// Returns [`InspectorCancelOutcome::Cancelled`] when the inspector observes cancellation and
    /// exits normally before the timeout. Returns [`InspectorCancelOutcome::Aborted`] when the
    /// timeout elapses; in that case the task is aborted and any pending callback future is
    /// dropped.
    ///
    /// Cancellation is still cooperative until the timeout boundary: an in-flight async callback
    /// must finish before cancellation can be observed. For single-subscriber streams, the
    /// consumer claim is released before this method returns, both after successful cooperative
    /// cancellation and after timeout-driven abort.
    ///
    /// # Errors
    ///
    /// Returns [`InspectorError::TaskJoin`] if the inspector task cannot be joined before the
    /// timeout, or [`InspectorError::StreamRead`] if the underlying stream fails while being read
    /// before cancellation is observed.
    ///
    /// # Panics
    ///
    /// Panics if the inspector's internal cancellation sender has already been taken.
    pub async fn cancel_or_abort_after(
        self,
        timeout: Duration,
    ) -> Result<InspectorCancelOutcome, InspectorError> {
        match self.0.cancel_or_abort_after(timeout).await {
            Ok(ConsumerCancelOutcome::Cancelled(())) => Ok(InspectorCancelOutcome::Cancelled),
            Ok(ConsumerCancelOutcome::Aborted) => Ok(InspectorCancelOutcome::Aborted),
            Err(err) => Err(into_inspector_error(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::event_receiver;
    use super::*;
    use crate::InspectorError;
    use crate::output_stream::event::StreamEvent;
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::io;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    #[test]
    fn stream_read_display_uses_source_context() {
        let source = StreamReadError::new("stderr", io::Error::from(io::ErrorKind::BrokenPipe));
        let expected = source.to_string();
        let err = InspectorError::StreamRead { source };

        assert_that!(err.to_string()).is_equal_to(expected);
    }

    #[tokio::test]
    async fn cancel_or_abort_after_returns_cancelled_when_cooperative() {
        let (task_termination_sender, task_termination_receiver) = oneshot::channel();
        let inspector = Inspector(Consumer {
            stream_name: "custom",
            task: Some(tokio::spawn(async move {
                let _res = task_termination_receiver.await;
                Ok(())
            })),
            task_termination_sender: Some(task_termination_sender),
        });

        let outcome = inspector
            .cancel_or_abort_after(Duration::from_secs(1))
            .await
            .unwrap();

        assert_that!(outcome).is_equal_to(InspectorCancelOutcome::Cancelled);
    }

    mod inspect_lines {
        use super::*;
        use crate::NumBytesExt;

        #[tokio::test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        async fn panics_when_max_line_length_is_zero() {
            let _inspector = inspect_lines(
                "custom",
                event_receiver(vec![StreamEvent::Eof]).await,
                |_line| Next::Continue,
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: crate::LineOverflowBehavior::default(),
                },
            );
        }

        #[tokio::test]
        async fn inspectors_return_stream_read_error() {
            let error = StreamReadError::new("custom", io::Error::from(io::ErrorKind::BrokenPipe));
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
                Err(InspectorError::StreamRead { source }) => {
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

        #[tokio::test]
        async fn accepts_stateful_callback() {
            let (count_tx, count_rx) = oneshot::channel();
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
        use crate::NumBytesExt;

        #[tokio::test]
        #[should_panic(expected = "LineParsingOptions::max_line_length must be greater than zero")]
        async fn panics_when_max_line_length_is_zero() {
            let _inspector = inspect_lines_async(
                "custom",
                event_receiver(vec![StreamEvent::Eof]).await,
                |_line| async { Next::Continue },
                LineParsingOptions {
                    max_line_length: 0.bytes(),
                    overflow_behavior: crate::LineOverflowBehavior::default(),
                },
            );
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
