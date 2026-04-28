use super::collect::{Collector, CollectorTaskError, Sink};
use super::collect_owned_final_line;
use crate::output_stream::Subscription;
use crate::output_stream::event::{Chunk, StreamEvent};
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use std::borrow::Cow;
use std::io;
use tokio::io::AsyncWriteExt;

/// Controls how line-based write helpers delimit successive lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineWriteMode {
    /// Write lines exactly as parsed, without appending any delimiter.
    ///
    /// Use this when your mapper already includes delimiters or when the downstream format does
    /// not want line separators reintroduced.
    AsIs,

    /// Append a trailing `\n` after each emitted line.
    ///
    /// This reconstructs conventional line-oriented output after parsing removed the original
    /// newline byte.
    AppendLf,
}

/// Action to take after an async writer sink rejects collected output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkWriteErrorAction {
    /// Stop collection and return [`crate::CollectorError::SinkWrite`] from the collector.
    Stop,

    /// Accept the individual write failure and keep collecting later stream output.
    Continue,
}

/// The write operation that failed while forwarding collected output into an async writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkWriteOperation {
    /// A raw output chunk failed to write.
    Chunk,

    /// Parsed line bytes failed to write.
    Line,

    /// The line delimiter requested by [`crate::LineWriteMode::AppendLf`] failed to write.
    LineDelimiter,
}

/// Details about a failed async write into a collector sink.
#[derive(Debug)]
pub struct SinkWriteError {
    stream_name: &'static str,
    operation: SinkWriteOperation,
    attempted_len: usize,
    source: io::Error,
}

impl SinkWriteError {
    pub(crate) fn new(
        stream_name: &'static str,
        operation: SinkWriteOperation,
        attempted_len: usize,
        source: io::Error,
    ) -> Self {
        Self {
            stream_name,
            operation,
            attempted_len,
            source,
        }
    }

    /// The name of the stream this collector operates on.
    #[must_use]
    pub fn stream_name(&self) -> &'static str {
        self.stream_name
    }

    /// The write operation that failed.
    #[must_use]
    pub fn operation(&self) -> SinkWriteOperation {
        self.operation
    }

    /// Number of bytes passed to the failed `write_all` call.
    #[must_use]
    pub fn attempted_len(&self) -> usize {
        self.attempted_len
    }

    /// The underlying async writer error.
    #[must_use]
    pub fn source(&self) -> &io::Error {
        &self.source
    }

    pub(crate) fn into_source(self) -> io::Error {
        self.source
    }
}

/// Handles async writer sink failures observed by writer collectors.
pub trait SinkWriteErrorHandler: Send + 'static {
    /// Decide whether collection should continue after a sink write failure.
    fn handle(&mut self, error: &SinkWriteError) -> SinkWriteErrorAction;
}

impl<F> SinkWriteErrorHandler for F
where
    F: FnMut(&SinkWriteError) -> SinkWriteErrorAction + Send + 'static,
{
    fn handle(&mut self, error: &SinkWriteError) -> SinkWriteErrorAction {
        self(error)
    }
}

/// Options for forwarding collected stream output into an async writer.
///
/// Use [`WriteCollectionOptions::fail_fast`] to stop on the first sink write failure,
/// [`WriteCollectionOptions::log_and_continue`] to preserve best-effort logging behavior, or
/// [`WriteCollectionOptions::with_error_handler`] to make a custom per-error decision.
#[derive(Debug, Clone, Copy)]
pub struct WriteCollectionOptions<H = fn(&SinkWriteError) -> SinkWriteErrorAction> {
    error_handler: H,
}

impl WriteCollectionOptions {
    /// Creates writer collection options that fail on the first sink write error.
    #[must_use]
    pub fn fail_fast() -> Self {
        Self {
            error_handler: |_| SinkWriteErrorAction::Stop,
        }
    }

