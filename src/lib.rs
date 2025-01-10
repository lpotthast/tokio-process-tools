mod interrupt;

use std::borrow::Cow;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};
use tokio::process::Child;
use tokio::sync::oneshot::Sender;
use tokio::sync::RwLock;
use tokio::task::{AbortHandle, JoinHandle};
use tokio::time::error::Elapsed;
use tracing::info;

#[derive(Debug)]
pub struct TerminateOnDrop {
    pub(crate) process_handle: ProcessHandle,
    pub(crate) graceful_termination_timeout: Option<Duration>,
    pub(crate) forceful_termination_timeout: Option<Duration>,
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        // 1. First, we're in a Drop implementation which is synchronous - it can't be async.
        // But we need to execute an async operation (the `terminate` call).
        //
        // 2. `block_on` is needed because it takes an async operation and runs it to completion
        // synchronously - it's how we can execute our async terminate call within the synchronous
        // drop.
        //
        // 3. However, block_on by itself isn't safe to call from within an async context
        // (which we are in since we're inside the Tokio runtime).
        // This is because it could lead to deadlocks - imagine if the current thread is needed to
        // process some task that our blocked async operation is waiting on.
        //
        // 4. This is where block_in_place comes in - it tells Tokio:
        // "hey, I'm about to block this thread, please make sure other threads are available to
        // still process tasks". It essentially moves the blocking work to a dedicated thread pool
        // so that the async runtime can continue functioning.
        //
        // 5. Note that `block_in_place` requires a multithreaded tokio runtime to be active!
        // So use `#[tokio::test(flavor = "multi_thread")]` in tokio-enabled tests.
        //
        // 6. Also note that `block_in_place` enforces that the given closure run to completion,
        // even when the async executor is terminated - this might be because our program ended
        // or because we crashed due to a panic.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                if !self.process_handle.is_running().as_bool() {
                    tracing::debug!(
                        process = %self.process_handle.name,
                        "Process already terminated"
                    );
                    return;
                }

                tracing::debug!(process = %self.process_handle.name, "Terminating process");
                match self
                    .process_handle
                    .terminate(
                        self.graceful_termination_timeout,
                        self.forceful_termination_timeout,
                    )
                    .await
                {
                    Ok(exit_status) => {
                        tracing::debug!(
                            process = %self.process_handle.name,
                            ?exit_status,
                            "Successfully Terminated process"
                        )
                    }
                    Err(err) => {
                        panic!(
                            "Failed to terminate process '{}': {}",
                            self.process_handle.name, err
                        );
                    }
                };
            });
        });
    }
}

#[derive(Debug, Error)]
pub enum TerminationError {
    #[error("Failed to terminate process: {0}")]
    IoError(#[from] io::Error),

    #[error("Failed to terminate process. Graceful termination failed with: {graceful_error}. Forceful termination failed with: {forceful_error}")]
    TerminationFailed {
        /// The error that occurred during the graceful termination attempt.
        graceful_error: io::Error,

        /// The error that occurred during the forceful termination attempt.
        forceful_error: io::Error,
    },
}

/// Represents the running state of a process.
#[derive(Debug)]
pub enum IsRunning {
    /// Process is still running.
    Running,

    /// Process has terminated with the given exit status.
    NotRunning(ExitStatus),

