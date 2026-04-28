use crate::StreamReadError;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::{Next, Subscription};
use std::future::Future;
use tokio::sync::oneshot;

/// A synchronous visitor that observes stream events and produces a final value.
///
/// `StreamVisitor` is the synchronous counterpart to [`AsyncStreamVisitor`]. Implement it on a
/// type that needs to react to chunks, gaps, and EOF without `.await`-ing between events, then
/// drive it via `consume_with` to obtain a [`Consumer`](crate::Consumer) handle that owns the
/// resulting tokio task.
///
/// All built-in consumer factories (`inspect_*`, `collect_*`, `wait_for_line`) construct a
/// built-in visitor and call `consume_with` internally; this trait is what users implement to
/// plug in custom logic without wrapping a closure in shared mutable state.
///
/// # Lifecycle
///
/// 1. [`on_chunk`](StreamVisitor::on_chunk) is invoked for every observed chunk. Return
///    [`Next::Continue`] to keep going or [`Next::Break`] to stop early.
/// 2. [`on_gap`](StreamVisitor::on_gap) is invoked when the stream backend reports that chunks
///    were dropped (e.g., best-effort delivery overflow). Use it to reset partial-line buffers
///    or other accumulated state.
/// 3. [`on_eof`](StreamVisitor::on_eof) is invoked exactly once when the stream ends naturally.
///    It is *not* invoked when the visitor returned [`Next::Break`], nor when the consumer task
///    is cancelled or aborted.
/// 4. [`into_output`](StreamVisitor::into_output) consumes `self` and returns the value the
///    [`Consumer`](crate::Consumer)'s `wait`/`cancel` methods yield.
///
/// # Example
///
/// ```rust, no_run
/// # use tokio_process_tools::{Chunk, Next, StreamVisitor};
/// /// Counts chunks and stops after `limit`.
/// struct CountUntil { count: usize, limit: usize }
///
/// impl StreamVisitor for CountUntil {
///     type Output = usize;
///
///     fn on_chunk(&mut self, _chunk: Chunk) -> Next {
///         self.count += 1;
///         if self.count >= self.limit { Next::Break } else { Next::Continue }
///     }
///
///     fn into_output(self) -> usize { self.count }
/// }
/// ```
pub trait StreamVisitor: Send + 'static {
    /// The value produced by [`into_output`](StreamVisitor::into_output) after the visitor has
    /// finished observing the stream. Returned via [`Consumer::wait`](crate::Consumer::wait) and
    /// [`Consumer::cancel`](crate::Consumer::cancel).
    type Output: Send + 'static;

    /// Invoked for every chunk observed on the stream.
    ///
    /// Return [`Next::Continue`] to keep visiting, or [`Next::Break`] to stop without consuming
    /// further events; in the latter case [`on_eof`](StreamVisitor::on_eof) is not called.
    fn on_chunk(&mut self, chunk: Chunk) -> Next;

    /// Invoked when the stream backend reports that one or more chunks were dropped between the
    /// last delivered chunk and the next one.
    ///
    /// The default implementation does nothing. Override it to reset any partial-line buffers or
    /// other accumulated state that would be invalidated by the gap.
    fn on_gap(&mut self) {}

    /// Invoked exactly once when the stream ends (EOF or write side dropped).
    ///
    /// Not called when the visitor returned [`Next::Break`] from
    /// [`on_chunk`](StreamVisitor::on_chunk), nor when the consumer task is cancelled or aborted
    /// before the stream ends.
    fn on_eof(&mut self) {}

    /// Consumes the visitor and returns its final output.
    ///
    /// Called after the visitor has stopped observing events (via EOF, `Break`, or cancellation).
    /// The returned value is what the owning [`Consumer`](crate::Consumer)'s `wait`/`cancel`
    /// methods yield.
    fn into_output(self) -> Self::Output;
}