    /// Creates writer collection options that log sink write errors and keep collecting.
    #[must_use]
    pub fn log_and_continue() -> Self {
        Self {
            error_handler: |error| {
                tracing::warn!(
                    stream = error.stream_name(),
                    operation = ?error.operation(),
                    attempted_len = error.attempted_len(),
                    source = %error.source(),
                    "Could not write collected output to write sink; continuing"
                );
                SinkWriteErrorAction::Continue
            },
        }
    }

    /// Creates writer collection options with a custom sink write error handler.
    #[must_use]
    pub fn with_error_handler<H>(handler: H) -> WriteCollectionOptions<H>
    where
        H: FnMut(&SinkWriteError) -> SinkWriteErrorAction + Send + 'static,
    {
        WriteCollectionOptions {
            error_handler: handler,
        }
    }
}

impl<H> WriteCollectionOptions<H> {
    pub(crate) fn into_error_handler(self) -> H {
        self.error_handler
    }
}

pub(crate) fn collect_chunks_into_write<S, W, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: Subscription,
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                handle_write_result(
                                    stream_name,
                                    &mut error_handler,
                                    SinkWriteOperation::Chunk,
                                    chunk.as_ref().len(),
                                    write.write_all(chunk.as_ref()).await,
                                )?;
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_chunks_into_write_mapped<S, W, B, F, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    mapper: F,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: Subscription,
    W: Sink + AsyncWriteExt + Unpin,
    B: AsRef<[u8]> + Send,
    F: Fn(Chunk) -> B + Send + Sync + Copy + 'static,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                let mapped_output = mapper(chunk);
                                handle_write_result(
                                    stream_name,
                                    &mut error_handler,
                                    SinkWriteOperation::Chunk,
                                    mapped_output.as_ref().len(),
                                    write.write_all(mapped_output.as_ref()).await,
                                )?;
                            }
                            Some(StreamEvent::Gap) => {}
                            Some(StreamEvent::Eof) | None => break,
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_into_write<S, W, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    options: LineParsingOptions,
    mode: LineWriteMode,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: Subscription,
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        line.as_bytes(),
                                        mode,
                                    )
                                    .await?;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        line.as_bytes(),
                                        mode,
                                    )
                                    .await?;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

pub(crate) fn collect_lines_into_write_mapped<S, W, B, F, H>(
    stream_name: &'static str,
    mut subscription: S,
    write: W,
    mapper: F,
    options: LineParsingOptions,
    mode: LineWriteMode,
    write_options: WriteCollectionOptions<H>,
) -> Collector<W>
where
    S: Subscription,
    W: Sink + AsyncWriteExt + Unpin,
    B: AsRef<[u8]> + Send,
    F: Fn(Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
    H: SinkWriteErrorHandler,
{
    let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();
    Collector {
        stream_name,
        task: Some(tokio::spawn(async move {
            let mut parser = LineParserState::new();
            let mut write = write;
            let mut error_handler = write_options.into_error_handler();
            loop {
                tokio::select! {
                    out = subscription.next_event() => {
                        match out {
                            Some(StreamEvent::Chunk(chunk)) => {
                                for line in parser.owned_lines(chunk.as_ref(), options) {
                                    let mapped_output = mapper(Cow::Owned(line));
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        mapped_output.as_ref(),
                                        mode,
                                    )
                                    .await?;
                                }
                            }
                            Some(StreamEvent::Gap) => parser.on_gap(),
                            Some(StreamEvent::Eof) | None => {
                                if let Some(line) = collect_owned_final_line(&parser) {
                                    let mapped_output = mapper(Cow::Owned(line));
                                    write_line(
                                        stream_name,
                                        &mut write,
                                        &mut error_handler,
                                        mapped_output.as_ref(),
                                        mode,
                                    )
                                    .await?;
                                }
                                break;
                            }
                            Some(StreamEvent::ReadError(err)) => return Err(err.into()),
                        }
                    }
                    _msg = &mut term_sig_rx => break,
                }
            }
            Ok(write)
        })),
        task_termination_sender: Some(term_sig_tx),
    }
}

