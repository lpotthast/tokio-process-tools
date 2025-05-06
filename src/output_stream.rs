use crate::CollectorError;
use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::RwLock;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::error::Elapsed;

/// Represents the type of output stream (stdout or stderr)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    StdOut,
    StdErr,
}

/// Control flag to indicate whether processing should continue or break.
///
/// Returning `Break` from an `Inspector` will let that inspector stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Continue,
    Break,
}

/// Errors that can occur when waiting for process output.
#[derive(Debug, Error)]
pub enum WaitError {
    #[error("A general io error occurred")]
    IoError(#[from] io::Error),

    #[error("Collector failed")]
    CollectorFailed(#[from] CollectorError), // TODO: refactor?
}

pub struct WaitFor {
    task: AbortHandle,
}

impl Drop for WaitFor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Represents an output stream from a process.
pub struct OutputStream {
    ty: OutputType,
    output_collector: JoinHandle<()>,
    // TODO: Can we use a reusable buffer here?
    sender: tokio::sync::broadcast::Sender<Option<Vec<u8>>>,
}

impl Drop for OutputStream {
    fn drop(&mut self) {
        self.output_collector.abort();
    }
}

impl Debug for OutputStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputStream")
            .field("ty", &self.ty)
            .field(
                "sender",
                &"non-debug < tokio::sync::broadcast::Sender<Option<String>> >",
            )
            .finish()
    }
}

/// Processes a byte chunk, handling line breaks and applying a callback function to complete lines.
///
/// This function takes a byte slice and appends it to the current line buffer. When it encounters
/// newline characters, it calls the callback function with the completed line and continues
/// processing the remainder of the chunk.
///
/// # Parameters
/// * `chunk` - Byte slice to process
/// * `line_buffer` - Current accumulated line content
/// * `process_line` - Callback function to apply to completed lines
///
/// # Returns
/// The updated line buffer with any remaining content after the last newline
fn process_lines_in_chunk(
    chunk: &[u8],
    line_buffer: &mut String,
    process_line: &mut impl FnMut(String) -> Next,
) -> Next {
    let mut current_chunk = chunk;
    let mut next = Next::Continue;

    while !current_chunk.is_empty() {
        match current_chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - append remaining chunk and exit loop.
                line_buffer.push_str(String::from_utf8_lossy(current_chunk).as_ref());
                break;
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = current_chunk.split_at(pos);
                line_buffer.push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                next = process_line(line_buffer.clone());

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                line_buffer.clear();
                line_buffer.shrink_to(2048);
                current_chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                if next == Next::Break {
                    break;
                }
            }
        }
    }

    next
}

async fn process_lines_in_chunk_async<
    Fut: Future<Output = Result<Next, Box<dyn Error + Send + Sync>>> + Send,
