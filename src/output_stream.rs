use crate::collector::{AsyncCollectFn, Collector, Sink};
use crate::inspector::Inspector;
use crate::CollectorError;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::Child;
use tokio::sync::RwLock;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::error::Elapsed;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    StdOut,
    StdErr,
}

pub struct OutputStream {
    ty: OutputType,
    line_follower_task: JoinHandle<()>,
    sender: tokio::sync::broadcast::Sender<Option<String>>,
}

impl Drop for OutputStream {
    fn drop(&mut self) {
        self.line_follower_task.abort();
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

#[derive(Debug, Error)]
pub enum WaitError {
    #[error("A general io error occurred")]
    IoError(#[from] io::Error),

    #[error("Collector failed")]
    CollectorFailed(#[from] CollectorError),
}

pub struct WaitFor {
    task: AbortHandle,
}

impl Drop for WaitFor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl OutputStream {
    pub fn ty(&self) -> OutputType {
        self.ty
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Option<String>> {
        self.sender.subscribe()
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect(&self, f: impl Fn(String) + Send + 'static) -> Inspector {
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut receiver = self.subscribe();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = receiver.recv() => {
                        match out {
                            Ok(Some(line)) => {
                                f(line);
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "Inspector failed to receive output line"
                                );
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
            task: Some(task),
            task_termination_sender: Some(term_sig_tx),
        }
    }

    // NOTE: We only use Pin<Box> here to force usage of Higher-Rank Trait Bounds (HRTBs).
    // The returned futures will most-likely capture the `&mut T`and are therefore poised
    // by its lifetime. Without the trait-object usage, this would not work.
    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the inspector effectively dies immediately. You can safely do a `let _inspector = ...` binding to ignore the typical 'unused' warning."]
    pub fn inspect_async<F>(&self, f: F) -> Inspector
    where
        F: Fn(
                String,
            )
                -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send>>
            + Send
            + 'static,
    {
        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(line)) => {
                                let result = f(line).await;
                                match result {
                                    Ok(()) => { /* do nothing */ }
                                    Err(err) => {
                                        tracing::warn!(?err, "Inspection failed")
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "Inspector failed to receive output line"
                                );
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
    pub fn collect<S: Sink>(
        &self,
        into: S,
        collect: impl Fn(String, &mut S) + Send + 'static,
    ) -> Collector<S> {
        let target = Arc::new(RwLock::new(into));

        let (t_send, mut t_recv) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(line)) => {
                                let mut write_guard = target.write().await;
                                collect(line, &mut (*write_guard));
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "Collector failed to receive output line"
                                );
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
    pub fn collect_async<S, F>(&self, into: S, collect: F) -> Collector<S>
    where
        S: Sink,
        F: Fn(String, &mut S) -> AsyncCollectFn<'_> + Send + 'static,
    {
        let target = Arc::new(RwLock::new(into));

        let (term_sig_tx, mut term_sig_rx) = tokio::sync::oneshot::channel::<()>();

        let mut out_receiver = self.subscribe();
        let capture = tokio::spawn(async move {
            loop {
                tokio::select! {
                    out = out_receiver.recv() => {
                        match out {
                            Ok(Some(line)) => {
                                let result = {
                                    let mut write_guard = target.write().await;
                                    collect(line, &mut *write_guard).await
                                };
                                match result {
                                    Ok(()) => { /* do nothing */ }
                                    Err(err) => {
                                        tracing::warn!(?err, "Collecting failed")
                                    }
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    "Collector failed to receive output line"
                                );
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

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_into_vec(&self) -> Collector<Vec<String>> {
        self.collect(Vec::new(), |line, vec| vec.push(line))
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_into_write<W: Sink + AsyncWriteExt + Unpin>(&self, write: W) -> Collector<W> {
        self.collect_async(write, move |line, write| {
            Box::pin(async move {
                write.write_all(line.as_bytes()).await?;
                write.write_all("\n".as_bytes()).await?;
                Ok(())
            })
        })
    }

    #[must_use = "If not at least assigned to a variable, the return value will be dropped immediately, which in turn drops the internal tokio task, meaning that your callback is never called and the collector effectively dies immediately. You can safely do a `let _collector = ...` binding to ignore the typical 'unused' warning."]
    pub fn collect_into_write_mapped<W: Sink + AsyncWriteExt + Unpin, B: AsRef<[u8]> + Send>(
        &self,
        write: W,
        mapper: impl Fn(String) -> B + Send + Sync + Copy + 'static,
    ) -> Collector<W> {
        self.collect_async(write, move |line, write| {
            Box::pin(async move {
                let mapped = mapper(line);
                let mapped = mapped.as_ref();
                write.write_all(mapped).await?;
                Ok(())
            })
        })
    }

    /// This function is cancel safe.
    pub async fn wait_for(&self, predicate: impl Fn(String) -> bool + Send + 'static) -> WaitFor {
        let mut receiver = self.subscribe();
        let jh = tokio::spawn(async move {
            loop {
                if let Ok(Some(line)) = receiver.recv().await {
                    if predicate(line) {
                        break;
                    }
                }
            }
        });
        let result = WaitFor {
            task: jh.abort_handle(),
        };
        let _ = jh.await;
        result
    }

    /// This function is cancel safe.
    pub async fn wait_for_with_timeout(
        &self,
        predicate: impl Fn(String) -> bool + Send + 'static,
        timeout: Duration,
    ) -> Result<WaitFor, Elapsed> {
        tokio::time::timeout(timeout, self.wait_for(predicate)).await
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
        .expect("Child process stdout not captured");
    let stderr = child
        .stderr
        .take()
        .expect("Child process stderr not captured");

    fn follow_lines_with_background_task<B: AsyncBufRead + Unpin + Send + 'static>(
        mut lines: Lines<B>,
        sender: tokio::sync::broadcast::Sender<Option<String>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match lines.next_line().await {
                    Ok(maybe_line) => {
                        let is_none = maybe_line.is_none();
                        match sender.send(maybe_line) {
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
                        if is_none {
                            break;
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
        tokio::sync::broadcast::channel::<Option<String>>(stdout_channel_capacity);
    let (tx_stderr, _rx_stderr) =
        tokio::sync::broadcast::channel::<Option<String>>(stderr_channel_capacity);

    let stdout_jh =
        follow_lines_with_background_task(BufReader::new(stdout).lines(), tx_stdout.clone());
    let stderr_jh =
        follow_lines_with_background_task(BufReader::new(stderr).lines(), tx_stderr.clone());

    (
        child,
        OutputStream {
            ty: OutputType::StdOut,
            line_follower_task: stdout_jh,
            sender: tx_stdout,
        },
        OutputStream {
            ty: OutputType::StdErr,
            line_follower_task: stderr_jh,
            sender: tx_stderr,
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::ProcessHandle;
    use std::process::Stdio;

    #[tokio::test]
    async fn collect_into_write() {
        let child = tokio::process::Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut handle = ProcessHandle::new_from_child_with_piped_io("ls", child);

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

        let collector1 = handle.stdout().collect_into_write(file1);

        let collector2 = handle
            .stdout()
            .collect_into_write_mapped(file2, |line| format!("ok-{}", line));

        let _exit_status = handle.wait().await.unwrap();
        let _file = collector1.abort().await.unwrap();
        let _file = collector2.abort().await.unwrap();
    }
}
