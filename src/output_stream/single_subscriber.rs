use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::impls::{
    impl_collect_chunks, impl_collect_chunks_async, impl_collect_lines, impl_collect_lines_async,
    impl_inspect_chunks, impl_inspect_lines, impl_inspect_lines_async,
};
use crate::output_stream::{
    BackpressureControl, Chunk, FromStreamOptions, LineReader, Next, OutputStream,
};
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one consumer.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::broadcast::BroadcastOutputSteam`.
pub struct SingleSubscriberOutputStream {
    /// The task that captured the `tokio::sync::mpsc::Sender` part and is asynchronously awaiting
    /// new output, sending it to our receiver in chunks. TODO: of up to size ???
    stream_reader: JoinHandle<()>,

    /// The receiver is wrapped in an `Option` so that we can take it out of it and move it
    /// into an inspector or collector task.
    receiver: Option<mpsc::Receiver<Option<Chunk>>>,
}

impl OutputStream for SingleSubscriberOutputStream {}

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
                &"non-debug < tokio::sync::mpsc::Receiver<Option<Chunk>> >",
            )
            .finish()
    }
}

async fn read_chunked<B: AsyncRead + Unpin + Send + 'static>(
    mut reader: BufReader<B>,
    chunk_size: usize,
    sender: mpsc::Sender<Option<Chunk>>,
    backpressure_control: BackpressureControl,
) {
    struct AfterSend {
        do_break: bool,
    }

    fn try_send_chunk(
        chunk: Option<Chunk>,
        sender: &mpsc::Sender<Option<Chunk>>,
        lagged: &mut usize,
    ) -> AfterSend {
        match sender.try_send(chunk) {
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

    async fn send_chunk(chunk: Option<Chunk>, sender: &mpsc::Sender<Option<Chunk>>) -> AfterSend {
        match sender.send(chunk).await {
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
    let mut buf = bytes::BytesMut::with_capacity(chunk_size);
    let mut lagged: usize = 0;
    loop {
        let _ = buf.try_reclaim(chunk_size);
        match reader.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                let is_eof = bytes_read == 0;

                match is_eof {
                    true => match backpressure_control {
                        BackpressureControl::DropLatestIncomingIfBufferFull => {
                            let after = try_send_chunk(None, &sender, &mut lagged);
                            if after.do_break {
                                break;
                            }
                        }
                        BackpressureControl::BlockUntilBufferHasSpace => {
                            let after = send_chunk(None, &sender).await;
                            if after.do_break {
                                break;
                            }
                        }
                    },
                    false => {
                        while !buf.is_empty() {
                            let split_to = usize::min(chunk_size, buf.len());

                            match backpressure_control {
                                BackpressureControl::DropLatestIncomingIfBufferFull => {
                                    let after = try_send_chunk(
                                        Some(Chunk(buf.split_to(split_to).freeze())),
                                        &sender,
                                        &mut lagged,
                                    );
                                    if after.do_break {
                                        break;
                                    }
                                }
                                BackpressureControl::BlockUntilBufferHasSpace => {
                                    let after = send_chunk(
                                        Some(Chunk(buf.split_to(split_to).freeze())),
                                        &sender,
                                    )
                                    .await;
                                    if after.do_break {
                                        break;
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
    pub fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        backpressure_control: BackpressureControl,
        options: FromStreamOptions,
    ) -> SingleSubscriberOutputStream {
        let (tx_stdout, rx_stdout) = mpsc::channel::<Option<Chunk>>(options.channel_capacity);

        let buf_reader = BufReader::with_capacity(options.read_buffer_size, stream);
        let stream_reader = tokio::spawn(read_chunked(
            buf_reader,
            options.chunk_size,
            tx_stdout,
            backpressure_control,
        ));

        SingleSubscriberOutputStream {
            stream_reader,
            receiver: Some(rx_stdout),
        }
    }

    fn take_receiver(&mut self) -> mpsc::Receiver<Option<Chunk>> {
        self.receiver.take().unwrap()
    }
}

// Expected types:
// receiver: tokio::sync::mpsc::Receiver<Option<Chunk>>
// term_rx: tokio::sync::oneshot::Receiver<()>
macro_rules! handle_subscription {
    ($loop_label:tt, $receiver:expr, $term_rx:expr, |$chunk:ident| $body:block) => {
        $loop_label: loop {
            tokio::select! {
                out = $receiver.recv() => {
                    match out {
                        Some(maybe_chunk) => {
                            let $chunk = maybe_chunk;
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
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&mut self, f: impl Fn(Chunk) -> Next + Send + 'static) -> Inspector {
        let mut receiver = self.take_receiver();
        impl_inspect_chunks!(receiver, f, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(
        &mut self,
        mut f: impl FnMut(String) -> Next + Send + 'static,
    ) -> Inspector {
        let mut receiver = self.take_receiver();
        impl_inspect_lines!(receiver, f, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &mut self,
        mut f: impl FnMut(String) -> Fut + Send + 'static,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        let mut receiver = self.take_receiver();
        impl_inspect_lines_async!(receiver, f, handle_subscription)
    }
}

// Impls for collecting the output of the stream.
impl SingleSubscriberOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &mut self,
        into: S,
        collect: impl Fn(Chunk, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_chunks!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &mut self,
        into: S,
        collect: impl Fn(String, &mut S) -> Next + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_lines!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, F>(&mut self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(Chunk, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_chunks_async!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, F>(&mut self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.take_receiver();
        impl_collect_lines_async!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_vec(&mut self) -> Collector<Vec<u8>> {
        self.collect_chunks(Vec::new(), |chunk, vec| vec.extend(chunk.as_ref()))
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_vec(&mut self) -> Collector<Vec<String>> {
        self.collect_lines(Vec::new(), |line, vec| {
            vec.push(line);
            Next::Continue
        })
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &mut self,
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

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &mut self,
        write: W,
    ) -> Collector<W> {
        self.collect_lines_async(write, move |line, write| {
            Box::pin(async move {
                if let Err(err) = write.write_all(line.as_bytes()).await {
                    tracing::warn!("Could not write line to write sink: {err:#?}");
                };
                Next::Continue
            })
        })
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write_mapped<
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
    >(
        &mut self,
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

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write_mapped<
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
    >(
        &mut self,
        write: W,
        mapper: impl Fn(String) -> B + Send + Sync + Copy + 'static,
    ) -> Collector<W> {
        self.collect_lines_async(write, move |line, write| {
            Box::pin(async move {
                let mapped = mapper(line);
                let mapped = mapped.as_ref();
                if let Err(err) = write.write_all(mapped).await {
                    tracing::warn!("Could not write line to write sink: {err:#?}");
                };
                Next::Continue
            })
        })
    }
}

// Impls for waiting for a specific line of output.
impl SingleSubscriberOutputStream {
    pub async fn wait_for_line(
        &mut self,
        predicate: impl Fn(String) -> bool + Send + Sync + 'static,
    ) {
        let inspector = self.inspect_lines(move |line| {
            if predicate(line) {
                Next::Break
            } else {
                Next::Continue
            }
        });
        // TODO: Should we propagate the error here?
        inspector.wait().await.unwrap();
    }

    pub async fn wait_for_line_with_timeout(
        &mut self,
        predicate: impl Fn(String) -> bool + Send + Sync + 'static,
        timeout: Duration,
    ) -> Result<(), Elapsed> {
        tokio::time::timeout(timeout, self.wait_for_line(predicate)).await
    }
}

#[cfg(test)]
mod tests {
    use crate::output_stream::single_subscriber::SingleSubscriberOutputStream;
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::{BackpressureControl, FromStreamOptions, Next};
    use crate::single_subscriber::read_chunked;
    use assertr::prelude::*;
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
        let buf_reader = tokio::io::BufReader::new(read_half);

        // Let's preemptively write more data into the stream than our later selected chunk size (2)
        // can handle, forcing the initial read to completely fill our chunk buffer.
        // Our expectation is that we still receive all data written here through multiple
        // consecutive reads.
        // The behavior of bytes::BytesMut, potentially reaching zero capacity when splitting a
        // full buffer of, must not prevent this from happening but allocate more memory instead!
        write_half.write_all(b"hello world").await.unwrap();
        write_half.flush().await.unwrap();

        let stream_reader = tokio::spawn(read_chunked(
            buf_reader,
            2,
            tx,
            BackpressureControl::DropLatestIncomingIfBufferFull,
        ));

        drop(write_half); // This closes the stream and should let stream_reader terminate.
        stream_reader.await.unwrap();

        let mut chunks = Vec::<String>::new();
        while let Some(Some(chunk)) = rx.recv().await {
            chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
        }
        assert_that(chunks).contains_exactly(&["he", "ll", "o ", "wo", "rl", "d"]);
    }

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                channel_capacity: 2,
                ..Default::default()
            },
        );

        let inspector = os.inspect_lines_async(async |_line| {
            // Mimic a slow consumer.
            sleep(Duration::from_millis(100)).await;
            Next::Continue
        });

        #[rustfmt::skip]
        let producer = tokio::spawn(async move {
            for count in 1..=15 {
                write_half
                    .write(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                sleep(Duration::from_millis(25)).await;
            }
        });

        producer.await.unwrap();
        inspector.wait().await.unwrap();
        drop(os);

        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|line| line.contains("Stream reader is lagging behind lagged=1"))
                .count()
            {
                1 => {}
                n => return Err(format!("Expected exactly one lagged=1 log, but found {n}")),
            };
            match lines
                .iter()
                .filter(|line| line.contains("Stream reader is lagging behind lagged=3"))
                .count()
            {
                2 => {}
                n => return Err(format!("Expected exactly two lagged=3 logs, but found {n}")),
            };
            Ok(())
        });
    }

    #[tokio::test]
    async fn inspect_lines() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
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

        let inspector = os.inspect_lines(move |line| {
            mock.visit(line);
            Next::Continue
        });

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
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32,
                ..Default::default()
            },
        );

        let seen: Vec<String> = Vec::new();
        let collector = os.collect_lines_async(seen, move |line, seen: &mut Vec<String>| {
            Box::pin(async move {
                if line == "break" {
                    seen.push(line);
                    Next::Break
                } else {
                    seen.push(line);
                    Next::Continue
                }
            })
        });

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

        assert_that(seen).contains_exactly(&["start", "break"]);
    }

    #[tokio::test]
    async fn collect_lines_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                channel_capacity: 32,
                ..Default::default()
            },
        );

        let temp_file = tempfile::tempfile().unwrap();
        let collector = os.collect_lines(temp_file, |line, temp_file| {
            writeln!(temp_file, "{}", line).unwrap();
            Next::Continue
        });

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();

        assert_that(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    #[tokio::test]
    async fn collect_lines_async_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32,
                ..Default::default()
            },
        );

        let temp_file = tempfile::tempfile().unwrap();
        let collector = os.collect_lines_async(temp_file, |line, temp_file| {
            Box::pin(async move {
                writeln!(temp_file, "{}", line).unwrap();
                Next::Continue
            })
        });

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).unwrap();

        assert_that(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    #[tokio::test]
    #[traced_test]
    async fn collect_chunks_into_write_mapped() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            BackpressureControl::DropLatestIncomingIfBufferFull,
            FromStreamOptions {
                chunk_size: 32,
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

        assert_that(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }

    // TODO: Multiple subscribers are not possible!
}