>(
    chunk: &[u8],
    line_buffer: &mut String,
    process_line: &mut impl FnMut(String) -> Fut,
) -> Next {
    let mut current_chunk = chunk;
    let mut next = Next::Continue;

    while !current_chunk.is_empty() {
        match current_chunk.iter().position(|b| *b == b'\n') {
            None => {
                // No more line breaks - append remaining chunk and exit loop.
                line_buffer.push_str(String::from_utf8_lossy(current_chunk).as_ref());
                break;
            }
            Some(pos) => {
                // Found a line break at `pos` - process the line and continue.
                let (until_line_break, rest) = current_chunk.split_at(pos);
                line_buffer.push_str(String::from_utf8_lossy(until_line_break).as_ref());

                // Process the completed line.
                let fut = process_line(line_buffer.clone());
                let result = fut.await;
                match result {
                    Ok(ok) => next = ok,
                    Err(err) => {
                        // TODO: Should this break the inspector?
                        tracing::warn!(?err, "Inspection failed")
                    }
                }

                // Reset line buffer and continue with rest of chunk (skip the newline).
                // Ensure we don't go out of bounds when skipping the newline character.
                line_buffer.clear();
                line_buffer.shrink_to(2048);
                current_chunk = if rest.len() > 1 { &rest[1..] } else { &[] };

                if next == Next::Break {
                    break;
                }
            }
        }
    }

    next
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
                        tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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

impl OutputStream {
    pub fn ty(&self) -> OutputType {
        self.ty
    }

    // TODO: Do we really want this?
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Option<Vec<u8>>> {
        self.sender.subscribe()
    }

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
                                tracing::info!("Received chunk: {}", String::from_utf8_lossy(&chunk));
                                let next = process_lines_in_chunk_async(&chunk, &mut line_buffer, &mut f).await;
                                if next == Next::Break {
                                    break;
                                }
                            }
                            Ok(None) => {
                                if !line_buffer.is_empty() {
                                    f(line_buffer);
                                    line_buffer = String::new();
                                }
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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
                            Ok(None) => {
                                // EOF reached.
                                break;
                            }
                            Err(RecvError::Closed) => {
                                // All senders have been dropped.
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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
                                tracing::warn!("Inspector was kept longer than the OutputStream from which it was created. Remember to manually abort or await inspectors when no longer needed.");
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

pub(crate) fn extract_output_streams(
    mut child: Child,
    stdout_channel_capacity: usize,
    stderr_channel_capacity: usize,
) -> (Child, OutputStream, OutputStream) {
    let stdout = child
        .stdout
        .take()
        .expect("Child process stdout wasn't captured");
    let stderr = child
        .stderr
        .take()
        .expect("Child process stderr wasn't captured");

    fn follow_lines_with_background_task<B: AsyncRead + Unpin + Send + 'static>(
        mut reader: BufReader<B>,
        sender: tokio::sync::broadcast::Sender<Option<Vec<u8>>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let mut buf = [0; 16 * 1024]; // 16kb, TODO: make configurable?
                match reader.read(&mut buf).await {
                    Ok(bytes_read) => {
                        let is_eof = bytes_read == 0;
                        match is_eof {
                            true => {
                                sender.send(None).unwrap();
                                break;
                            }
                            false => {
                                match sender.send(Some(Vec::from(&buf[..bytes_read]))) {
                                    Ok(_received_by) => {}
                                    Err(err) => {
                                        // All receivers already dropped.
                                        // We intentionally ignore these errors.
                                        // If they occur, the user wasn't interested in seeing this message.
                                        // We won't store it (history) to later feed it back to a new subscriber.
                                        tracing::debug!(
                                            error = %err,
                                            "No active receivers for output line, dropping it"
                                        );
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

    // Intentionally dropping the initial subscribers.
    // This will potentially lead to the (ignored) errors in follow_lines_with_background_task.
    let (tx_stdout, _rx_stdout) =
        tokio::sync::broadcast::channel::<Option<Vec<u8>>>(stdout_channel_capacity);
    let (tx_stderr, _rx_stderr) =
        tokio::sync::broadcast::channel::<Option<Vec<u8>>>(stderr_channel_capacity);

    let stdout_buf_reader = BufReader::with_capacity(128 * 1024, stdout); // 128KB input buffer
    let stderr_buf_reader = BufReader::with_capacity(128 * 1024, stderr); // 128KB input buffer

    let stdout_jh = follow_lines_with_background_task(stdout_buf_reader, tx_stdout.clone());
    let stderr_jh = follow_lines_with_background_task(stderr_buf_reader, tx_stderr.clone());

    (
        child,
        OutputStream {
            ty: OutputType::StdOut,
            output_collector: stdout_jh,
            sender: tx_stdout,
        },
        OutputStream {
            ty: OutputType::StdErr,
            output_collector: stderr_jh,
            sender: tx_stderr,
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::output_stream::{Next, process_lines_in_chunk};
    use crate::{OutputStream, OutputType, ProcessHandle};
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::time::sleep;
    use tracing_test::traced_test;

    #[test]
    fn test_process_lines_in_chunk() {
        // Helper function to reduce duplication in test cases
        fn run_test_case(
            test_name: &str,
            chunk: &[u8],
            initial_line_buffer: &str,
            expected_result: &str,
            expected_lines: &[&str],
        ) {
            let mut line_buffer = String::from(initial_line_buffer);
            let mut collected_lines: Vec<String> = Vec::new();

            let _next = process_lines_in_chunk(chunk, &mut line_buffer, &mut |line: String| {
                collected_lines.push(line);
                Next::Continue
            });

            assert_that(line_buffer)
                .with_detail_message(format!("Test case: {test_name}"))
                .is_equal_to(expected_result);

            let expected_lines: Vec<String> =
                expected_lines.iter().map(|s| s.to_string()).collect();

            assert_that(collected_lines)
                .with_detail_message(format!("Test case: {test_name}"))
                .is_equal_to(expected_lines);
        }

        // Test case 1: Empty chunk
        run_test_case(
            "Empty chunk",
            b"",
            "existing line: ",
            "existing line: ",
            &[],
        );

        // Test case 2: Chunk with no newlines
        run_test_case(
            "Chunk with no newlines",
            b"no newlines here",
            "existing line: ",
            "existing line: no newlines here",
            &[],
        );

        // Test case 3: Single complete line
        run_test_case("Single complete line", b"one line\n", "", "", &["one line"]);

        // Test case 4: Multiple complete lines
        run_test_case(
            "Multiple complete lines",
            b"first line\nsecond line\nthird line\n",
            "",
            "",
            &["first line", "second line", "third line"],
        );

        // Test case 5: Partial line at the end
        run_test_case(
            "Partial line at the end",
            b"complete line\npartial",
            "",
            "partial",
            &["complete line"],
        );

        // Test case 6: Initial line with multiple newlines
        run_test_case(
            "Initial line with multiple newlines",
            b"continuation\nmore lines\n",
            "initial part of line: ",
            "",
            &["initial part of line: continuation", "more lines"],
        );

        // Test case 7: Non-UTF8 data
        {
            // This test case needs special handling due to its assertions
            let chunk = b"valid utf8\xF0\x28\x8C\xBC invalid utf8\n";
            let mut line_buffer = String::from("");
            let mut collected_lines = Vec::new();

            let _next = process_lines_in_chunk(chunk, &mut line_buffer, &mut |line: String| {
                collected_lines.push(line);
                Next::Continue
            });

            assert_that(line_buffer).is_equal_to("");
            assert_that(collected_lines[0].as_str()).contains("valid utf8");
            assert_that(collected_lines[0].as_str()).contains("invalid utf8");
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn backpressure() {
        let (sender, _) = tokio::sync::broadcast::channel::<Option<Vec<u8>>>(2);

        let sender_clone = sender.clone();
        let task = tokio::spawn(async move {
            let mut count = 1;
            loop {
                tracing::info!("Sending chunk {}", count);
                sender_clone
                    .send(Some(Vec::from(format!("{count}\n"))))
                    .unwrap();
                count += 1;
                sleep(Duration::from_millis(25)).await;
            }
        });

        let os = OutputStream {
            ty: OutputType::StdOut,
            output_collector: task,
            sender,
        };

        let consumer = os.inspect_lines_async(async |line| {
            // Mimic a slow consumer.
            tracing::info!("Processing line: {line}");
            sleep(Duration::from_millis(100)).await;
            drop(line);
            Ok(Next::Continue)
        });

        sleep(Duration::from_millis(500)).await;
        consumer.abort().await.unwrap();
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
    #[traced_test]
    async fn collect_into_write() {
        let cmd = tokio::process::Command::new("ls");

        let mut handle = ProcessHandle::spawn("ls", cmd).unwrap();

        let file1 = tokio::fs::File::create(
            std::env::temp_dir()
                .join("tokio_process_tools_test_write_with_predefined_collector_1.txt"),
        )
        .await
        .unwrap();
        let file2 = tokio::fs::File::create(
            std::env::temp_dir()
                .join("tokio_process_tools_test_write_with_predefined_collector_2.txt"),
        )
        .await
        .unwrap();

        let collector1 = handle.stdout().collect_chunks_into_write(file1);

        let collector2 = handle
            .stdout()
            .collect_chunks_into_write_mapped(file2, |chunk| {
                format!("ok-{}", String::from_utf8_lossy(&chunk))
            });

        let _exit_status = handle.wait().await.unwrap();
        let _file = collector1.abort().await.unwrap();
        let _file = collector2.abort().await.unwrap();
    }
}
