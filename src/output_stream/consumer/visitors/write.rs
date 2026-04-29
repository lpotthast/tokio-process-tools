use super::super::consumer::Sink;
use super::super::visitor::AsyncStreamVisitor;
use super::inspect::assert_max_line_length_non_zero;
use crate::output_stream::Next;
use crate::output_stream::event::Chunk;
use crate::output_stream::line::{LineParserState, LineParsingOptions};
use std::borrow::Cow;
use std::io;
use tokio::io::AsyncWriteExt;
use typed_builder::TypedBuilder;

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
    /// Stop collection and surface the [`SinkWriteError`] as the consumer's output. The
    /// writer-backed consumer's `wait` returns `Ok(Err(sink_write_error))` in that case.
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
}

impl std::fmt::Display for SinkWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to write consumed output from stream '{}' to sink: {}",
            self.stream_name, self.source
        )
    }
}

impl std::error::Error for SinkWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
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

#[derive(TypedBuilder)]
pub(crate) struct WriteChunks<W, H, F, B>
where
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
    B: AsRef<[u8]> + Send + 'static,
    F: Fn(Chunk) -> B + Send + Sync + 'static,
{
    pub stream_name: &'static str,
    pub writer: W,
    pub error_handler: H,
    pub mapper: F,
    pub error: Option<SinkWriteError>,
}

impl<W, H, F, B> AsyncStreamVisitor for WriteChunks<W, H, F, B>
where
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
    B: AsRef<[u8]> + Send + 'static,
    F: Fn(Chunk) -> B + Send + Sync + 'static,
{
    type Output = Result<W, SinkWriteError>;

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let mapped_output = (self.mapper)(chunk);
        let bytes = mapped_output.as_ref();
        let attempted_len = bytes.len();
        let result = self.writer.write_all(bytes).await;
        match handle_write_result(
            self.stream_name,
            &mut self.error_handler,
            SinkWriteOperation::Chunk,
            attempted_len,
            result,
        ) {
            Ok(_) => Next::Continue,
            Err(err) => {
                self.error = Some(err);
                Next::Break
            }
        }
    }

    fn into_output(self) -> Self::Output {
        match self.error {
            Some(err) => Err(err),
            None => Ok(self.writer),
        }
    }
}

#[derive(TypedBuilder)]
pub(crate) struct WriteLines<W, H, F, B>
where
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
    B: AsRef<[u8]> + Send + 'static,
    F: Fn(Cow<'_, str>) -> B + Send + Sync + 'static,
{
    pub stream_name: &'static str,
    pub writer: W,
    pub error_handler: H,
    pub mapper: F,
    pub parser: LineParserState,
    #[builder(setter(transform = |options: LineParsingOptions| {
        assert_max_line_length_non_zero(&options);
        options
    }))]
    pub options: LineParsingOptions,
    pub mode: LineWriteMode,
    pub error: Option<SinkWriteError>,
}

