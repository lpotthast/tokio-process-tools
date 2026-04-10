use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::impls::{
    impl_collect_chunks, impl_collect_chunks_async, impl_collect_lines, impl_collect_lines_async,
    impl_inspect_chunks, impl_inspect_lines, impl_inspect_lines_async, visit_final_line,
    visit_lines,
};
use crate::output_stream::{
    BackpressureControl, Chunk, FromStreamOptions, LineWriteMode, Next, OutputStream, StreamEvent,
};
use crate::{LineParsingOptions, NumBytes, WaitForLineResult};
use atomic_take::AtomicTake;
use std::borrow::Cow;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one consumer.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::broadcast::BroadcastOutputSteam`.
pub struct SingleSubscriberOutputStream {
    /// The task that captured our `mpsc::Sender` and is now asynchronously awaiting
    /// new output from the underlying stream, sending it to our registered receiver (if present).
    stream_reader: JoinHandle<()>,

    /// The receiver is wrapped in a `Cell<Option<>>` to allow interior mutability and to take the
    /// receiver out and move it into an inspector or collector task.
    /// This enables `&self` methods while tracking if the receiver has been taken.
    /// Once taken by a consumer, attempting to create another consumer will panic with a clear
    /// message, stating that a broadcast subscriber should be used instead.
    receiver: AtomicTake<mpsc::Receiver<StreamEvent>>,

    /// The maximum size of every chunk read by the backing `stream_reader`.
    chunk_size: NumBytes,

    /// The maximum capacity of the channel caching the chunks before being processed.
    max_channel_capacity: usize,

    /// The backpressure strategy used by the stream reader.
    backpressure_control: BackpressureControl,

    /// Name of this stream.
    name: &'static str,
}

impl OutputStream for SingleSubscriberOutputStream {
    fn chunk_size(&self) -> NumBytes {
        self.chunk_size
    }

    fn channel_capacity(&self) -> usize {
        self.max_channel_capacity
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl Drop for SingleSubscriberOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
    }
}

impl Debug for SingleSubscriberOutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleSubscriberOutputStream")
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field(
                "receiver",
                &"non-debug < tokio::sync::mpsc::Receiver<StreamEvent> >",
            )
            .finish()
    }
}

/// Uses a single `bytes::BytesMut` instance into which the input stream is read.
/// Every chunk sent into `sender` is a frozen slice of that buffer.
/// Once chunks were handled by all active receivers, the space of the chunk is reclaimed and reused.
async fn read_chunked<R: AsyncRead + Unpin + Send + 'static>(
    mut read: R,
    chunk_size: NumBytes,
    sender: mpsc::Sender<StreamEvent>,
    backpressure_control: BackpressureControl,
) {
    struct AfterSend {
        do_break: bool,
    }

    fn try_send_event(
        event: StreamEvent,
        sender: &mpsc::Sender<StreamEvent>,
        lagged: &mut usize,
    ) -> AfterSend {
        match sender.try_send(event) {
            Ok(()) => {
                if *lagged > 0 {
                    tracing::debug!(lagged, "Stream reader is lagging behind");
                    *lagged = 0;
                }
            }
            Err(err) => {
                match err {
                    TrySendError::Full(_data) => {
                        *lagged += 1;
                    }
                    TrySendError::Closed(_data) => {
                        // All receivers already dropped.
                        // We intentionally ignore this error.
                        // If it occurs, the user just isn't interested in
                        // newer chunks anymore.
                        return AfterSend { do_break: true };
                    }
                }
            }
        }
        AfterSend { do_break: false }
    }

    async fn send_event(event: StreamEvent, sender: &mpsc::Sender<StreamEvent>) -> AfterSend {
        match sender.send(event).await {
            Ok(()) => {}
            Err(_err) => {
                // All receivers already dropped.
                // We intentionally ignore this error.
                // If it occurs, the user just isn't interested in
                // newer chunks anymore.
                return AfterSend { do_break: true };
            }
        }
        AfterSend { do_break: false }
    }

    // NOTE: buf may grow when required!
    let mut buf = bytes::BytesMut::with_capacity(chunk_size.bytes());
    let mut lagged: usize = 0;
    let mut gap_pending = false;
    'outer: loop {
        let _ = buf.try_reclaim(chunk_size.bytes());
        match read.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                let is_eof = bytes_read == 0;

                match is_eof {
                    true => match backpressure_control {
                        BackpressureControl::DropLatestIncomingIfBufferFull => {
                            if gap_pending {
                                let after = send_event(StreamEvent::Gap, &sender).await;
                                if after.do_break {
                                    break 'outer;
                                }
                                gap_pending = false;
                            }
                            let after = send_event(StreamEvent::Eof, &sender).await;
                            if after.do_break {
                                break 'outer;
                            }
                        }
                        BackpressureControl::BlockUntilBufferHasSpace => {
                            let after = send_event(StreamEvent::Eof, &sender).await;
                            if after.do_break {
                                break 'outer;
                            }
                        }
                    },
                    false => {
                        while !buf.is_empty() {
                            let split_to = usize::min(chunk_size.bytes(), buf.len());
                            let event = StreamEvent::Chunk(Chunk(buf.split_to(split_to).freeze()));

                            match backpressure_control {
                                BackpressureControl::DropLatestIncomingIfBufferFull => {
                                    if gap_pending {
                                        let after =
                                            try_send_event(StreamEvent::Gap, &sender, &mut lagged);
                                        if after.do_break {
                                            break 'outer;
                                        }
                                        if lagged != 0 {
                                            gap_pending = true;
                                            continue;
                                        }
                                        gap_pending = false;
                                    }

                                    let after = try_send_event(event, &sender, &mut lagged);
                                    if after.do_break {
                                        break 'outer;
                                    }
                                    if lagged != 0 {
                                        gap_pending = true;
                                    }
                                }
                                BackpressureControl::BlockUntilBufferHasSpace => {
                                    let after = send_event(event, &sender).await;
                                    if after.do_break {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                };

                if is_eof {
                    break;
                }
            }
            Err(err) => panic!("Could not read from stream: {err}"),
        }
    }
}

impl SingleSubscriberOutputStream {
    /// Creates a new single subscriber output stream from an async read stream.
    pub fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        stream_name: &'static str,
        backpressure_control: BackpressureControl,
        options: FromStreamOptions,
    ) -> SingleSubscriberOutputStream {
        let (tx_stdout, rx_stdout) = mpsc::channel::<StreamEvent>(options.channel_capacity);

        let stream_reader = tokio::spawn(read_chunked(
            stream,
            options.chunk_size,
            tx_stdout,
            backpressure_control,
        ));

        SingleSubscriberOutputStream {
            stream_reader,
            receiver: AtomicTake::new(rx_stdout),
            chunk_size: options.chunk_size,
            max_channel_capacity: options.channel_capacity,
            backpressure_control,
            name: stream_name,
        }
    }

    /// Returns the configured backpressure policy.
    pub fn backpressure_control(&self) -> BackpressureControl {
        self.backpressure_control
    }

    fn take_receiver(&self) -> mpsc::Receiver<StreamEvent> {
        self.receiver.take().unwrap_or_else(|| {
            panic!(
                "Cannot create multiple consumers on SingleSubscriberOutputStream (stream: '{}'). \
                Only one inspector or collector can be active at a time. \
                Use .spawn_broadcast() instead of .spawn_single_subscriber() to support multiple consumers.",
                self.name
            )
        })
    }
}

// Expected types:
// receiver: tokio::sync::mpsc::Receiver<StreamEvent>
// term_rx: tokio::sync::oneshot::Receiver<()>
macro_rules! handle_subscription {
    ($loop_label:tt, $receiver:expr, $term_rx:expr, |$chunk:ident| $body:block) => {
        $loop_label: loop {
            tokio::select! {
                out = $receiver.recv() => {
                    match out {
                        Some(event) => {
                            let $chunk = event;
                            $body
                        }
                        None => {
                            // All senders have been dropped.
                            break $loop_label;
                        }
                    }
                }
                _msg = &mut $term_rx => break $loop_label,
            }
        }
    };
}

// Impls for inspecting the output of the stream.
impl SingleSubscriberOutputStream {
    /// Inspects chunks of output from the stream without storing them.
    ///
    /// The provided closure is called for each chunk of data. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&self, f: impl Fn(Chunk) -> Next + Send + 'static) -> Inspector {
        let mut receiver = self.take_receiver();
        impl_inspect_chunks!(self.name(), receiver, f, handle_subscription)
    }

    /// Inspects lines of output from the stream without storing them.
    ///
    /// The provided closure is called for each line. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(
        &self,
        mut f: impl FnMut(Cow<'_, str>) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector {
        let mut receiver = self.take_receiver();
        impl_inspect_lines!(self.name(), receiver, f, options, handle_subscription)
    }

    /// Inspects lines of output from the stream without storing them, using an async closure.
    ///
    /// The provided async closure is called for each line. Return [`Next::Continue`] to keep
    /// processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &self,
        mut f: impl FnMut(Cow<'_, str>) -> Fut + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        let mut receiver = self.take_receiver();
        impl_inspect_lines_async!(self.name(), receiver, f, options, handle_subscription)
    }
}

// Impls for collecting the output of the stream.
impl SingleSubscriberOutputStream {
    /// Collects chunks from the stream into a sink.
    ///
    /// The provided closure is called for each chunk, with mutable access to the sink.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        collect: impl Fn(Chunk, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_chunks!(self.name(), receiver, collect, sink, handle_subscription)
    }

    /// Collects chunks from the stream into a sink using an async closure.
    ///
    /// The provided async closure is called for each chunk, with mutable access to the sink.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(Chunk, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_chunks_async!(self.name(), receiver, collect, sink, handle_subscription)
    }

    /// Collects lines from the stream into a sink.
    ///
    /// The provided closure is called for each line, with mutable access to the sink.
    /// Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        collect: impl Fn(Cow<'_, str>, &mut S) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_lines!(
            self.name(),
            receiver,
            collect,
            options,
            sink,
            handle_subscription
        )
    }

    /// Collects lines from the stream into a sink using an async closure.
    ///
    /// The provided async closure is called for each line, with mutable access to the sink.
    /// Return [`Next::Continue`] to keep processing or [`Next::Break`] to stop.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, F>(
        &self,
        into: S,
        collect: F,
        options: LineParsingOptions,
    ) -> Collector<S>
    where
        S: Sink,
        F: for<'a> Fn(Cow<'a, str>, &'a mut S) -> AsyncCollectFn<'a> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_lines_async!(
            self.name(),
            receiver,
            collect,
            options,
            sink,
            handle_subscription
        )
    }

    /// Convenience method to collect all chunks into a `Vec<u8>`.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_vec(&self) -> Collector<Vec<u8>> {
        self.collect_chunks(Vec::new(), |chunk, vec| vec.extend(chunk.as_ref()))
    }

    /// Convenience method to collect all lines into a `Vec<String>`.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_vec(&self, options: LineParsingOptions) -> Collector<Vec<String>> {
        self.collect_lines(
            Vec::new(),
            |line, vec| {
                vec.push(line.into_owned());
                Next::Continue
            },
            options,
        )
    }

    /// Collects chunks into an async writer.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &self,
        write: W,
    ) -> Collector<W> {
        self.collect_chunks_async(write, move |chunk, write| {
            Box::pin(async move {
                if let Err(err) = write.write_all(chunk.as_ref()).await {
                    tracing::warn!("Could not write chunk to write sink: {err:#?}");
                };
                Next::Continue
            })
        })
    }

    /// Collects lines into an async writer.
    ///
    /// Parsed lines no longer include their trailing newline byte, so `mode` controls whether a
    /// `\n` delimiter should be reintroduced for each emitted line.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &self,
        write: W,
        options: LineParsingOptions,
        mode: LineWriteMode,
    ) -> Collector<W> {
        self.collect_lines_async(
            write,
            move |line, write| {
                Box::pin(async move {
                    if let Err(err) = write.write_all(line.as_bytes()).await {
                        tracing::warn!("Could not write line to write sink: {err:#?}");
                    } else if matches!(mode, LineWriteMode::AppendLf)
                        && let Err(err) = write.write_all(b"\n").await
                    {
                        tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                    };
                    Next::Continue
                })
            },
            options,
        )
    }

    /// Collects chunks into an async writer after mapping them with the provided function.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write_mapped<
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
    >(
        &self,
        write: W,
        mapper: impl Fn(Chunk) -> B + Send + Sync + Copy + 'static,
    ) -> Collector<W> {
        self.collect_chunks_async(write, move |chunk, write| {
            Box::pin(async move {
                let mapped = mapper(chunk);
                let mapped = mapped.as_ref();
                if let Err(err) = write.write_all(mapped).await {
                    tracing::warn!("Could not write chunk to write sink: {err:#?}");
                };
                Next::Continue
            })
        })
    }

    /// Collects lines into an async writer after mapping them with the provided function.
    ///
    /// `mode` applies after `mapper`: choose [`LineWriteMode::AsIs`] when the mapped output
    /// already contains delimiters, or [`LineWriteMode::AppendLf`] to append `\n` after each
    /// mapped line.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write_mapped<
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
    >(
        &self,
        write: W,
        mapper: impl Fn(Cow<'_, str>) -> B + Send + Sync + Copy + 'static,
        options: LineParsingOptions,
        mode: LineWriteMode,
    ) -> Collector<W> {
        self.collect_lines_async(
            write,
            move |line, write| {
                Box::pin(async move {
                    let mapped = mapper(line);
                    let mapped = mapped.as_ref();
                    if let Err(err) = write.write_all(mapped).await {
                        tracing::warn!("Could not write line to write sink: {err:#?}");
                    } else if matches!(mode, LineWriteMode::AppendLf)
                        && let Err(err) = write.write_all(b"\n").await
                    {
                        tracing::warn!("Could not write line delimiter to write sink: {err:#?}");
                    };
                    Next::Continue
                })
            },
            options,
        )
    }
}

// Impls for waiting for a specific line of output.
impl SingleSubscriberOutputStream {
    async fn wait_for_line_inner(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> WaitForLineResult {
        let mut receiver = self.take_receiver();
        let mut parser = crate::output_stream::LineParserState::new();

        loop {
            match receiver.recv().await {
                Some(StreamEvent::Chunk(chunk)) => {
                    if visit_lines(chunk.as_ref(), &mut parser, options, |line| {
                        if predicate(line) {
                            Next::Break
                        } else {
                            Next::Continue
                        }
                    }) == Next::Break
                    {
                        return WaitForLineResult::Matched;
                    }
                }
                Some(StreamEvent::Gap) => {
                    parser.on_gap();
                }
                Some(StreamEvent::Eof) | None => {
                    if visit_final_line(&parser, |line| {
                        if predicate(line) {
                            Next::Break
                        } else {
                            Next::Continue
                        }
                    }) == Next::Break
                    {
                        return WaitForLineResult::Matched;
                    }
                    return WaitForLineResult::StreamClosed;
                }
            }
        }
    }

    /// Waits for a line that matches the given predicate.
    ///
    /// Returns [`WaitForLineResult::Matched`] if a matching line is found, or
    /// [`WaitForLineResult::StreamClosed`] if the stream ends first.
    /// This method never returns [`WaitForLineResult::Timeout`]; use
    /// [`SingleSubscriberOutputStream::wait_for_line_with_timeout`] if you need a bounded wait.
    ///
    /// This method consumes the only receiver owned by the single-subscriber stream. After calling
    /// it, no other inspector or collector can be created for the same stream. Use the broadcast
    /// stream implementation if you need multiple consumers.
    ///
    /// When chunks are dropped because [`BackpressureControl::DropLatestIncomingIfBufferFull`]
    /// is active, this waiter discards any partial line in progress and resynchronizes at the next
    /// newline instead of matching across the gap.
    pub async fn wait_for_line(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) -> WaitForLineResult {
        self.wait_for_line_inner(predicate, options).await
    }

    /// Waits for a line that matches the given predicate, with a timeout.
    ///
    /// Returns [`WaitForLineResult::Matched`] if a matching line is found,
    /// [`WaitForLineResult::StreamClosed`] if the stream ends first, or
    /// [`WaitForLineResult::Timeout`] if the timeout expires first.
    /// This is the only line-wait variant on this type that can return
    /// [`WaitForLineResult::Timeout`].
    ///
    /// This method consumes the only receiver owned by the single-subscriber stream. After calling
    /// it, no other inspector or collector can be created for the same stream. Use the broadcast
    /// stream implementation if you need multiple consumers.
    ///
    /// When chunks are dropped because [`BackpressureControl::DropLatestIncomingIfBufferFull`]
    /// is active, this waiter discards any partial line in progress and resynchronizes at the next
    /// newline instead of matching across the gap.
    pub async fn wait_for_line_with_timeout(
        &self,
        predicate: impl Fn(Cow<'_, str>) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
        timeout: Duration,
    ) -> WaitForLineResult {
        tokio::time::timeout(timeout, self.wait_for_line_inner(predicate, options))
            .await
            .unwrap_or(WaitForLineResult::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use crate::output_stream::Chunk;
    use crate::output_stream::StreamEvent;
    use crate::output_stream::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::{BackpressureControl, FromStreamOptions, LineWriteMode, Next};
    use crate::single_subscriber::read_chunked;
    use crate::{LineParsingOptions, NumBytesExt, WaitForLineResult};
    use assertr::prelude::*;
    use atomic_take::AtomicTake;
    use bytes::Bytes;
    use mockall::{automock, predicate};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn read_chunked_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
    {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let (tx, mut rx) = mpsc::channel(64);

        // Let's preemptively write more data into the stream than our later selected chunk size (2)
        // can handle, forcing the initial read to completely fill our chunk buffer.
        // Our expectation is that we still receive all data written here through multiple
        // consecutive reads.
        // The behavior of bytes::BytesMut, potentially reaching zero capacity when splitting a
        // full buffer of, must not prevent this from happening but allocate more memory instead!
        write_half.write_all(b"hello world").await.unwrap();
        write_half.flush().await.unwrap();

        let stream_reader = tokio::spawn(read_chunked(
            read_half,
            2.bytes(),
            tx,
            BackpressureControl::DropLatestIncomingIfBufferFull,
        ));

        drop(write_half); // This closes the stream and should let stream_reader terminate.
        stream_reader.await.unwrap();

        let mut chunks = Vec::<String>::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Chunk(chunk) => {
                    chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
                }
                StreamEvent::Gap => {}
                StreamEvent::Eof => break,
            }
        }
        assert_that!(chunks).contains_exactly(["he", "ll", "o ", "wo", "rl", "d"]);
    }

    #[tokio::test]
    async fn wait_for_line_returns_matched_when_line_appears_before_eof() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let waiter = tokio::spawn(async move {
            os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default())
                .await
        });

        write_half.write_all(b"booting\nready\n").await.unwrap();
        write_half.flush().await.unwrap();
        drop(write_half);

        let result = waiter.await.unwrap();
        assert_eq!(result, WaitForLineResult::Matched);
    }

    #[tokio::test]
    async fn wait_for_line_returns_stream_closed_when_stream_ends_before_match() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let waiter = tokio::spawn(async move {
            os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default())
                .await
        });

        write_half
            .write_all(b"booting\nstill starting\n")
            .await
            .unwrap();
        write_half.flush().await.unwrap();
        drop(write_half);

        let result = waiter.await.unwrap();
        assert_eq!(result, WaitForLineResult::StreamClosed);
    }

    #[tokio::test]
    async fn wait_for_line_returns_matched_for_partial_final_line_at_eof() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let waiter = tokio::spawn(async move {
            os.wait_for_line(|line| line.contains("ready"), LineParsingOptions::default())
                .await
        });

        write_half.write_all(b"booting\nready").await.unwrap();
        write_half.flush().await.unwrap();
        drop(write_half);

        let result = waiter.await.unwrap();
        assert_eq!(result, WaitForLineResult::Matched);
    }

    #[tokio::test]
    async fn wait_for_line_with_timeout_returns_timeout_while_stream_stays_open() {
        let (read_half, _write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let result = os
            .wait_for_line_with_timeout(
                |line| line.contains("ready"),
                LineParsingOptions::default(),
                Duration::from_millis(25),
            )
            .await;

        assert_eq!(result, WaitForLineResult::Timeout);
    }

    #[tokio::test]
    async fn wait_for_line_returns_stream_closed_when_stream_ends_after_writes_without_match() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        write_half.write_all(b"booting\n").await.unwrap();
        write_half.flush().await.unwrap();
        drop(write_half);

        // No yield needed: `SingleSubscriberOutputStream` is built on an mpsc channel
        // that buffers every chunk and the terminal EOF event regardless of when the
        // consumer attaches, so this is race-free by construction.
        let result = os
            .wait_for_line(|line| line.contains("ready"), LineParsingOptions::default())
            .await;

        assert_eq!(result, WaitForLineResult::StreamClosed);
    }

    #[tokio::test]
    async fn wait_for_line_does_not_match_across_explicit_gap_event() {
        let (tx, rx) = mpsc::channel::<StreamEvent>(4);
        let os = SingleSubscriberOutputStream {
            stream_reader: tokio::spawn(async {}),
            receiver: AtomicTake::new(rx),
            chunk_size: 4.bytes(),
            max_channel_capacity: 4,
            backpressure_control: BackpressureControl::DropLatestIncomingIfBufferFull,
            name: "custom",
        };

        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"rea"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Gap).await.unwrap();
        tx.send(StreamEvent::Chunk(Chunk(Bytes::from_static(b"dy\n"))))
            .await
            .unwrap();
        tx.send(StreamEvent::Eof).await.unwrap();
        drop(tx);

        let result = os
            .wait_for_line(|line| line == "ready", LineParsingOptions::default())
            .await;

        assert_eq!(result, WaitForLineResult::StreamClosed);
    }

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                channel_capacity: 2,
                ..Default::default()
            },
        );

        let inspector = os.inspect_lines_async(
            |_line| async move {
                // Mimic a slow consumer.
                sleep(Duration::from_millis(100)).await;
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        #[rustfmt::skip]
        let producer = tokio::spawn(async move {
            for count in 1..=15 {
                write_half
                    .write_all(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                sleep(Duration::from_millis(25)).await;
            }
        });

        producer.await.unwrap();
        inspector.wait().await.unwrap();
        drop(os);

        logs_assert(|lines: &[&str]| {
            let lagged_logs = lines
                .iter()
                .filter(|line| line.contains("Stream reader is lagging behind lagged="))
                .count();
            if lagged_logs == 0 {
                return Err("Expected at least one lagged log".to_string());
            }
            Ok(())
        });
    }

    #[tokio::test]
    async fn inspect_lines() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        #[automock]
        trait LineVisitor {
            fn visit(&self, line: String);
        }

        let mut mock = MockLineVisitor::new();
        #[rustfmt::skip]
        fn configure(mock: &mut MockLineVisitor) {
            mock.expect_visit().with(predicate::eq("Cargo.lock".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("Cargo.toml".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("README.md".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("src".to_string())).times(1).return_const(());
            mock.expect_visit().with(predicate::eq("target".to_string())).times(1).return_const(());
        }
        configure(&mut mock);

        let inspector = os.inspect_lines(
            move |line| {
                mock.visit(line.into_owned());
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        inspector.cancel().await.unwrap();
        drop(os)
    }

    /// This tests that our impl macros properly `break 'outer`, as they might be in an inner loop!
    /// With `break` instead of `break 'outer`, this test would never complete, as the `Next::Break`
    /// would not terminate the collector!
    #[tokio::test]
    #[traced_test]
    async fn inspect_lines_async() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32.bytes(),
                ..Default::default()
            },
        );

        let seen: Vec<String> = Vec::new();
        let collector = os.collect_lines_async(
            seen,
            move |line, seen: &mut Vec<String>| {
                Box::pin(async move {
                    if line == "break" {
                        seen.push(line.into_owned());
                        Next::Break
                    } else {
                        seen.push(line.into_owned());
                        Next::Continue
                    }
                })
            },
            LineParsingOptions::default(),
        );

        let _writer = tokio::spawn(async move {
            write_half.write_all("start\n".as_bytes()).await.unwrap();
            write_half.write_all("break\n".as_bytes()).await.unwrap();
            write_half.write_all("end\n".as_bytes()).await.unwrap();

            loop {
                write_half
                    .write_all("gibberish\n".as_bytes())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let seen = collector.wait().await.unwrap();

        assert_that!(seen).contains_exactly(["start", "break"]);
    }

    #[tokio::test]
    async fn collect_lines_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                channel_capacity: 32,
                ..Default::default()
            },
        );

        let temp_file = tempfile::tempfile().unwrap();
        let collector = os.collect_lines(
            temp_file,
            |line, temp_file| {
                writeln!(temp_file, "{}", line).unwrap();
                Next::Continue
            },
            LineParsingOptions::default(),
        );

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();

        assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    #[tokio::test]
    async fn collect_lines_async_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32.bytes(),
                ..Default::default()
            },
        );

        let temp_file = tempfile::tempfile().unwrap();
        let collector = os.collect_lines_async(
            temp_file,
            |line, temp_file| {
                Box::pin(async move {
                    writeln!(temp_file, "{}", line).unwrap();
                    Next::Continue
                })
            },
            LineParsingOptions::default(),
        );

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();

        assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    #[tokio::test]
    async fn collect_lines_into_write_respects_requested_line_delimiter_mode() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let temp_file = tokio::fs::File::from_std(tempfile::tempfile().unwrap());
        let collector = os.collect_lines_into_write(
            temp_file,
            LineParsingOptions::default(),
            LineWriteMode::AsIs,
        );

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).await.unwrap();

        assert_that!(contents).is_equal_to("Cargo.lockCargo.tomlREADME.mdsrctarget");
    }

    #[tokio::test]
    #[traced_test]
    async fn collect_chunks_into_write_mapped() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32.bytes(),
                ..Default::default()
            },
        );

        let temp_file = tokio::fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(std::env::temp_dir().join(
                "tokio_process_tools_test_single_subscriber_collect_chunks_into_write_mapped.txt",
            ))
            .await
            .unwrap();

        let collector = os.collect_chunks_into_write_mapped(temp_file, |chunk| {
            String::from_utf8_lossy(chunk.as_ref()).to_string()
        });

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).await.unwrap();

        assert_that!(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    #[tokio::test]
    #[traced_test]
    async fn multiple_subscribers_are_not_possible() {
        let (read_half, _write_half) = tokio::io::duplex(64);
        let os = SingleSubscriberOutputStream::from_stream(
            read_half,
            "custom",
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions::default(),
        );

        let _inspector = os.inspect_lines(|_line| Next::Continue, Default::default());

        // Doesn't matter if we call `inspect_lines` or some other "consuming" function instead.
        assert_that_panic_by(move || os.inspect_lines(|_line| Next::Continue, Default::default()))
            .has_type::<String>()
            .is_equal_to("Cannot create multiple consumers on SingleSubscriberOutputStream (stream: 'custom'). Only one inspector or collector can be active at a time. Use .spawn_broadcast() instead of .spawn_single_subscriber() to support multiple consumers.");
    }
}
