use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::{process_lines_in_chunk, process_lines_in_chunk_async, Next};
use crate::{OutputStream, StreamType};
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;
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

/// Handles subscription output with a custom handler
async fn handle_subscription<F>(
    mut chunk_receiver: tokio::sync::broadcast::Receiver<Option<Vec<u8>>>,
    mut term_sig_rx: tokio::sync::oneshot::Receiver<()>,
    mut handler: F,
) where
    F: FnMut(Option<Vec<u8>>) -> Next,
{
    loop {
        tokio::select! {
            out = chunk_receiver.recv() => {
                match out {
                    Ok(maybe_chunk) => {
                        match handler(maybe_chunk) {
                            Next::Continue => continue,
                            Next::Break => break,
                        }
                    }
                    Err(RecvError::Closed) => {
                        // All senders have been dropped.
                        break;
                    }
                    Err(RecvError::Lagged(lagged)) => {
                        tracing::warn!(lagged, "Inspector is lagging behind");
                    }
                }
            }
            _msg = &mut term_sig_rx => {
                break;
            }
        }
    }
}

// Impls for inspecting the output of the stream.
impl BroadcastOutputStream {
    /// NOTE: This is a convenience function.
    /// Only use it if you trust the output of the child process.
    /// This will happily collect gigabytes of output in an allocated `String` if the captured
    /// process never writes a line break!
    ///
    /// For more control, use `inspect_chunks` instead.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(&self, f: impl Fn(Vec<u8>) -> Next + Send + 'static) -> Inspector {
        let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        let receiver = self.subscribe();
        let task = tokio::spawn(async move {
            handle_subscription(receiver, term_sig_rx, move |maybe_chunk| {
                match maybe_chunk {
                    Some(chunk) => f(chunk),
                    None => {
                        // EOF reached.
                        Next::Break
                    }
                }
            })
            .await;
        });
        Inspector {
            task: Some(task),
            task_termination_sender: Some(term_sig_tx),
        }
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines(&self, mut f: impl FnMut(String) -> Next + Send + 'static) -> Inspector {
        let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        let receiver = self.subscribe();
        let task = tokio::spawn(async move {
            let mut line_buffer = String::new();
            handle_subscription(
                receiver,
                term_sig_rx,
                move |maybe_chunk| match maybe_chunk {
                    Some(chunk) => process_lines_in_chunk(&chunk, &mut line_buffer, &mut f),
                    None => {
                        if !line_buffer.is_empty() {
                            f(line_buffer.clone());
                        }
                        Next::Break
                    }
                },
            )
            .await;
        });
        Inspector {
            task: Some(task),
            task_termination_sender: Some(term_sig_tx),
        }
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_lines_async<F, Fut>(&self, mut f: F) -> Inspector
    where
        F: Fn(String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Next, Box<dyn Error + Send + Sync>>> + Send + Sync,
    {
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(chunk)) => {
                                let next = process_lines_in_chunk_async(&chunk, &mut line_buffer, &mut f).await;
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Ok(None) => {
                                if !line_buffer.is_empty() {
                                    f(line_buffer);
                                }
                                break;
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                break;
                            }
                            Err(RecvError::Lagged(lagged)) => {
                                tracing::warn!(lagged, "Inspector is lagging behind");
                            }
                        }
                    }
                    _msg = &mut term_sig_rx => {
                        break;
                    }
                }
            }
        });

        Inspector {
            task: Some(capture),
            task_termination_sender: Some(term_sig_tx),
        }
    }
}