/// An asynchronous visitor that observes stream events and produces a final value.
///
/// `AsyncStreamVisitor` is the asynchronous counterpart to [`StreamVisitor`]. Use it when
/// observing a chunk needs to `.await` (network I/O, async writers, channel sends).
///
/// The trait uses return-position `impl Future` rather than `async fn` to keep the `Send` bound
/// on the returned future expressible on stable Rust; this is the same shape used by
/// [`AsyncChunkCollector`](crate::AsyncChunkCollector) and
/// [`AsyncLineCollector`](crate::AsyncLineCollector). See [`StreamVisitor`] for the lifecycle
/// description; the only difference is that `on_chunk` and `on_eof` are async.
///
/// # Example
///
/// ```rust, no_run
/// # use std::future::Future;
/// # use tokio_process_tools::{AsyncStreamVisitor, Chunk, Next};
/// /// Forwards every chunk to an mpsc channel.
/// struct ForwardChunks { tx: tokio::sync::mpsc::Sender<Vec<u8>> }
///
/// impl AsyncStreamVisitor for ForwardChunks {
///     type Output = ();
///
///     async fn on_chunk(&mut self, chunk: Chunk) -> Next {
///         match self.tx.send(chunk.as_ref().to_vec()).await {
///             Ok(()) => Next::Continue,
///             Err(_) => Next::Break,
///         }
///     }
///
///     fn into_output(self) {}
/// }
/// ```
pub trait AsyncStreamVisitor: Send + 'static {
    /// The value produced by [`into_output`](AsyncStreamVisitor::into_output) after the visitor
    /// has finished observing the stream. Returned via [`Consumer::wait`](crate::Consumer::wait)
    /// and [`Consumer::cancel`](crate::Consumer::cancel).
    type Output: Send + 'static;

    /// Asynchronously observes a single chunk.
    ///
    /// Return [`Next::Continue`] to keep visiting, or [`Next::Break`] to stop without consuming
    /// further events; in the latter case [`on_eof`](AsyncStreamVisitor::on_eof) is not called.
    fn on_chunk(&mut self, chunk: Chunk) -> impl Future<Output = Next> + Send + '_;

    /// Invoked when the stream backend reports that one or more chunks were dropped between the
    /// last delivered chunk and the next one.
    ///
    /// Synchronous because gap notification carries no payload to await on. The default
    /// implementation does nothing.
    fn on_gap(&mut self) {}

    /// Asynchronously observes end-of-stream.
    ///
    /// Not called when the visitor returned [`Next::Break`] from
    /// [`on_chunk`](AsyncStreamVisitor::on_chunk), nor when the consumer task is cancelled or
    /// aborted before the stream ends. The default implementation is a no-op.
    fn on_eof(&mut self) -> impl Future<Output = ()> + Send + '_ {
        async {}
    }

    /// Consumes the visitor and returns its final output.
    ///
    /// Called after the visitor has stopped observing events. Synchronous because no further
    /// stream interaction is required at this point.
    fn into_output(self) -> Self::Output;
}

pub(crate) async fn consume_sync<S, V>(
    mut subscription: S,
    mut visitor: V,
    mut term_sig_rx: oneshot::Receiver<()>,
) -> Result<V::Output, StreamReadError>
where
    S: Subscription,
    V: StreamVisitor,
{
    loop {
        tokio::select! {
            out = subscription.next_event() => {
                match out {
                    Some(StreamEvent::Chunk(chunk)) => {
                        if visitor.on_chunk(chunk) == Next::Break {
                            break;
                        }
                    }
                    Some(StreamEvent::Gap) => visitor.on_gap(),
                    Some(StreamEvent::Eof) | None => {
                        visitor.on_eof();
                        break;
                    }
                    Some(StreamEvent::ReadError(err)) => return Err(err),
                }
            }
            _msg = &mut term_sig_rx => break,
        }
    }
    Ok(visitor.into_output())
}

pub(crate) async fn consume_async<S, V>(
    mut subscription: S,
    mut visitor: V,
    mut term_sig_rx: oneshot::Receiver<()>,
) -> Result<V::Output, StreamReadError>
where
    S: Subscription,
    V: AsyncStreamVisitor,
{
    loop {
        tokio::select! {
            out = subscription.next_event() => {
                match out {
                    Some(StreamEvent::Chunk(chunk)) => {
                        if visitor.on_chunk(chunk).await == Next::Break {
                            break;
                        }
                    }
                    Some(StreamEvent::Gap) => visitor.on_gap(),
                    Some(StreamEvent::Eof) | None => {
                        visitor.on_eof().await;
                        break;
                    }
                    Some(StreamEvent::ReadError(err)) => return Err(err),
                }
            }
            _msg = &mut term_sig_rx => break,
        }
    }
    Ok(visitor.into_output())
}
