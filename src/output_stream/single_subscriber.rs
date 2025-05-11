use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::output_stream::{
    process_lines_in_chunk, process_lines_in_chunk_async, BackpressureControl, Next,
};
use crate::{OutputStream, StreamType};
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;

/// The output stream from a process. Either representing stdout or stderr.
///
/// This is the single-subscriber variant, allowing for just one consumer.
/// This has the upside of requiring as few memory allocations as possible.
/// If multiple concurrent inspections are required, prefer using the
/// `output_stream::broadcast::BroadcastOutputSteam`.
pub struct SingleOutputStream {
    ty: StreamType,

    /// The task that captured the `tokio::sync::mpsc::Sender` part and is asynchronously awaiting
    /// new output, sending it to our receiver in chunks. TODO: of up to size ???
    stream_reader: JoinHandle<()>,

    /// The receiver is wrapped in an `Option` so that we can take it out of it and move it
    /// into an inspector or collector task.
    receiver: Option<tokio::sync::mpsc::Receiver<Option<bytes::Bytes>>>,
}

impl OutputStream for SingleOutputStream {}

impl Drop for SingleOutputStream {
    fn drop(&mut self) {
        self.stream_reader.abort();
    }
}

impl Debug for SingleOutputStream {
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
    buffer_size: usize,
    sender: tokio::sync::mpsc::Sender<Option<bytes::Bytes>>,
    backpressure_control: BackpressureControl,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // NOTE: buf may grow when required! TODO
            let mut buf = bytes::BytesMut::with_capacity(buffer_size);
            match reader.read(&mut buf).await {
                Ok(bytes_read) => {
                    let is_eof = bytes_read == 0;
                    match is_eof {
                        true => {
                            // Blocking until the previous value was received.
                            match backpressure_control {
                                BackpressureControl::DropLatestIncomingIfBufferFull => {
                                    sender.try_send(None).unwrap();
                                }
                                BackpressureControl::BlockUntilBufferHasSpace => {
                                    sender.send(None).await.unwrap();
                                }
                            }
                            break;
                        }
                        false => {
                            let bytes = buf.split().freeze();
                            match backpressure_control {
                                BackpressureControl::DropLatestIncomingIfBufferFull => {
                                    match sender.try_send(Some(bytes)) {
                                        Ok(()) => {}
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
                                }
                                BackpressureControl::BlockUntilBufferHasSpace => {
                                    match sender.send(Some(bytes)).await {
                                        Ok(()) => {}
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
                                }
                            }
                        }
                    }
                }
                Err(err) => panic!("Could not read from stream: {err}"),
            }
        }
    })
}

