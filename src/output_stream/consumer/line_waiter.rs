use crate::{StreamReadError, WaitForLineResult};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Future returned by output stream line-wait methods.
///
/// Backend methods create the stream subscription before returning this value, so output cannot
/// race ahead before the future is first polled.
pub struct LineWaiter {
    inner:
        Pin<Box<dyn Future<Output = Result<WaitForLineResult, StreamReadError>> + Send + 'static>>,
}

impl LineWaiter {
    pub(crate) fn new(
        future: impl Future<Output = Result<WaitForLineResult, StreamReadError>> + Send + 'static,
    ) -> Self {
        Self {
            inner: Box::pin(future),
        }
    }
}

impl Future for LineWaiter {
    type Output = Result<WaitForLineResult, StreamReadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.as_mut().poll(cx)
    }
}
