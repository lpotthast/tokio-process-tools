use super::collect::{AsyncChunkCollector, AsyncLineCollector, Sink};
use crate::StreamReadError;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use crate::output_stream::{Next, Subscription};
use std::borrow::Cow;
use std::future::Future;
use tokio::sync::oneshot;

pub(crate) trait Visitor: Send + 'static {
    type Output: Send + 'static;

    fn on_chunk(&mut self, chunk: Chunk) -> Next;

    fn on_gap(&mut self) {}

    fn on_eof(&mut self) {}

    fn into_output(self) -> Self::Output;
}

pub(crate) trait AsyncVisitor: Send + 'static {
    type Output: Send + 'static;

    fn on_chunk(&mut self, chunk: Chunk) -> impl Future<Output = Next> + Send + '_;

    fn on_gap(&mut self) {}

    fn on_eof(&mut self) -> impl Future<Output = ()> + Send + '_ {
        async {}
    }

    fn into_output(self) -> Self::Output;
}

pub(crate) async fn drive_sync<S, V>(
    mut subscription: S,
    mut visitor: V,
    mut term_sig_rx: oneshot::Receiver<()>,
) -> Result<V::Output, StreamReadError>
where
    S: Subscription,
    V: Visitor,
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

pub(crate) async fn drive_async<S, V>(
    mut subscription: S,
    mut visitor: V,
    mut term_sig_rx: oneshot::Receiver<()>,
) -> Result<V::Output, StreamReadError>
where
    S: Subscription,
    V: AsyncVisitor,
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

pub(crate) struct WaitForLine<P> {
    pub parser: LineParserState,
    pub options: LineParsingOptions,
    pub predicate: P,
    pub matched: bool,
}

impl<P> Visitor for WaitForLine<P>
where
    P: Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
{
    type Output = bool;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            predicate,
            matched,
        } = self;
        parser.visit_chunk(chunk.as_ref(), *options, |line| {
            if predicate(line) {
                *matched = true;
                Next::Break
            } else {
                Next::Continue
            }
        })
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
    }

    fn on_eof(&mut self) {
        let Self {
            parser,
            predicate,
            matched,
            ..
        } = self;
        let _ = parser.finish(|line| {
            if predicate(line) {
                *matched = true;
                Next::Break
            } else {
                Next::Continue
            }
        });
    }

    fn into_output(self) -> Self::Output {
        self.matched
    }
}

pub(crate) struct InspectChunks<F> {
    pub f: F,
}

impl<F> Visitor for InspectChunks<F>
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

impl<F, Fut> AsyncVisitor for InspectChunksAsync<F>
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

impl<F> Visitor for InspectLines<F>
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

impl<F, Fut> AsyncVisitor for InspectLinesAsync<F>
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

pub(crate) struct CollectChunks<T, F> {
    pub sink: T,
    pub f: F,
}

impl<T, F> Visitor for CollectChunks<T, F>
where
    T: Sink,
    F: FnMut(Chunk, &mut T) + Send + 'static,
{
    type Output = T;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        (self.f)(chunk, &mut self.sink);
        Next::Continue
    }

    fn into_output(self) -> Self::Output {
        self.sink
    }
}

pub(crate) struct CollectChunksAsync<T, C> {
    pub sink: T,
    pub collector: C,
}

impl<T, C> AsyncVisitor for CollectChunksAsync<T, C>
where
    T: Sink,
    C: AsyncChunkCollector<T>,
{
    type Output = T;

    fn on_chunk(&mut self, chunk: Chunk) -> impl Future<Output = Next> + Send + '_ {
        self.collector.collect(chunk, &mut self.sink)
    }

    fn into_output(self) -> Self::Output {
        self.sink
    }
}

pub(crate) struct CollectLines<T, F> {
    pub parser: LineParserState,
    pub options: LineParsingOptions,
    pub sink: T,
    pub f: F,
}

impl<T, F> Visitor for CollectLines<T, F>
where
    T: Sink,
    F: FnMut(Cow<'_, str>, &mut T) -> Next + Send + 'static,
{
    type Output = T;

    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            sink,
            f,
        } = self;
        parser.visit_chunk(chunk.as_ref(), *options, |line| f(line, sink))
    }

    fn on_gap(&mut self) {
        self.parser.on_gap();
    }

    fn on_eof(&mut self) {
        if let Some(line) = self.parser.finish_owned() {
            let _ = (self.f)(Cow::Owned(line), &mut self.sink);
        }
    }

    fn into_output(self) -> Self::Output {
        self.sink
    }
}

pub(crate) struct CollectLinesAsync<T, C> {
    pub parser: LineParserState,
    pub options: LineParsingOptions,
    pub sink: T,
    pub collector: C,
}

impl<T, C> AsyncVisitor for CollectLinesAsync<T, C>
where
    T: Sink,
    C: AsyncLineCollector<T>,
{
    type Output = T;

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            parser,
            options,
            sink,
            collector,
        } = self;
        for line in parser.owned_lines(chunk.as_ref(), *options) {
            if collector.collect(Cow::Owned(line), sink).await == Next::Break {
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
            let _ = self
                .collector
                .collect(Cow::Owned(line), &mut self.sink)
                .await;
        }
    }

    fn into_output(self) -> Self::Output {
        self.sink
    }
}
