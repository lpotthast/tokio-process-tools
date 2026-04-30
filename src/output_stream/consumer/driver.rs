//! Tokio-bound driver loops that step a [`StreamVisitor`] / [`AsyncStreamVisitor`] over a
//! [`Subscription`] until EOF, `Break`, cancellation, or a stream read error.
//!
//! The traits these loops drive are runtime-agnostic and live one level up at
//! [`crate::output_stream::visitor`]; the loops themselves are tokio-bound because they use
//! `tokio::select!` to race the next stream event against a `oneshot::Receiver` cancellation
//! token. The [`spawn_consumer_*`] helpers package the loop into a [`Consumer<S>`] handle by
//! `tokio::spawn`-ing the driver and wiring the cancellation channel.

use crate::StreamReadError;
use crate::output_stream::Next;
use crate::output_stream::Subscription;
use crate::output_stream::consumer::Consumer;
use crate::output_stream::event::StreamEvent;
use crate::output_stream::visitor::{AsyncStreamVisitor, StreamVisitor};
use tokio::sync::oneshot;

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

pub(crate) fn spawn_consumer_sync<S, V>(
    stream_name: &'static str,
    subscription: S,
    visitor: V,
) -> Consumer<V::Output>
where
    S: Subscription,
    V: StreamVisitor,
{
    let (term_sig_tx, term_sig_rx) = oneshot::channel::<()>();
    let driver = consume_sync(subscription, visitor, term_sig_rx);
    Consumer {
        stream_name,
        task: Some(tokio::spawn(driver)),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn spawn_consumer_async<S, V>(
    stream_name: &'static str,
    subscription: S,
    visitor: V,
) -> Consumer<V::Output>
where
    S: Subscription,
    V: AsyncStreamVisitor,
{
    let (term_sig_tx, term_sig_rx) = oneshot::channel::<()>();
    let driver = consume_async(subscription, visitor, term_sig_rx);
    Consumer {
        stream_name,
        task: Some(tokio::spawn(driver)),
        task_termination_sender: Some(term_sig_tx),
    }
}
