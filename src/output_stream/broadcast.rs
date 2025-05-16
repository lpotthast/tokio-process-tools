use crate::InspectorError;
use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::impls::{
    impl_collect_chunks, impl_collect_chunks_async, impl_collect_lines, impl_collect_lines_async,
    impl_inspect_chunks, impl_inspect_lines, impl_inspect_lines_async,
};
use crate::output_stream::{Chunk, FromStreamOptions, LineReader, Next};
use crate::output_stream::{LineParsingOptions, OutputStream};
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the broadcast variant, allowing for multiple simultaneous consumers with the downside
/// of inducing memory allocations not required when only one consumer is listening.
/// For that case, prefer using the `output_stream::single_subscriber::SingleOutputSteam`.
pub struct BroadcastOutputStream {
    /// The task that captured a clone of our `broadcast::Sender` and is now asynchronously
    /// awaiting new output from the underlying stream, sending it to all registered receivers.
    stream_reader: JoinHandle<()>,

    /// We only store the chunk sender. The initial receiver is dropped immediately after creating
    /// the channel.
    /// New subscribers can be created from this sender.
    sender: broadcast::Sender<Option<Chunk>>,
}

impl OutputStream for BroadcastOutputStream {}

impl Drop for BroadcastOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
    }
}

impl Debug for BroadcastOutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BroadcastOutputStream")
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field(
                "sender",
                &"non-debug < tokio::sync::broadcast::Sender<Option<Chunk>> >",
            )
            .finish()
    }
}

