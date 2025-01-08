mod interrupt;

use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};
use tokio::process::Child;
use tokio::sync::oneshot::Sender;
use tokio::sync::RwLock;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::error::Elapsed;
use tracing::info;

pub struct Exec {
    child: Child,
    std_out_stream: OutputStream,
    std_err_stream: OutputStream,
}

impl Exec {
    pub fn new_from_child_with_piped_io(child: Child) -> Self {
        let (child, std_out_stream, std_err_stream) = extract_output_streams(child);
        Self {
            child,
            std_out_stream,
            std_err_stream,
        }
    }

    pub fn stdout(&self) -> &OutputStream {
        &self.std_out_stream
    }

    pub fn stderr(&self) -> &OutputStream {
        &self.std_err_stream
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    pub async fn wait_with_output(&mut self) -> io::Result<(ExitStatus, Vec<String>, Vec<String>)> {
        let out_collector = self.std_out_stream.collect_into_vec();
        let err_collector = self.std_err_stream.collect_into_vec();

        let status = self.child.wait().await?;
        let std_out = out_collector.abort().await;
        let std_err = err_collector.abort().await;

        Ok((status, std_out, std_err))
    }

    pub async fn terminate(
        &mut self,
        graceful_shutdown_timeout: Option<Duration>,
        forceful_shutdown_timeout: Option<Duration>,
    ) -> io::Result<ExitStatus> {
        // Try a graceful shutdown first.
        interrupt::send_interrupt(&self.child).await?;

        // Wait for process termination.
        match self.await_termination(graceful_shutdown_timeout).await {
            Ok(exit_status) => Ok(exit_status),
            Err(_err) => {
                // Graceful shutdown did not lead to process termination.
                // Try a forceful kill.
                self.child.kill().await?;

                // And again, wait for termination.
                self.await_termination(forceful_shutdown_timeout).await
            }
        }
    }

    async fn await_termination(&mut self, timeout: Option<Duration>) -> io::Result<ExitStatus> {
        match timeout {
            None => self.child.wait().await,
            Some(timeout) => match tokio::time::timeout(timeout, self.child.wait()).await {
                Ok(exit_status) => exit_status,
                Err(err) => Err(err.into()),
            },
        }
    }
}

impl Debug for Exec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exec")
            .field("child", &self.child)
            .field("std_out_stream", &self.std_out_stream)
            .field("std_out_stream", &self.std_err_stream)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    StdOut,
    StdErr,
}

pub struct Collector<T: Debug> {
    task: JoinHandle<T>,
    task_termination_sender: Sender<()>,
}

impl<T: Debug> Collector<T> {
    pub async fn abort(self) -> T {
        self.task_termination_sender
            .send(())
            .expect("send term signal to collector");
        self.task.await.expect("collector to terminate")
    }
}

pub struct Inspector {
    task: Option<JoinHandle<()>>,
    task_termination_sender: Option<Sender<()>>,
}