// Impls for collecting the output of the stream.
impl BroadcastOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &self,
        into: S,
        collect: impl Fn(Vec<u8>, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let target = Arc::new(RwLock::new(into));

        let (t_send, mut t_recv) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(chunk)) => {
                                let mut write_guard = target.write().await;
                                collect(chunk, &mut (*write_guard));
                            }
                            Ok(None) => {
                                // EOF reached.
                                break;
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                break;
                            }
                            Err(RecvError::Lagged(lagged)) => {
                                tracing::warn!(lagged, "Inspector is lagging behind");
                            }
                        }
                    }
                    _msg = &mut t_recv => {
                        tracing::info!("Terminating collector!");
                        break;
                    }
                }
            }
            Arc::try_unwrap(target).expect("single owner").into_inner()
        });

        Collector {
            task: Some(capture),
            task_termination_sender: Some(t_send),
        }
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines<S: Sink>(
        &self,
        into: S,
        collect: impl Fn(String, &mut S) -> Next + Send + 'static,
    ) -> Collector<S> {
        let target = Arc::new(RwLock::new(into));

        let (t_send, mut t_recv) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(chunk)) => {
                                let mut write_guard = target.write().await;
                                let next = process_lines_in_chunk(&chunk, &mut line_buffer, &mut |line| {
                                    collect(line, &mut (*write_guard))
                                });
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Ok(None) => {
                                // EOF reached.
                                break;
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                break;
                            }
                            Err(RecvError::Lagged(lagged)) => {
                                tracing::warn!(lagged, "Inspector is lagging behind");
                            }
                        }
                    }
                    _msg = &mut t_recv => {
                        tracing::info!("Terminating collector!");
                        break;
                    }
                }
            }
            Arc::try_unwrap(target).expect("single owner").into_inner()
        });

        Collector {
            task: Some(capture),
            task_termination_sender: Some(t_send),
        }
    }

    // NOTE: We only use Pin<Box> here to force usage of Higher-Rank Trait Bounds (HRTBs).
    // The returned futures will most-likely capture the `&mut T`and are therefore poised
    // by its lifetime. Without the trait-object usage, this would not work.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(Vec<u8>, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let target = Arc::new(RwLock::new(into));

        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(chunk)) => {
                                // TODO: refactor error handling?
                                let result = {
                                    let mut write_guard = target.write().await;
                                    let fut = collect(chunk, &mut *write_guard);
                                    fut.await
                                };
                                match result {
                                    Ok(_next) => { /* do nothing */ }
                                    Err(err) => {
                                        tracing::warn!(?err, "Collecting failed")
                                    }
                                }
                            }
                            Ok(None) | Err(RecvError::Closed) => {
                                // EOF reached or all senders dropped.
                                break;
                            }
                            Err(RecvError::Lagged(lagged)) => {
                                tracing::warn!(lagged, "Inspector is lagging behind");
                            }
                        }
                    }
                    _msg = &mut term_sig_rx => {
                        break;
                    }
                }
            }
            Arc::try_unwrap(target).expect("single owner").into_inner()
        });

        Collector {
            task: Some(capture),
            task_termination_sender: Some(term_sig_tx),
        }
    }

    // NOTE: We only use Pin<Box> here to force usage of Higher-Rank Trait Bounds (HRTBs).
    // The returned futures will most-likely capture the `&mut T`and are therefore poised
    // by its lifetime. Without the trait-object usage, this would not work.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_lines_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));

        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();

        // Extract the collection function into an Arc to move ownership into the task
        let collect_fn = Arc::new(collect);

        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();

            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(chunk)) => {
                                let next = process_lines_in_chunk_async(&chunk, &mut line_buffer, &mut |line| {
                                    let collect_fn = collect_fn.clone();
                                    let sink = sink.clone();
                                    async move {
                                        let mut sink_guard = sink.write().await;
                                        let result = collect_fn(line.clone(), &mut *sink_guard).await;
                                        drop(sink_guard); // Explicitly release the lock
                                        match result {
                                            Ok(next) => Ok(next),
                                            Err(err) => {
                                                tracing::warn!(?err, "Collection failed");
                                                Ok(Next::Continue)
                                            }
                                        }
                                    }
                                }).await;
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Ok(None) => {
                                // EOF reached.
                                if !line_buffer.is_empty() {
                                    let mut write_guard = sink.write().await;
                                    let fut = collect_fn(line_buffer, &mut *write_guard);
                                    let result = fut.await;
                                    match result {
                                        Ok(_next) => { /* not relevant, we always break on EOF */ },
                                        Err(err) => {
                                            tracing::warn!(?err, "Collection failed");
                                        }
                                    }
                                }
                                break;
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                break;
                            }
                            Err(RecvError::Lagged(lagged)) => {
                                tracing::warn!(lagged, "Inspector is lagging behind");
                            }
                        }
                    }
                    _msg = &mut term_sig_rx => {
                        break;
                    }
                }
            }
            Arc::try_unwrap(sink).expect("single owner").into_inner()
        });

        Collector {
            task: Some(capture),
            task_termination_sender: Some(term_sig_tx),
        }
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
                write.write_all(&chunk).await?;
                //write.write_all("\n".as_bytes()).await?;
                Ok(Next::Continue)
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
                write.write_all(mapped).await?;
                Ok(Next::Continue)
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
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::Next;
    use crate::StreamType;
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
            Ok(Next::Continue)
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