/// Uses a single `bytes::BytesMut` instance into which the input stream is read.
/// Every chunk sent into `sender` is a frozen slice of that buffer.
/// Once chunks were handled by all active receivers, the space of the chunk is reclaimed and reused.
async fn read_chunked<B: AsyncRead + Unpin + Send + 'static>(
    mut read: B,
    chunk_size: usize,
    sender: broadcast::Sender<Option<Chunk>>,
) {
    let send_chunk = move |chunk: Option<Chunk>| {
        // When we could not send the chunk, we get it back in the error value and
        // then drop it. This means that the BytesMut storage portion of that chunk
        // is now reclaimable and can be used for storing the next chunk of incoming
        // bytes.
        match sender.send(chunk) {
            Ok(_received_by) => {}
            Err(err) => {
                // No receivers: All already dropped or none was yet created.
                // We intentionally ignore these errors.
                // If they occur, the user just wasn't interested in seeing this chunk.
                // We won't store it (history) to later feed it back to a new subscriber.
                tracing::debug!(
                    error = %err,
                    "No active receivers for the output chunk, dropping it"
                );
            }
        }
    };

    // A BytesMut may grow when used in a `read_buf` call.
    let mut buf = bytes::BytesMut::with_capacity(chunk_size);
    loop {
        let _ = buf.try_reclaim(chunk_size);
        match read.read_buf(&mut buf).await {
            Ok(bytes_read) => {
                let is_eof = bytes_read == 0;

                match is_eof {
                    true => send_chunk(None),
                    false => {
                        while !buf.is_empty() {
                            // Split of at least `chunk_size` bytes and send it, even if we were
                            // able to read more than `chunk_size` bytes.
                            // We could have read more
                            let split_to = usize::min(chunk_size, buf.len());
                            // Splitting off bytes reduces the remaining capacity of our BytesMut.
                            // It might have now reached a capacity of 0. But this is fine!
                            // The next usage of it in `read_buf` will not return `0`, as you may
                            // expect from the read_buf documentation. The BytesMut will grow
                            // to allow buffering of more data.
                            //
                            // NOTE: If we only split of at max `chunk_size` bytes, we have to repeat
                            // this, unless all data is processed.
                            send_chunk(Some(Chunk(buf.split_to(split_to).freeze())));
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

impl BroadcastOutputStream {
    pub(crate) fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        options: FromStreamOptions,
    ) -> BroadcastOutputStream {
        let (sender, receiver) = broadcast::channel::<Option<Chunk>>(options.channel_capacity);
        drop(receiver);

        let stream_reader = tokio::spawn(read_chunked(stream, options.chunk_size, sender.clone()));

        BroadcastOutputStream {
            stream_reader,
            sender,
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<Option<Chunk>> {
        self.sender.subscribe()
    }
}

// Expected types:
// loop_label: &'static str
// receiver: tokio::sync::broadcast::Receiver<Option<Chunk>>,
// term_rx: tokio::sync::oneshot::Receiver<()>,
macro_rules! handle_subscription {
    ($loop_label:tt, $receiver:expr, $term_rx:expr, |$chunk:ident| $body:block) => {
        $loop_label: loop {
            tokio::select! {
                out = $receiver.recv() => {
                    match out {
                        Ok(maybe_chunk) => {
                            let $chunk = maybe_chunk;
                            $body
                        }
                        Err(RecvError::Closed) => {
                            // All senders have been dropped.
                            break $loop_label;
                        },
                        Err(RecvError::Lagged(lagged)) => {
                            tracing::warn!(lagged, "Inspector is lagging behind");
                        }
                    }
                }
                _msg = &mut $term_rx => break $loop_label,
            }
        }
    };
}

// Impls for inspecting the output of the stream.
impl BroadcastOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&self, mut f: impl FnMut(Chunk) -> Next + Send + 'static) -> Inspector {
        let mut receiver = self.subscribe();
        impl_inspect_chunks!(receiver, f, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(
        &self,
        mut f: impl FnMut(String) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector {
        let mut receiver = self.subscribe();
        impl_inspect_lines!(receiver, f, options, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &self,
        mut f: impl FnMut(String) -> Fut + Send + 'static,
        options: LineParsingOptions,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        let mut receiver = self.subscribe();
        impl_inspect_lines_async!(receiver, f, options, handle_subscription)
    }
}

// Impls for collecting the output of the stream.
impl BroadcastOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        mut collect: impl FnMut(Chunk, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_chunks!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(Chunk, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_chunks_async!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        mut collect: impl FnMut(String, &mut S) -> Next + Send + 'static,
        options: LineParsingOptions,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_lines!(receiver, collect, options, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, F>(
        &self,
        into: S,
        collect: F,
        options: LineParsingOptions,
    ) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_lines_async!(receiver, collect, options, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_vec(&self) -> Collector<Vec<u8>> {
        self.collect_chunks(Vec::new(), |chunk, vec| vec.extend(chunk.as_ref()))
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_vec(&self, options: LineParsingOptions) -> Collector<Vec<String>> {
        self.collect_lines(
            Vec::new(),
            |line, vec| {
                vec.push(line);
                Next::Continue
            },
            options,
        )
    }

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

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &self,
        write: W,
        options: LineParsingOptions,
    ) -> Collector<W> {
        self.collect_lines_async(
            write,
            move |line, write| {
                Box::pin(async move {
                    if let Err(err) = write.write_all(line.as_bytes()).await {
                        tracing::warn!("Could not write line to write sink: {err:#?}");
                    };
                    Next::Continue
                })
            },
            options,
        )
    }

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

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_write_mapped<
        W: Sink + AsyncWriteExt + Unpin,
        B: AsRef<[u8]> + Send,
    >(
        &self,
        write: W,
        mapper: impl Fn(String) -> B + Send + Sync + Copy + 'static,
        options: LineParsingOptions,
    ) -> Collector<W> {
        self.collect_lines_async(
            write,
            move |line, write| {
                Box::pin(async move {
                    let mapped = mapper(line);
                    let mapped = mapped.as_ref();
                    if let Err(err) = write.write_all(mapped).await {
                        tracing::warn!("Could not write line to write sink: {err:#?}");
                    };
                    Next::Continue
                })
            },
            options,
        )
    }
}

// Impls for waiting for a specific line of output.
impl BroadcastOutputStream {
    pub async fn wait_for_line(
        &self,
        predicate: impl Fn(String) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
    ) {
        let inspector = self.inspect_lines(
            move |line| {
                if predicate(line) {
                    Next::Break
                } else {
                    Next::Continue
                }
            },
            options,
        );
        match inspector.wait().await {
            Ok(()) => {}
            Err(err) => match err {
                InspectorError::TaskJoin(join_error) => {
                    panic!("Inspector task join error: {join_error:#?}");
                }
            },
        };
    }

    pub async fn wait_for_line_with_timeout(
        &self,
        predicate: impl Fn(String) -> bool + Send + Sync + 'static,
        options: LineParsingOptions,
        timeout: Duration,
    ) -> Result<(), Elapsed> {
        tokio::time::timeout(timeout, self.wait_for_line(predicate, options)).await
    }
}

/// Configuration for line parsing behavior.
pub struct LineConfig {
    // Existing fields
    // ...
    /// Maximum length of a single line in bytes.
    /// When reached, the current line will be emitted.
    /// A value of 0 means no limit (default).
    pub max_line_length: usize,
}

#[cfg(test)]
mod tests {
    use super::read_chunked;
    use crate::output_stream::NumBytesExt;
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::{FromStreamOptions, LineParsingOptions, Next};
    use assertr::assert_that;
    use assertr::prelude::{LengthAssertions, PartialEqAssertions, VecAssertions};
    use mockall::*;
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio::sync::broadcast;
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn read_chunked_does_not_terminate_when_first_read_can_fill_the_entire_bytes_mut_buffer()
    {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let (tx, mut rx) = broadcast::channel(10);

        // Let's preemptively write more data into the stream than our later selected chunk size (2)
        // can handle, forcing the initial read to completely fill our chunk buffer.
        // Our expectation is that we still receive all data written here through multiple
        // consecutive reads.
        // The behavior of bytes::BytesMut, potentially reaching zero capacity when splitting a
        // full buffer of, must not prevent this from happening!
        write_half.write_all(b"hello world").await.unwrap();
        write_half.flush().await.unwrap();

        let stream_reader = tokio::spawn(read_chunked(read_half, 2, tx));

        drop(write_half); // This closes the stream and should let stream_reader terminate.
        stream_reader.await.unwrap();

        let mut chunks = Vec::<String>::new();
        while let Ok(Some(chunk)) = rx.recv().await {
            chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
        }
        assert_that(chunks).contains_exactly(&["he", "ll", "o ", "wo", "rl", "d"]);
    }

    #[tokio::test]
    #[traced_test]
    async fn read_chunked_no_data() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let (tx, mut rx) = broadcast::channel(10);

        let stream_reader = tokio::spawn(read_chunked(read_half, 2, tx));

        drop(write_half); // This closes the stream and should let stream_reader terminate.
        stream_reader.await.unwrap();

        let mut chunks = Vec::<String>::new();
        while let Ok(Some(chunk)) = rx.recv().await {
            chunks.push(String::from_utf8_lossy(chunk.as_ref()).to_string());
        }
        assert_that(chunks).is_empty();
    }

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(
            read_half,
            FromStreamOptions {
                channel_capacity: 2,
                ..Default::default()
            },
        );

        let consumer = os.inspect_lines_async(
            async |_line| {
                // Mimic a slow consumer.
                sleep(Duration::from_millis(100)).await;
                Next::Continue
            },
            LineParsingOptions {
                max_line_length: 16.kilobytes(),
            },
        );

        #[rustfmt::skip]
        let producer = tokio::spawn(async move {
            for count in 1..=15 {
                write_half
                    .write(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                sleep(Duration::from_millis(25)).await;
            }
            write_half.flush().await.unwrap();
            drop(write_half);
        });

        producer.await.unwrap();
        consumer.wait().await.unwrap();
        drop(os);

        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|line| line.contains("Inspector is lagging behind lagged=1"))
                .count()
            {
                1 => {}
                n => return Err(format!("Expected exactly one lagged=1 log, but found {n}")),
            };
            match lines
                .iter()
                .filter(|line| line.contains("Inspector is lagging behind lagged=3"))
                .count()
            {
                2 => {}
                3 => {}
                n => return Err(format!("Expected exactly two lagged=3 logs, but found {n}")),
            };
            Ok(())
        });
    }

    #[tokio::test]
    async fn inspect_lines() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(read_half, FromStreamOptions::default());

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
                mock.visit(line);
                Next::Continue
            },
            LineParsingOptions {
                max_line_length: 16.kilobytes(),
            },
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
        let os = BroadcastOutputStream::from_stream(
            read_half,
            FromStreamOptions {
                chunk_size: 32,
                ..Default::default()
            },
        );

        let seen: Vec<String> = Vec::new();
        let collector = os.collect_lines_async(
            seen,
            move |line, seen: &mut Vec<String>| {
                Box::pin(async move {
                    if line == "break" {
                        seen.push(line);
                        Next::Break
                    } else {
                        seen.push(line);
                        Next::Continue
                    }
                })
            },
            LineParsingOptions {
                max_line_length: 16.kilobytes(),
            },
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

        assert_that(seen).contains_exactly(&["start", "break"]);
    }

    #[tokio::test]
    async fn collect_lines_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(
            read_half,
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
            LineParsingOptions {
                max_line_length: 16.kilobytes(),
            },
        );

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
        let os = BroadcastOutputStream::from_stream(
            read_half,
            FromStreamOptions {
                chunk_size: 32,
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
            LineParsingOptions {
                max_line_length: 16.kilobytes(),
            },
        );

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
        let os = BroadcastOutputStream::from_stream(
            read_half,
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

    #[tokio::test]
    #[traced_test]
    async fn collect_chunks_into_write_in_parallel() {
        // Big enough to hold any individual test write that we perform.
        let (read_half, write_half) = tokio::io::duplex(64);

        let os = BroadcastOutputStream::from_stream(
            read_half,
            FromStreamOptions {
                // Big enough to hold any individual test write that we perform.
                // Actual chunks will be smaller.
                chunk_size: 64,
                channel_capacity: 2,
                ..Default::default()
            },
        );

        let file1 = tokio::fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(
                std::env::temp_dir()
                    .join("tokio_process_tools_test_broadcast_stream_collect_chunks_into_write_in_parallel_1.txt"),
            )
            .await
            .unwrap();
        let file2 = tokio::fs::File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(
                std::env::temp_dir()
                    .join("tokio_process_tools_test_broadcast_stream_collect_chunks_into_write_in_parallel_2.txt"),
            )
            .await
            .unwrap();

        let collector1 = os.collect_chunks_into_write(file1);
        let collector2 = os.collect_chunks_into_write_mapped(file2, |chunk| {
            format!("ok-{}", String::from_utf8_lossy(chunk.as_ref()))
        });

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file1 = collector1.cancel().await.unwrap();
        temp_file1.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = String::new();
        temp_file1.read_to_string(&mut contents).await.unwrap();
        assert_that(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");

        let mut temp_file2 = collector2.cancel().await.unwrap();
        temp_file2.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = String::new();
        temp_file2.read_to_string(&mut contents).await.unwrap();
        assert_that(contents)
            .is_equal_to("ok-Cargo.lock\nok-Cargo.toml\nok-README.md\nok-src\nok-target\n");
    }
}