impl Inspector {
    pub async fn abort(mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            task_termination_sender
                .send(())
                .expect("send term signal to inspector");
        }
        if let Some(task) = self.task.take() {
            task.await.expect("inspector to terminate");
        }
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            task_termination_sender
                .send(())
                .expect("send term signal to inspector");
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub struct OutputStream {
    ty: OutputType,
    line_follower_task: JoinHandle<()>,
    sender: tokio::sync::broadcast::Sender<Option<String>>,
}

impl OutputStream {
    pub fn ty(&self) -> OutputType {
        self.ty
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Option<String>> {
        self.sender.subscribe()
    }

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
                            Err(_err) => {} // TODO: Handle error?
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
                            Err(_err) => {} // TODO: Handle error?
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

    pub fn collect<T: Debug + Send + Sync + 'static>(
        &self,
        into: T,
        collect: impl Fn(String, &mut T) + Send + 'static,
    ) -> Collector<T> {
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
                            Err(_err) => {} // TODO: Handle error?
                        }
                    }
                    _msg = &mut t_recv => {
                        info!("Terminating collector!");
                        break;
                    }
                }
            }
            Arc::try_unwrap(target).expect("single owner").into_inner()
        });

        Collector {
            task: capture,
            task_termination_sender: t_send,
        }
    }

    // NOTE: We only use Pin<Box> here to force usage of Higher-Rank Trait Bounds (HRTBs).
    // The returned futures will most-likely capture the `&mut T`and are therefore poised
    // by its lifetime. Without the trait-object usage, this would not work.
    pub fn collect_async<T, F>(&self, into: T, collect: F) -> Collector<T>
    where
        T: Debug + Send + Sync + 'static,
        F: Fn(
                String,
                &mut T,
            ) -> Pin<
                Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send + '_>,
            > + Send
            + 'static,
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
                            Err(_err) => {} // TODO: Handle error?
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
            task: capture,
            task_termination_sender: term_sig_tx,
        }
    }

    pub fn collect_into_vec(&self) -> Collector<Vec<String>> {
        self.collect(Vec::new(), |line, vec| vec.push(line))
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

pub struct WaitFor {
    task: AbortHandle,
}

impl Drop for WaitFor {
    fn drop(&mut self) {
        self.task.abort();
    }
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
                &"<tokio::sync::broadcast::Sender<Option<String>>>",
            )
            .finish()
    }
}

pub fn extract_output_streams(mut child: Child) -> (Child, OutputStream, OutputStream) {
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
                            Err(_err) => {}
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

    let (tx_stdout, _rx_stdout) = tokio::sync::broadcast::channel::<Option<String>>(16);
    let (tx_stderr, _rx_stderr) = tokio::sync::broadcast::channel::<Option<String>>(16);

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

/// A collector usable in `collect_async`.
pub fn collect_to_file(
    line: String,
    file: &mut tokio::fs::File,
) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send + '_>> {
    use tokio::io::AsyncWriteExt;
    Box::pin(async move {
        file.write_all(line.as_bytes()).await?;
        file.write("\n".as_bytes()).await?;
        Ok(())
    })
}

/// A collector usable in `collect_async`.
pub fn collect_to_file_buffered(
    line: String,
    file: &mut tokio::io::BufWriter<tokio::fs::File>,
) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn Error + Send + Sync>>> + Send + '_>> {
    use tokio::io::AsyncWriteExt;
    Box::pin(async move {
        file.write_all(line.as_bytes()).await?;
        file.write("\n".as_bytes()).await?;
        Ok(())
    })
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;
    use std::process::Stdio;

    use tokio::process::Command;

    use crate::Exec;

    #[tokio::test]
    async fn test() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = Exec::new_from_child_with_piped_io(child);
        let (status, stdout, stderr) = exec.wait_with_output().await.unwrap();
        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn manually_await_with_collectors() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = Exec::new_from_child_with_piped_io(child);

        let mut temp_file_out = tempfile::tempfile().unwrap();
        let mut temp_file_err = tempfile::tempfile().unwrap();

        let out_collector = exec
            .std_out_stream
            .collect(temp_file_out, |line, temp_file| {
                writeln!(temp_file, "{}", line).unwrap();
            });
        let err_collector = exec
            .std_err_stream
            .collect(temp_file_err, |line, temp_file| {
                writeln!(temp_file, "{}", line).unwrap();
            });

        let status = exec.wait().await.unwrap();
        let stdout = out_collector.abort().await;
        let stderr = err_collector.abort().await;

        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn manually_await_with_async_collectors() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = Exec::new_from_child_with_piped_io(child);

        let mut temp_file_out: File = tempfile::tempfile().unwrap();
        let mut temp_file_err: File = tempfile::tempfile().unwrap();

        let out_collector =
            exec.std_out_stream
                .collect_async(temp_file_out, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(())
                    })
                });
        let err_collector =
            exec.std_err_stream
                .collect_async(temp_file_err, |line, temp_file: &mut File| {
                    Box::pin(async move {
                        writeln!(temp_file, "{}", line).unwrap();
                        Ok(())
                    })
                });

        let status = exec.wait().await.unwrap();
        let stdout = out_collector.abort().await;
        let stderr = err_collector.abort().await;

        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }
}
