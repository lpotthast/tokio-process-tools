use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

#[derive(Debug)]
pub(crate) struct ReadErrorAfterBytes {
    bytes: &'static [u8],
    offset: usize,
    error_kind: io::ErrorKind,
}

impl ReadErrorAfterBytes {
    pub(crate) fn new(bytes: &'static [u8], error_kind: io::ErrorKind) -> Self {
        Self {
            bytes,
            offset: 0,
            error_kind,
        }
    }
}

impl AsyncRead for ReadErrorAfterBytes {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.offset < self.bytes.len() {
            let remaining = &self.bytes[self.offset..];
            let len = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..len]);
            self.offset += len;
            return Poll::Ready(Ok(()));
        }

        Poll::Ready(Err(io::Error::new(
            self.error_kind,
            "injected read failure",
        )))
    }
}

pub(crate) async fn write_test_data(mut write: impl AsyncWrite + Unpin) {
    write.write_all("Cargo.lock\n".as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    write.write_all("Cargo.toml\n".as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    write.write_all("README.md\n".as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    write.write_all("src\n".as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    write.write_all("target\n".as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[derive(Debug)]
pub(crate) struct FailingWrite {
    fail_after_successful_writes: usize,
    error_kind: io::ErrorKind,
    pub(crate) write_calls: usize,
    pub(crate) bytes_written: usize,
}

impl FailingWrite {
    pub(crate) fn new(fail_after_successful_writes: usize, error_kind: io::ErrorKind) -> Self {
        Self {
            fail_after_successful_writes,
            error_kind,
            write_calls: 0,
            bytes_written: 0,
        }
    }
}

impl AsyncWrite for FailingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.write_calls += 1;
        if self.write_calls > self.fail_after_successful_writes {
            return Poll::Ready(Err(io::Error::new(
                self.error_kind,
                "injected write failure",
            )));
        }

        self.bytes_written += buf.len();
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