async fn write_line<W, H>(
    stream_name: &'static str,
    write: &mut W,
    error_handler: &mut H,
    line: &[u8],
    mode: LineWriteMode,
) -> Result<(), CollectorTaskError>
where
    W: AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
{
    let line_write = write.write_all(line).await;
    let line_written = handle_write_result(
        stream_name,
        error_handler,
        SinkWriteOperation::Line,
        line.len(),
        line_write,
    )?;
    if !line_written || !matches!(mode, LineWriteMode::AppendLf) {
        return Ok(());
    }

    handle_write_result(
        stream_name,
        error_handler,
        SinkWriteOperation::LineDelimiter,
        1,
        write.write_all(b"\n").await,
    )?;

    Ok(())
}

fn handle_write_result<H>(
    stream_name: &'static str,
    error_handler: &mut H,
    operation: SinkWriteOperation,
    attempted_len: usize,
    result: io::Result<()>,
) -> Result<bool, CollectorTaskError>
where
    H: SinkWriteErrorHandler,
{
    match result {
        Ok(()) => Ok(true),
        Err(source) => {
            let error = SinkWriteError::new(stream_name, operation, attempted_len, source);
            match error_handler.handle(&error) {
                SinkWriteErrorAction::Stop => {
                    Err(CollectorTaskError::SinkWrite(error.into_source()))
                }
                SinkWriteErrorAction::Continue => Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::event_receiver;
    use super::*;
    use crate::CollectorError;
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::cell::Cell;
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    #[derive(Debug)]
    struct FailingWrite {
        fail_after_successful_writes: usize,
        error_kind: io::ErrorKind,
        write_calls: usize,
        bytes_written: usize,
    }

    impl FailingWrite {
        fn new(fail_after_successful_writes: usize, error_kind: io::ErrorKind) -> Self {
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

    #[derive(Default)]
    struct SendOnlyWrite {
        bytes: Vec<u8>,
        write_calls: Cell<usize>,
    }

    impl AsyncWrite for SendOnlyWrite {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write_calls.set(self.write_calls.get() + 1);
            self.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn chunk_writer_reports_and_can_handle_sink_write_errors() {
        let collector = collect_chunks_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"abc"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            WriteCollectionOptions::fail_fast(),
        );

        match collector.wait().await {
            Err(CollectorError::SinkWrite {
                stream_name,
                source,
            }) => {
                assert_that!(stream_name).is_equal_to("custom");
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
            }
        }

        let handled_count = Arc::new(Mutex::new(0_usize));
        let count_for_handler = Arc::clone(&handled_count);
        let collector = collect_chunks_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"abc"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            WriteCollectionOptions::with_error_handler(move |err| {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
                *count_for_handler.lock().unwrap() += 1;
                SinkWriteErrorAction::Continue
            }),
        );

        let write = collector.wait().await.unwrap();
        assert_that!(write.bytes_written).is_equal_to(0);
        assert_that!(*handled_count.lock().unwrap()).is_equal_to(1);
    }

    #[tokio::test]
    async fn line_writer_reports_line_and_delimiter_write_errors() {
        let line_error = collect_lines_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"line\n"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            LineParsingOptions::default(),
            LineWriteMode::AppendLf,
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await;
        match line_error {
            Err(CollectorError::SinkWrite { source, .. }) => {
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected line write error, got {other:?}"));
            }
        }

        let delimiter_error = collect_lines_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"line\n"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(1, io::ErrorKind::WriteZero),
            LineParsingOptions::default(),
            LineWriteMode::AppendLf,
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await;
        match delimiter_error {
            Err(CollectorError::SinkWrite { source, .. }) => {
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::WriteZero);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected delimiter write error, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn line_writer_respects_requested_delimiter_mode() {
        let collector = collect_lines_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(
                    b"Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n",
                ))),
                StreamEvent::Eof,
            ])
            .await,
            SendOnlyWrite::default(),
            LineParsingOptions::default(),
            LineWriteMode::AsIs,
            WriteCollectionOptions::fail_fast(),
        );

        let writer = collector.wait().await.unwrap();
        assert_that!(writer.bytes).is_equal_to(b"Cargo.lockCargo.tomlREADME.mdsrctarget".to_vec());
    }

    #[tokio::test]
    async fn chunk_writer_accepts_send_only_writer() {
        let collector = collect_chunks_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"abc"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"def"))),
                StreamEvent::Eof,
            ])
            .await,
            SendOnlyWrite::default(),
            WriteCollectionOptions::fail_fast(),
        );

        let writer = collector.wait().await.unwrap();
        assert_that!(writer.bytes).is_equal_to(b"abcdef".to_vec());
        assert_that!(writer.write_calls.get()).is_greater_than(0);
    }

    #[tokio::test]
    async fn chunk_writer_mapped_writes_mapped_output() {
        let collector = collect_chunks_into_write_mapped(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"Cargo.lock\n"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"Cargo.toml\n"))),
                StreamEvent::Eof,
            ])
            .await,
            SendOnlyWrite::default(),
            |chunk| String::from_utf8_lossy(chunk.as_ref()).to_string(),
            WriteCollectionOptions::fail_fast(),
        );

        let writer = collector.wait().await.unwrap();
        assert_that!(writer.bytes).is_equal_to(b"Cargo.lock\nCargo.toml\n".to_vec());
    }

    #[tokio::test]
    async fn mapped_writers_return_sink_write_errors() {
        let chunk_error = collect_chunks_into_write_mapped(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"abc"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::ConnectionReset),
            |chunk| chunk,
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await;
        match chunk_error {
            Err(CollectorError::SinkWrite { source, .. }) => {
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::ConnectionReset);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
            }
        }

        let line_error = collect_lines_into_write_mapped(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"one\n"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            |line| line.into_owned().into_bytes(),
            LineParsingOptions::default(),
            LineWriteMode::AsIs,
            WriteCollectionOptions::fail_fast(),
        )
        .wait()
        .await;
        match line_error {
            Err(CollectorError::SinkWrite { source, .. }) => {
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
            }
        }
    }

    #[tokio::test]
    async fn line_write_error_handler_can_continue_after_sink_write_errors() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let handled_events = Arc::clone(&events);
        let collector = collect_lines_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"a\nb\n"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            LineParsingOptions::default(),
            LineWriteMode::AppendLf,
            WriteCollectionOptions::with_error_handler(move |err| {
                handled_events.lock().unwrap().push((
                    err.stream_name(),
                    err.operation(),
                    err.attempted_len(),
                    err.source().kind(),
                ));
                SinkWriteErrorAction::Continue
            }),
        );

        let write = collector.wait().await.unwrap();
        assert_that!(write.bytes_written).is_equal_to(0);
        assert_that!(events.lock().unwrap().as_slice()).is_equal_to([
            (
                "custom",
                SinkWriteOperation::Line,
                1,
                io::ErrorKind::BrokenPipe,
            ),
            (
                "custom",
                SinkWriteOperation::Line,
                1,
                io::ErrorKind::BrokenPipe,
            ),
        ]);
    }

    #[tokio::test]
    async fn chunk_write_error_handler_can_continue_then_stop() {
        let handled_count = Arc::new(Mutex::new(0_usize));
        let count_for_handler = Arc::clone(&handled_count);
        let collector = collect_chunks_into_write(
            "custom",
            event_receiver(vec![
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"a"))),
                StreamEvent::Chunk(Chunk(Bytes::from_static(b"b"))),
                StreamEvent::Eof,
            ])
            .await,
            FailingWrite::new(0, io::ErrorKind::BrokenPipe),
            WriteCollectionOptions::with_error_handler(move |err| {
                assert_that!(err.operation()).is_equal_to(SinkWriteOperation::Chunk);
                let mut count = count_for_handler.lock().unwrap();
                *count += 1;
                if *count == 1 {
                    SinkWriteErrorAction::Continue
                } else {
                    SinkWriteErrorAction::Stop
                }
            }),
        );

        match collector.wait().await {
            Err(CollectorError::SinkWrite { source, .. }) => {
                assert_that!(source.kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
            }
        }
        assert_that!(*handled_count.lock().unwrap()).is_equal_to(2);
    }
}