impl SingleOutputStream {
    pub(crate) fn from_stream<S: AsyncRead + Unpin + Send + 'static>(
        stream: S,
        ty: StreamType,
        channel_capacity: usize,
        backpressure_control: BackpressureControl,
    ) -> SingleOutputStream {
        const INPUT_BUFFER_SIZE: usize = 32 * 1024; // 32kb
        const CHUNK_BUFFER_SIZE: usize = 16 * 1024; // 16kb

        let (tx_stdout, rx_stdout) =
            tokio::sync::mpsc::channel::<Option<bytes::Bytes>>(channel_capacity);

        let buf_reader = BufReader::with_capacity(INPUT_BUFFER_SIZE, stream);
        let output_collector = read_chunked(
            buf_reader,
            CHUNK_BUFFER_SIZE,
            tx_stdout,
            backpressure_control,
        );

        SingleOutputStream {
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

/// Handles subscription output with a custom handler
async fn handle_subscription<F>(
    mut chunk_receiver: tokio::sync::mpsc::Receiver<Option<bytes::Bytes>>,
    mut term_sig_rx: tokio::sync::oneshot::Receiver<()>,
    mut handler: F,
) where
    F: FnMut(Option<bytes::Bytes>) -> Next,
{
    loop {
        tokio::select! {
            out = chunk_receiver.recv() => {
                match out {
                    Some(maybe_chunk) => {
                        match handler(maybe_chunk) {
                            Next::Continue => continue,
                            Next::Break => break,
                        }
                    }
                    None => {
                        // All senders have been dropped.
                        // TODO: track name. Warning still correct?
                        tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                        break;
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
impl SingleOutputStream {
    /// NOTE: This is a convenience function.
    /// Only use it if you trust the output of the child process.
    /// This will happily collect gigabytes of output in an allocated `String` if the captured
    /// process never writes a line break!
    ///
    /// For more control, use `inspect_chunks` instead.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_chunks(
        &mut self,
        f: impl Fn(bytes::Bytes) -> Next + Send + 'static,
    ) -> Inspector {
        let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        let receiver = self.take_receiver();
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
    pub fn inspect_lines(
        &mut self,
        mut f: impl FnMut(String) -> Next + Send + 'static,
    ) -> Inspector {
        let (term_sig_tx, term_sig_rx) = tokio::sync::oneshot::channel::<()>();
        let receiver = self.take_receiver();
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
    pub fn inspect_lines_async<F, Fut>(&mut self, mut f: F) -> Inspector
    where
        F: Fn(String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Next, Box<dyn Error + Send + Sync>>> + Send + Sync,
    {
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.take_receiver();
        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Some(Some(chunk)) => {
                                let next = process_lines_in_chunk_async(&chunk, &mut line_buffer, &mut f).await;
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Some(None) => {
                                if !line_buffer.is_empty() {
                                    f(line_buffer);
                                    line_buffer = String::new();
                                }
                            }
                            None => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                                break;
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
impl SingleOutputStream {
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_chunks<S: Sink>(
        &mut self,
        into: S,
        collect: impl Fn(bytes::Bytes, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let target = Arc::new(RwLock::new(into));

        let (t_send, mut t_recv) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.take_receiver();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Some(Some(chunk)) => {
                                let mut write_guard = target.write().await;
                                collect(chunk, &mut (*write_guard));
                            }
                            Some(None) => {
                                // EOF reached.
                                break;
                            }
                            None => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                                break;
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
        &mut self,
        into: S,
        collect: impl Fn(String, &mut S) -> Next + Send + 'static,
    ) -> Collector<S> {
        let target = Arc::new(RwLock::new(into));

        let (t_send, mut t_recv) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.take_receiver();
        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Some(Some(chunk)) => {
                                let mut write_guard = target.write().await;
                                let next = process_lines_in_chunk(&chunk, &mut line_buffer, &mut |line| {
                                    collect(line, &mut (*write_guard))
                                });
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Some(None) => {
                                // EOF reached.
                                break;
                            }
                            None => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                                break;
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
    pub fn collect_chunks_async<S, F>(&mut self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(bytes::Bytes, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let target = Arc::new(RwLock::new(into));

        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.take_receiver();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Some(Some(chunk)) => {
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
                            Some(None) => {
                                // EOF reached.
                                break;
                            }
                            None => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                                break;
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
    pub fn collect_lines_async<S, F>(&mut self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + Sync + 'static,
    {
        let sink = Arc::new(RwLock::new(into));

        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.take_receiver();

        // Extract the collection function into an Arc to move ownership into the task
        let collect_fn = Arc::new(collect);

        let capture = tokio::spawn(async move {
            let mut line_buffer = String::new();

            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Some(Some(chunk)) => {
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
                            Some(None) => {
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
                            None => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
                                break;
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
        &mut self,
        write: W,
        mapper: impl Fn(bytes::Bytes) -> B + Send + Sync + Copy + 'static,
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
impl SingleOutputStream {
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
    use crate::output_stream::broadcast::BroadcastOutputStream;
    use crate::output_stream::single_subscriber::SingleOutputStream;
    use crate::output_stream::tests::write_test_data;
    use crate::output_stream::{BackpressureControl, Next};
    use crate::StreamType;
    use assertr::assert_that;
    use assertr::prelude::PartialEqAssertions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[tokio::test]
    #[traced_test]
    async fn backpressure() {
        let (read_half, mut write_half) = tokio::io::duplex(64); // Buffer size of 64 bytes
        let mut os = SingleOutputStream::from_stream(
            read_half,
            StreamType::StdOut,
            32,
            BackpressureControl::DropLatestIncomingIfBufferFull,
        );
        let task = tokio::spawn(async move {
            let mut count = 1;
            loop {
                tracing::info!("Sending chunk {}", count);
                write_half
                    .write_all(format!("{count}\n").as_bytes())
                    .await
                    .unwrap();
                count += 1;
                sleep(Duration::from_millis(25)).await;
            }
        });

        let consumer = os.inspect_lines_async(async |line| {
            // Mimic a slow consumer.
            tracing::info!("Processing line: {line}");
            sleep(Duration::from_millis(100)).await;
            drop(line);
            Ok(Next::Continue)
        });

        sleep(Duration::from_millis(500)).await;
        consumer.cancel().await.unwrap();
        task.abort();
        drop(os);

        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|line| line.contains("Inspector is lagging behind lagged=1"))
                .count()
            {
                1 => {}
                n => return Err(format!("Expected one matching log, but found {}", n)),
            };
            match lines
                .iter()
                .filter(|line| line.contains("Inspector is lagging behind lagged=3"))
                .count()
            {
                n @ (0 | 1 | 2) => return Err(format!("Expected 3 or more logs, but found {}", n)),
                _ => {}
            };
            Ok(())
        });
    }

    #[tokio::test]
    async fn collect_lines_to_file() {
        let (read_half, write_half) = tokio::io::duplex(64); // Buffer size of 64 bytes
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 32);

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
        let (read_half, write_half) = tokio::io::duplex(64); // Buffer size of 64 bytes
        let os = BroadcastOutputStream::from_stream(read_half, StreamType::StdOut, 32);

        let temp_file = tempfile::tempfile().unwrap();
        let collector = os.collect_lines_async(temp_file, |line, temp_file| {
            Box::pin(async move {
                writeln!(temp_file, "{}", line).unwrap();
                Ok(Next::Continue)
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
        let (read_half, write_half) = tokio::io::duplex(64); // Buffer size of 64 bytes
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