impl<W, H, F, B> AsyncStreamVisitor for WriteLines<W, H, F, B>
where
    W: Sink + AsyncWriteExt + Unpin,
    H: SinkWriteErrorHandler,
    B: AsRef<[u8]> + Send + 'static,
    F: Fn(Cow<'_, str>) -> B + Send + Sync + 'static,
{
    type Output = Result<W, SinkWriteError>;

    async fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let Self {
            stream_name,
            writer,
            error_handler,
            mapper,
            parser,
            options,
            mode,
            error,
        } = self;
        for line in parser.owned_lines(chunk.as_ref(), *options) {
            let mapped_output = mapper(Cow::Owned(line));
            if let Err(err) = write_line(
                stream_name,
                writer,
                error_handler,
                mapped_output.as_ref(),
                *mode,
            )
            .await
            {
                *error = Some(err);
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
            let mapped_output = (self.mapper)(Cow::Owned(line));
            if let Err(err) = write_line(
                self.stream_name,
                &mut self.writer,
                &mut self.error_handler,
                mapped_output.as_ref(),
                self.mode,
            )
            .await
            {
                self.error = Some(err);
            }
        }
    }

    fn into_output(self) -> Self::Output {
        match self.error {
            Some(err) => Err(err),
            None => Ok(self.writer),
        }
    }
}

async fn write_line<W, H>(
    stream_name: &'static str,
    write: &mut W,
    error_handler: &mut H,
    line: &[u8],
    mode: LineWriteMode,
) -> Result<(), SinkWriteError>
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
) -> Result<bool, SinkWriteError>
where
    H: SinkWriteErrorHandler,
{
    match result {
        Ok(()) => Ok(true),
        Err(source) => {
            let error = SinkWriteError::new(stream_name, operation, attempted_len, source);
            match error_handler.handle(&error) {
                SinkWriteErrorAction::Stop => Err(error),
                SinkWriteErrorAction::Continue => Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::consumer::{Consumer, spawn_consumer_async};
    use super::super::super::test_support::event_receiver;
    use super::*;
    use crate::output_stream::Subscription;
    use crate::output_stream::event::StreamEvent;
    use assertr::prelude::*;
    use bytes::Bytes;
    use std::cell::Cell;
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use tokio::io::AsyncWrite;

    // Test-only helpers replacing the deleted factory functions. They build the visitor via
    // the typed builder and spawn a consumer task — the same shape every test wants without
    // each test repeating the builder boilerplate.
    fn collect_chunks_into_write<S, W, H>(
        stream_name: &'static str,
        subscription: S,
        write: W,
        write_options: WriteCollectionOptions<H>,
    ) -> Consumer<Result<W, SinkWriteError>>
    where
        S: Subscription,
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        spawn_consumer_async(
            stream_name,
            subscription,
            WriteChunks::builder()
                .stream_name(stream_name)
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper((|chunk: Chunk| chunk) as fn(Chunk) -> Chunk)
                .error(None)
                .build(),
        )
    }

    fn collect_chunks_into_write_mapped<S, W, B, F, H>(
        stream_name: &'static str,
        subscription: S,
        write: W,
        mapper: F,
        write_options: WriteCollectionOptions<H>,
    ) -> Consumer<Result<W, SinkWriteError>>
    where
        S: Subscription,
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send + 'static,
        F: Fn(Chunk) -> B + Send + Sync + 'static,
        H: SinkWriteErrorHandler,
    {
        spawn_consumer_async(
            stream_name,
            subscription,
            WriteChunks::builder()
                .stream_name(stream_name)
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper(mapper)
                .error(None)
                .build(),
        )
    }

    fn collect_lines_into_write<S, W, H>(
        stream_name: &'static str,
        subscription: S,
        write: W,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Consumer<Result<W, SinkWriteError>>
    where
        S: Subscription,
        W: Sink + AsyncWriteExt + Unpin,
        H: SinkWriteErrorHandler,
    {
        spawn_consumer_async(
            stream_name,
            subscription,
            WriteLines::builder()
                .stream_name(stream_name)
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper((|line: Cow<'_, str>| line.into_owned()) as fn(Cow<'_, str>) -> String)
                .parser(LineParserState::new())
                .options(options)
                .mode(mode)
                .error(None)
                .build(),
        )
    }

    fn collect_lines_into_write_mapped<S, W, B, F, H>(
        stream_name: &'static str,
        subscription: S,
        write: W,
        mapper: F,
        options: LineParsingOptions,
        mode: LineWriteMode,
        write_options: WriteCollectionOptions<H>,
    ) -> Consumer<Result<W, SinkWriteError>>
    where
        S: Subscription,
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send + 'static,
        F: Fn(Cow<'_, str>) -> B + Send + Sync + 'static,
        H: SinkWriteErrorHandler,
    {
        spawn_consumer_async(
            stream_name,
            subscription,
            WriteLines::builder()
                .stream_name(stream_name)
                .writer(write)
                .error_handler(write_options.into_error_handler())
                .mapper(mapper)
                .parser(LineParserState::new())
                .options(options)
                .mode(mode)
                .error(None)
                .build(),
        )
    }

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
            Ok(Err(err)) => {
                assert_that!(err.stream_name()).is_equal_to("custom");
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
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

        let write = collector.wait().await.unwrap().unwrap();
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
            Ok(Err(err)) => {
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
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
            Ok(Err(err)) => {
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::WriteZero);
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

        let writer = collector.wait().await.unwrap().unwrap();
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

        let writer = collector.wait().await.unwrap().unwrap();
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

        let writer = collector.wait().await.unwrap().unwrap();
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
            Ok(Err(err)) => {
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::ConnectionReset);
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
            Ok(Err(err)) => {
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
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

        let write = collector.wait().await.unwrap().unwrap();
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
            Ok(Err(err)) => {
                assert_that!(err.source().kind()).is_equal_to(io::ErrorKind::BrokenPipe);
            }
            other => {
                assert_that!(&other).fail(format_args!("expected sink write error, got {other:?}"));
            }
        }
        assert_that!(*handled_count.lock().unwrap()).is_equal_to(2);
    }
}