    /// Failed to determine process state.
    Uncertain(io::Error),
}

impl IsRunning {
    pub fn as_bool(&self) -> bool {
        match self {
            IsRunning::Running => true,
            IsRunning::NotRunning(_) | IsRunning::Uncertain(_) => false,
        }
    }
}

impl From<IsRunning> for bool {
    fn from(is_running: IsRunning) -> Self {
        match is_running {
            IsRunning::Running => true,
            IsRunning::NotRunning(_) | IsRunning::Uncertain(_) => false,
        }
    }
}

#[derive(Debug, Error)]
pub enum WaitError {
    #[error("A general io error occurred")]
    IoError(#[from] io::Error),

    #[error("Collector failed")]
    CollectorFailed(#[from] CollectorError),
}

pub struct ProcessHandle {
    name: Cow<'static, str>,
    child: Child,
    std_out_stream: OutputStream,
    std_err_stream: OutputStream,
}

impl ProcessHandle {
    pub fn new_from_child_with_piped_io(name: impl Into<Cow<'static, str>>, child: Child) -> Self {
        Self::new_from_child_with_piped_io_and_capacity(name, child, 128, 128)
    }

    pub fn new_from_child_with_piped_io_and_capacity(
        name: impl Into<Cow<'static, str>>,
        child: Child,
        stdout_channel_capacity: usize,
        stderr_channel_capacity: usize,
    ) -> Self {
        let (child, std_out_stream, std_err_stream) =
            extract_output_streams(child, stdout_channel_capacity, stderr_channel_capacity);
        Self {
            name: name.into(),
            child,
            std_out_stream,
            std_err_stream,
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    //noinspection RsSelfConvention
    pub fn is_running(&mut self) -> IsRunning {
        match self.child.try_wait() {
            Ok(None) => IsRunning::Running,
            Ok(Some(exit_status)) => IsRunning::NotRunning(exit_status),
            Err(err) => IsRunning::Uncertain(err),
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

    pub async fn wait_with_output(
        &mut self,
    ) -> Result<(ExitStatus, Vec<String>, Vec<String>), WaitError> {
        let out_collector = self.std_out_stream.collect_into_vec();
        let err_collector = self.std_err_stream.collect_into_vec();

        let status = self.child.wait().await?;
        let std_out = out_collector.abort().await?;
        let std_err = err_collector.abort().await?;

        Ok((status, std_out, std_err))
    }

    pub fn terminate_on_drop(
        self,
        graceful_termination_timeout: Option<Duration>,
        forceful_termination_timeout: Option<Duration>,
    ) -> TerminateOnDrop {
        TerminateOnDrop {
            process_handle: self,
            graceful_termination_timeout,
            forceful_termination_timeout,
        }
    }

    pub async fn terminate(
        &mut self,
        graceful_shutdown_timeout: Option<Duration>,
        forceful_shutdown_timeout: Option<Duration>,
    ) -> Result<ExitStatus, TerminationError> {
        // Try a graceful shutdown first.
        interrupt::send_interrupt(&self.child).await?;

        // Wait for process termination.
        match self.await_termination(graceful_shutdown_timeout).await {
            Ok(exit_status) => Ok(exit_status),
            Err(graceful_err) => {
                // Log the graceful shutdown failure
                tracing::warn!(
                    process = %self.name,
                    error = %graceful_err,
                    "Graceful shutdown failed, attempting forceful termination"
                );

                // Try a forceful kill
                match self.child.kill().await {
                    Ok(()) => match self.await_termination(forceful_shutdown_timeout).await {
                        Ok(exit_status) => Ok(exit_status),
                        Err(err) => Err(TerminationError::from(err)),
                    },
                    Err(forceful_err) => Err(TerminationError::TerminationFailed {
                        graceful_error: graceful_err,
                        forceful_error: forceful_err,
                    }),
                }
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

impl Debug for ProcessHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Exec")
            .field("child", &self.child)
            .field("std_out_stream", &self.std_out_stream)
            .field("std_err_stream", &self.std_err_stream)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputType {
    StdOut,
    StdErr,
}

#[derive(Debug, Error)]
pub enum CollectorError {
    #[error("The collector task could not be joined/terminated: {0}")]
    TaskJoin(#[source] tokio::task::JoinError),
}

pub trait Sink: Debug + Send + Sync + 'static {}

impl<T> Sink for T where T: Debug + Send + Sync + 'static {}

/// A collector for output lines.
///
/// For proper cleanup, call `abort()` which gracefully waits for the collecting task to complete.
/// It should terminate fast, as an internal termination signal is send to it.
/// If dropped without calling `abort()`, the termination will be sent as well,
/// but the task will be aborted (forceful, not waiting for completion).
pub struct Collector<T: Sink> {
    task: Option<JoinHandle<T>>,
    task_termination_sender: Option<Sender<()>>,
}

impl<T: Sink> Collector<T> {
    pub async fn abort(mut self) -> Result<T, CollectorError> {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // Safety: This `expect` call SHOULD neve fail. The receiver lives in the tokio task,
            // and is only dropped after receiving the termination signal.
            // The task is only awaited-and-dropped after THIS send and only ONCE, gated by taking
            // it out the `Option`, which can only be done once.
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to collector task."
                );
            };
        }
        if let Some(task) = self.task.take() {
            return task.await.map_err(CollectorError::TaskJoin);
        }
        unreachable!("The collector task was already aborted");
    }
}

impl<T: Sink> Drop for Collector<T> {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to collector task."
                );
            }
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug, Error)]
pub enum InspectorError {
    #[error("The inspector task could not be joined/terminated: {0}")]
    TaskJoin(#[from] tokio::task::JoinError),
}

/// An inspector for output lines.
///
/// For proper cleanup, call `abort()` which gracefully waits for the collecting task to complete.
/// It should terminate fast, as an internal termination signal is send to it.
/// If dropped without calling `abort()`, the termination will be sent as well,
/// but the task will be aborted (forceful, not waiting for completion).
pub struct Inspector {
    task: Option<JoinHandle<()>>,
    task_termination_sender: Option<Sender<()>>,
}

impl Inspector {
    pub async fn abort(mut self) -> Result<(), InspectorError> {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            // Safety: This `expect` call SHOULD neve fail. The receiver lives in the tokio task,
            // and is only dropped after receiving the termination signal.
            // The task is only awaited-and-dropped after THIS send and only ONCE, gated by taking
            // it out the `Option`, which can only be done once.
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to inspector task."
                );
            };
        }
        if let Some(task) = self.task.take() {
            return task.await.map_err(InspectorError::TaskJoin);
        }
        unreachable!("The inspector task was already aborted");
    }
}

impl Drop for Inspector {
    fn drop(&mut self) {
        if let Some(task_termination_sender) = self.task_termination_sender.take() {
            if let Err(_err) = task_termination_sender.send(()) {
                tracing::error!(
                    "Unexpected failure when sending termination signal to inspector task."
                );
            }
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
                        info!("Terminating collector!");
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
        F: Fn(
                String,
                &mut S,
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
    use crate::ProcessHandle;
    use mockall::*;
    use std::fs::File;
    use std::io::Write;
    use std::process::Stdio;
    use tokio::process::Command;

    #[tokio::test]
    async fn test() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);
        let (status, stdout, stderr) = exec.wait_with_output().await.unwrap();
        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }

    #[tokio::test]
    async fn manually_await_with_sync_inspector() {
        let child = Command::new("ls")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn `ls` command");
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

        #[automock]
        trait FunctionCaller {
            fn call_function(&self, input: String);
        }

        let mut out_mock = MockFunctionCaller::new();
        out_mock
            .expect_call_function()
            .with(predicate::eq("Cargo.lock".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("Cargo.toml".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("README.md".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("src".to_string()))
            .times(1)
            .return_const(());
        out_mock
            .expect_call_function()
            .with(predicate::eq("target".to_string()))
            .times(1)
            .return_const(());
        let out_callback = move |input: String| {
            out_mock.call_function(input);
        };

        let err_mock = MockFunctionCaller::new();
        let err_callback = move |input: String| {
            err_mock.call_function(input);
        };

        let out_inspector = exec.stdout().inspect(move |line| {
            out_callback(line);
        });
        let err_inspector = exec.stderr().inspect(move |line| {
            err_callback(line);
        });

        let status = exec.wait().await.unwrap();
        let stdout = out_inspector.abort().await.unwrap();
        let stderr = err_inspector.abort().await.unwrap();

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
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

        let temp_file_out = tempfile::tempfile().unwrap();
        let temp_file_err = tempfile::tempfile().unwrap();

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
        let stdout = out_collector.abort().await.unwrap();
        let stderr = err_collector.abort().await.unwrap();

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
        let mut exec = ProcessHandle::new_from_child_with_piped_io("ls", child);

        let temp_file_out: File = tempfile::tempfile().unwrap();
        let temp_file_err: File = tempfile::tempfile().unwrap();

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
        let stdout = out_collector.abort().await.unwrap();
        let stderr = err_collector.abort().await.unwrap();

        println!("{:?}", status);
        println!("{:?}", stdout);
        println!("{:?}", stderr);
    }
}
