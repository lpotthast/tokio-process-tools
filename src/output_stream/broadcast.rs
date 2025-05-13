use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::impls::{
    impl_collect_chunks, impl_collect_chunks_async, impl_collect_lines, impl_collect_lines_async,
    impl_inspect_chunks, impl_inspect_lines, impl_inspect_lines_async,
};
use crate::output_stream::{LineReader, Next};
use crate::output_stream::{OutputStream, StreamType};
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the broadcast variant, allowing for multiple simultaneous consumers with the downside
/// of inducing memory allocations not required when only one consumer is listening.
/// For that case, prefer using the `output_stream::single_subscriber::SingleOutputSteam`.
pub struct BroadcastOutputStream {
    ty: StreamType,

    /// The task that captured the `tokio::sync::broadcast::Sender` part and is asynchronously
    /// awaiting new output, sending it to all registered receivers in chunks. TODO: of up to size ???
    stream_reader: JoinHandle<()>,

    sender: tokio::sync::broadcast::Sender<Option<Vec<u8>>>,
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
            .field("ty", &self.ty)
            .field("output_collector", &"non-debug < JoinHandle<()> >")
            .field(
                "sender",
                &"non-debug < tokio::sync::broadcast::Sender<Option<String>> >",
            )
            .finish()
    }
}

fn read_chunked<B: AsyncRead + Unpin + Send + 'static, const BUFFER_SIZE: usize>(
    mut reader: BufReader<B>,
    sender: tokio::sync::broadcast::Sender<Option<Vec<u8>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut buf = [0; BUFFER_SIZE]; // 16kb
            match reader.read(&mut buf).await {
                Ok(bytes_read) => {
                    let is_eof = bytes_read == 0;

                    let to_send = match is_eof {
                        true => None,
                        false => Some(Vec::from(&buf[..bytes_read])),
                    };

                    match sender.send(to_send) {
                        Ok(_received_by) => {}
                        Err(err) => {
                            // All receivers already dropped.
                            // We intentionally ignore these errors.
                            // If they occur, the user wasn't interested in seeing this chunk.
                            // We won't store it (history) to later feed it back to a new subscriber.
                            tracing::debug!(
                                error = %err,
                                "No active receivers for the output chunk, dropping it"
                            );
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

impl BroadcastOutputStream {
    pub(crate) fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        ty: StreamType,
        channel_capacity: usize,
    ) -> BroadcastOutputStream {
        const INPUT_BUFFER_SIZE: usize = 32 * 1024; // 32kb
        const CHUNK_BUFFER_SIZE: usize = 16 * 1024; // 16kb

        let (tx_stdout, _rx_stdout) =
            tokio::sync::broadcast::channel::<Option<Vec<u8>>>(channel_capacity);

        let buf_reader = BufReader::with_capacity(INPUT_BUFFER_SIZE, stream);
        let output_collector = read_chunked::<_, CHUNK_BUFFER_SIZE>(buf_reader, tx_stdout.clone());

        BroadcastOutputStream {
            ty,
            stream_reader: output_collector,
            sender: tx_stdout,
        }
    }

    pub fn ty(&self) -> &StreamType {
        &self.ty
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Option<Vec<u8>>> {
        self.sender.subscribe()
    }
}

// Expected types:
// receiver: tokio::sync::broadcast::Receiver<Option<Vec<u8>>>,
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
    pub fn inspect_chunks(&self, mut f: impl FnMut(Vec<u8>) -> Next + Send + 'static) -> Inspector {
        let mut receiver = self.subscribe();
        impl_inspect_chunks!(receiver, f, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(&self, mut f: impl FnMut(String) -> Next + Send + 'static) -> Inspector {
        let mut receiver = self.subscribe();
        impl_inspect_lines!(receiver, f, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<Fut>(
        &self,
        mut f: impl FnMut(String) -> Fut + Send + 'static,
    ) -> Inspector
    where
        Fut: Future<Output = Next> + Send,
    {
        let mut receiver = self.subscribe();
        impl_inspect_lines_async!(receiver, f, handle_subscription)
    }
}

// Impls for collecting the output of the stream.
impl BroadcastOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        mut collect: impl FnMut(Vec<u8>, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_chunks!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        mut collect: impl FnMut(String, &mut S) -> Next + Send + 'static,
    ) -> Collector<S> {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_lines!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(Vec<u8>, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_chunks_async!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));
        let mut receiver = self.subscribe();
        impl_collect_lines_async!(receiver, collect, sink, handle_subscription)
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_vec(&self) -> Collector<Vec<Vec<u8>>> {
        self.collect_chunks(Vec::new(), |chunk, vec| vec.push(chunk))
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_into_vec(&self) -> Collector<Vec<String>> {
        self.collect_lines(Vec::new(), |line, vec| {
            vec.push(line);
            Next::Continue
        })
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &self,
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
    pub fn collect_lines_into_write<W: Sink + AsyncWriteExt + Unpin>(
        &self,
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
        &self,
        write: W,
        mapper: impl Fn(Vec<u8>) -> B + Send + Sync + Copy + 'static,
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
impl BroadcastOutputStream {
    /// This function is cancel safe.
    pub async fn wait_for_line(&self, predicate: impl Fn(String) -> bool + Send + Sync + 'static) {
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
        &self,
        predicate: impl Fn(String) -> bool + Send + Sync + 'static,
        timeout: Duration,
    ) -> Result<(), Elapsed> {
        tokio::time::timeout(timeout, self.wait_for_line(predicate)).await
    }
}

#[cfg(test)]
mod tests {
    use crate::StreamType;
    use crate::output_stream::Next;
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::output_stream::tests::write_test_data;
    use assertr::assert_that;
    use assertr::prelude::PartialEqAssertions;
    use mockall::*;
    use std::io::SeekFrom;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[tokio::test]
    async fn inspect_lines() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 32);

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

    // TODO: Test that inspector/collector receivers terminate when data-sender is dropped!

    #[tokio::test]
    #[traced_test]
    async fn handles_backpressure_by_dropping_newer_chunks_after_channel_buffer_filled_up() {
        let (read_half, mut write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 2);

        let consumer = os.inspect_lines_async(async |_line| {
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
    #[traced_test]
    async fn collect_chunks_into_write_in_parallel() {
        let (read_half, write_half) = tokio::io::duplex(64);
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 2);

        let file1 = tokio::fs::File::options()
            .create(true)
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
            format!("ok-{}", String::from_utf8_lossy(&chunk))
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
