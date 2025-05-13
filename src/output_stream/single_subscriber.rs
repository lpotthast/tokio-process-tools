use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::impls::{
    impl_collect_chunks, impl_collect_chunks_async, impl_collect_lines, impl_collect_lines_async,
    impl_inspect_chunks, impl_inspect_lines, impl_inspect_lines_async,
};
use crate::output_stream::{BackpressureControl, LineReader, Next, OutputStream, StreamType};
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one consumer.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::broadcast::BroadcastOutputSteam`.
pub struct SingleSubscriberOutputStream {
    ty: StreamType,

    /// The task that captured the `tokio::sync::mpsc::Sender` part and is asynchronously awaiting
    /// new output, sending it to our receiver in chunks. TODO: of up to size ???
    stream_reader: JoinHandle<()>,

    /// The receiver is wrapped in an `Option` so that we can take it out of it and move it
    /// into an inspector or collector task.
    receiver: Option<tokio::sync::mpsc::Receiver<Option<bytes::Bytes>>>,
}

impl OutputStream for SingleSubscriberOutputStream {}

impl Drop for SingleSubscriberOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
    }
}

impl Debug for SingleSubscriberOutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleOutputStream")
            .field("ty", &self.ty)
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field(
                "receiver",
                &"non-debug < tokio::sync::mpsc::Receiver<Option<bytes::Bytes>> >",
            )
            .finish()
    }
}

fn read_chunked<B: AsyncRead + Unpin + Send + 'static>(
    mut reader: BufReader<B>,
    chunk_size: usize,
    sender: tokio::sync::mpsc::Sender<Option<bytes::Bytes>>,
    backpressure_control: BackpressureControl,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // NOTE: buf may grow when required! TODO
        let mut buf = bytes::BytesMut::with_capacity(chunk_size);
        let mut lagged: usize = 0;
        loop {
            match reader.read_buf(&mut buf).await {
                Ok(bytes_read) => {
                    let is_eof = bytes_read == 0;

                    let to_send = match is_eof {
                        true => None,
                        false => Some(buf.split().freeze()),
                    };

                    match backpressure_control {
                        BackpressureControl::DropLatestIncomingIfBufferFull => {
                            match sender.try_send(to_send) {
                                Ok(()) => {
                                    if lagged > 0 {
                                        tracing::debug!(lagged, "Stream reader is lagging behind");
                                        lagged = 0;
                                    }
                                }
                                Err(err) => {
                                    match err {
                                        TrySendError::Full(_data) => {
                                            lagged += 1;
                                        }
                                        TrySendError::Closed(_data) => {
                                            // All receivers already dropped.
                                            // We intentionally ignore this error.
                                            // If it occurs, the user just isn't interested in
                                            // newer chunks anymore.
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        BackpressureControl::BlockUntilBufferHasSpace => {
                            match sender.send(to_send).await {
                                Ok(()) => {}
                                Err(_err) => {
                                    // All receivers already dropped.
                                    // We intentionally ignore this error.
                                    // If it occurs, the user just isn't interested in
                                    // newer chunks anymore.
                                    break;
                                }
                            }
                        }
                    }

                    if is_eof {
                        break;
                    }
                }
                Err(err) => panic!("Could not read from stream: {err}"),
            }
        }
    })
}

pub struct FromStreamOptions {
    /// The size of the buffer used when reading from the stream in bytes.
    ///
    /// default: 32 * 1024 // 32 kb
    pub read_buffer_size: usize,

    /// The size of an individual chunk read from the read buffer in bytes.
    ///
    /// default: 16 * 1024 // 16 kb
    pub chunk_size: usize,

    /// The number of chunks held by the underlying async channel.
    ///
    /// When the subscriber (if present) is not fast enough to consume chunks equally fast or faster
    /// than them getting read, this acts as a buffer to hold not-yet processed messages.
    /// The bigger, the better, in terms of system resilience to write-spikes.
    /// Multiply with `chunk_size` to obtain the amount of system resources this will consume at
    /// max.
    pub channel_capacity: usize,
}

impl Default for FromStreamOptions {
    fn default() -> Self {
        Self {
            read_buffer_size: 32 * 1024, // 32 kb
            chunk_size: 16 * 1024,       // 16 kb
            channel_capacity: 128,       // => 16 kb * 128 = 2 mb (max memory usage consumption)
        }
    }
}

impl SingleSubscriberOutputStream {
    pub fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        ty: StreamType,
        backpressure_control: BackpressureControl,
        options: FromStreamOptions,
    ) -> SingleSubscriberOutputStream {
        let (tx_stdout, rx_stdout) =
            tokio::sync::mpsc::channel::<Option<bytes::Bytes>>(options.channel_capacity);

        let buf_reader = BufReader::with_capacity(options.read_buffer_size, stream);
        let output_collector = read_chunked(
            buf_reader,
            options.chunk_size,
            tx_stdout,
            backpressure_control,
        );

        SingleSubscriberOutputStream {
            ty,
            stream_reader: output_collector,
            receiver: Some(rx_stdout),
        }
    }

    pub fn ty(&self) -> &StreamType {
        &self.ty
    }

    fn take_receiver(&mut self) -> tokio::sync::mpsc::Receiver<Option<bytes::Bytes>> {
        self.receiver.take().unwrap()
    }
}

// Expected types:
// receiver: tokio::sync::mpsc::Receiver<Option<bytes::Bytes>>
// term_rx: tokio::sync::oneshot::Receiver<()>
macro_rules! handle_subscription {
    ($receiver:expr, $term_rx:expr, |$chunk:ident| $body:block) => {
        loop {
            tokio::select! {
                out = $receiver.recv() => {
                    match out {
                        Some(maybe_chunk) => {
                            let $chunk = maybe_chunk;
                            $body
                        }
                        None => {
                            // All senders have been dropped.
                            break;
                        }
                    }
                }
                _msg = &mut $term_rx => break,
            }
        }
    };
}

// Impls for inspecting the output of the stream.
impl SingleSubscriberOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(
        &mut self,
        f: impl Fn(bytes::Bytes) -> Next + Send + 'static,
    ) -> Inspector {
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
        collect: impl Fn(bytes::Bytes, &mut S) + Send + 'static,
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
        F: Fn(bytes::Bytes, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
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
    pub fn collect_chunks_into_vec(&mut self) -> Collector<Vec<bytes::Bytes>> {
        self.collect_chunks(Vec::new(), |chunk, vec| vec.push(chunk))
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
                if let Err(err) = write.write_all(&chunk).await {
                    tracing::warn!("Could not write chunk to write sink: {err:#?}");
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
        mapper: impl Fn(bytes::Bytes) -> B + Send + Sync + Copy + 'static,
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
}

// Impls for waiting for a specific line of output.
impl SingleSubscriberOutputStream {
    /// This function is cancel safe.
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

    /// This function is cancel safe.
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
    use crate::StreamType;
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::output_stream::single_subscriber::{
        FromStreamOptions, SingleSubscriberOutputStream,
    };
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::{BackpressureControl, Next};
    use assertr::assert_that;
    use assertr::prelude::PartialEqAssertions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            StreamType::StdOut,
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
    async fn collect_lines_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let mut os = SingleSubscriberOutputStream::from_stream(
            read_half,
            StreamType::StdOut,
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
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 32);

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
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 32);

        let temp_file = tokio::fs::File::options()
            .create(true)
            .write(true)
            .read(true)
            .open(std::env::temp_dir().join(
                "tokio_process_tools_test_single_subscriber_collect_chunks_into_write_mapped.txt",
            ))
            .await
            .unwrap();

        let collector = os.collect_chunks_into_write_mapped(temp_file, |chunk| {
            String::from_utf8_lossy(&chunk).to_string()
        });

        tokio::spawn(write_test_data(write_half)).await.unwrap();

        let mut temp_file = collector.cancel().await.unwrap();
        temp_file.seek(SeekFrom::Start(0)).await.unwrap();
        let mut contents = String::new();
        temp_file.read_to_string(&mut contents).await.unwrap();

        assert_that(contents).is_equal_to("Cargo.lock\nCargo.toml\nREADME.md\nsrc\ntarget\n");
    }
}
