//! Staged builder fusing plain wait, output collection, and graceful termination on a single
//! entry point: [`ProcessHandle::wait_for_completion`].
//!
//! The builder is configured by chaining `.with_line_output(...)` or `.with_raw_output(...)`
//! and / or `.or_terminate(...)` (in that order, output before terminate) and is run by
//! `.await`-ing it. The matrix of six combinations (no / line / raw output, plain wait /
//! wait-or-terminate) is encoded in the type system so each chain compiles to exactly one
//! terminal action.
//!
//! ```ignore
//! // no output, no terminate
//! handle.wait_for_completion(timeout).await?;
//!
//! // line output
//! handle.wait_for_completion(timeout)
//!     .with_line_output(eof_timeout, parsing_options, line_options)
//!     .await?;
//!
//! // line output + graceful termination on timeout
//! handle.wait_for_completion(timeout)
//!     .with_line_output(eof_timeout, parsing_options, line_options)
//!     .or_terminate(shutdown)
//!     .await?;
//! ```

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::time::Duration;

use crate::error::{WaitError, WaitForCompletionResult, WaitWithOutputError};
#[cfg(any(unix, windows))]
use crate::error::{WaitForCompletionOrTerminateResult, WaitOrTerminateError};
use crate::output_stream::line::options::LineParsingOptions;
use crate::output_stream::{OutputStream, Subscribable};
use crate::process_handle::ProcessHandle;
#[cfg(any(unix, windows))]
use crate::process_handle::output_collection::drain::wait_for_completion_or_terminate_with_collectors;
use crate::process_handle::output_collection::drain::wait_for_completion_with_collectors;
use crate::process_handle::output_collection::options::{LineOutputOptions, RawOutputOptions};
use crate::process_handle::output_collection::{
    ProcessOutput, spawn_chunk_collector, spawn_line_collector,
};
#[cfg(any(unix, windows))]
use crate::process_handle::termination::GracefulShutdown;
use crate::{CollectedBytes, CollectedLines};

/// Type-state markers for [`WaitForCompletion`].
///
/// The two state axes (`Output` and `Terminate`) gate which builder methods compile and which
/// `IntoFuture` impl the chain resolves to. The carried data on the populated states (eof
/// timeout, options, shutdown) is internal; users construct these states via
/// [`WaitForCompletion::with_line_output`], [`WaitForCompletion::with_raw_output`], and
/// [`WaitForCompletion::or_terminate`].
pub mod state {
    #[cfg(any(unix, windows))]
    use super::GracefulShutdown;
    use super::{Duration, LineOutputOptions, LineParsingOptions, RawOutputOptions};

    /// No output collection has been configured.
    #[derive(Debug)]
    pub struct NoOutput;

    /// Line-mode output collection has been configured.
    #[derive(Debug)]
    pub struct LineOutput {
        pub(super) eof_timeout: Duration,
        pub(super) line_parsing_options: LineParsingOptions,
        pub(super) options: LineOutputOptions,
    }

    /// Raw byte output collection has been configured.
    #[derive(Debug)]
    pub struct RawOutput {
        pub(super) eof_timeout: Duration,
        pub(super) options: RawOutputOptions,
    }

    /// No graceful termination on timeout.
    #[derive(Debug)]
    pub struct NoTerminate;

    /// Graceful termination on timeout has been configured.
    #[cfg(any(unix, windows))]
    #[derive(Debug)]
    pub struct WithTerminate {
        pub(super) shutdown: GracefulShutdown,
    }
}

#[cfg(any(unix, windows))]
use state::WithTerminate;
use state::{LineOutput, NoOutput, NoTerminate, RawOutput};

/// Staged builder returned by [`ProcessHandle::wait_for_completion`].
///
/// Compose one wait, optionally one output mode, and optionally graceful termination on
/// timeout, then `.await` to run it. See the module-level docs for examples.
///
/// Stdin is closed before the wait begins (matching [`tokio::process::Child::wait`]) on
/// every variant. If the wait times out without `.or_terminate(...)`, the process keeps
/// running; with `.or_terminate(...)`, cleanup is forced through
/// [`ProcessHandle::terminate`].
#[must_use = "calling `wait_for_completion(...)` only configures the wait. \
              `.await` the builder (or chain `.with_*_output(...)` / `.or_terminate(...)` first) \
              to actually run it."]
pub struct WaitForCompletion<'a, Stdout, Stderr, Output, Terminate>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    handle: &'a mut ProcessHandle<Stdout, Stderr>,
    timeout: Duration,
    output: Output,
    terminate: Terminate,
}

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Begin a staged wait for the process to run to completion within `timeout`.
    ///
    /// `.await` the returned builder to wait without collecting output. Chain
    /// `.with_line_output(...)` or `.with_raw_output(...)` to also drain stdout / stderr into a
    /// [`ProcessOutput`], and / or `.or_terminate(...)` to force graceful cleanup if the wait
    /// times out. Output collection must be configured before termination.
    ///
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`] and helping avoid deadlocks where the child is waiting
    /// for input while the parent is waiting for exit.
    pub fn wait_for_completion(
        &mut self,
        timeout: Duration,
    ) -> WaitForCompletion<'_, Stdout, Stderr, NoOutput, NoTerminate> {
        WaitForCompletion {
            handle: self,
            timeout,
            output: NoOutput,
            terminate: NoTerminate,
        }
    }
}

impl<'a, Stdout, Stderr> WaitForCompletion<'a, Stdout, Stderr, NoOutput, NoTerminate>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Configure line-mode output collection.
    ///
    /// Collectors are attached when this method is called. If the stream was configured with
    /// `.no_replay()`, output produced before attachment may be discarded; configure replay
    /// before spawning when startup output must be included.
    ///
    /// `eof_timeout` bounds the additional post-exit wait for stdout / stderr consumers to
    /// observe EOF after process completion (or, when paired with `.or_terminate(...)`, after
    /// cleanup termination). It is a single budget shared by stdout and stderr: when one
    /// stream finishes early, the surviving stream's drain is still bounded by the original
    /// `eof_timeout` measured from process exit, not restarted from the first stream's EOF.
    /// A stream still producing output once the budget is exhausted is aborted and
    /// [`WaitWithOutputError::OutputCollectionTimeout`] is returned.
    pub fn with_line_output(
        self,
        eof_timeout: Duration,
        line_parsing_options: LineParsingOptions,
        options: LineOutputOptions,
    ) -> WaitForCompletion<'a, Stdout, Stderr, LineOutput, NoTerminate> {
        WaitForCompletion {
            handle: self.handle,
            timeout: self.timeout,
            output: LineOutput {
                eof_timeout,
                line_parsing_options,
                options,
            },
            terminate: NoTerminate,
        }
    }

    /// Configure raw byte output collection.
    ///
    /// Use this when the child's output is not UTF-8 line-oriented (binary blobs, framed
    /// protocols, anything where line parsing would corrupt bytes). `eof_timeout` behaves the
    /// same as in [`Self::with_line_output`].
    pub fn with_raw_output(
        self,
        eof_timeout: Duration,
        options: RawOutputOptions,
    ) -> WaitForCompletion<'a, Stdout, Stderr, RawOutput, NoTerminate> {
        WaitForCompletion {
            handle: self.handle,
            timeout: self.timeout,
            output: RawOutput {
                eof_timeout,
                options,
            },
            terminate: NoTerminate,
        }
    }
}

#[cfg(any(unix, windows))]
impl<'a, Stdout, Stderr, Output> WaitForCompletion<'a, Stdout, Stderr, Output, NoTerminate>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Force graceful termination if the wait times out.
    ///
    /// On a wait timeout (or a non-timeout wait failure), [`ProcessHandle::terminate`] runs
    /// the supplied `shutdown` sequence and the result is reported as
    /// [`WaitForCompletionOrTerminateResult::TerminatedAfterTimeout`] on success, or as a
    /// [`WaitOrTerminateError`] / [`WaitWithOutputError`] when termination itself fails.
    ///
    /// Total wall-clock time can exceed the wait timeout plus the per-platform graceful
    /// budget carried by `shutdown` (the sum of every Unix phase timeout, or `timeout` on
    /// Windows) by one additional fixed 3-second wait when the force-kill fallback is
    /// required.
    pub fn or_terminate(
        self,
        shutdown: GracefulShutdown,
    ) -> WaitForCompletion<'a, Stdout, Stderr, Output, WithTerminate> {
        WaitForCompletion {
            handle: self.handle,
            timeout: self.timeout,
            output: self.output,
            terminate: WithTerminate { shutdown },
        }
    }
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl<'a, Stdout, Stderr> IntoFuture for WaitForCompletion<'a, Stdout, Stderr, NoOutput, NoTerminate>
where
    Stdout: OutputStream + Send + 'a,
    Stderr: OutputStream + Send + 'a,
{
    type Output = Result<WaitForCompletionResult, WaitError>;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move { self.handle.wait_for_completion_inner(self.timeout).await })
    }
}

#[cfg(any(unix, windows))]
impl<'a, Stdout, Stderr> IntoFuture
    for WaitForCompletion<'a, Stdout, Stderr, NoOutput, WithTerminate>
where
    Stdout: OutputStream + Send + 'a,
    Stderr: OutputStream + Send + 'a,
{
    type Output = Result<WaitForCompletionOrTerminateResult, WaitOrTerminateError>;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            self.handle
                .wait_for_completion_or_terminate_inner(self.timeout, self.terminate.shutdown)
                .await
        })
    }
}

impl<'a, Stdout, Stderr> IntoFuture
    for WaitForCompletion<'a, Stdout, Stderr, LineOutput, NoTerminate>
where
    Stdout: OutputStream + Subscribable + Send + 'a,
    Stderr: OutputStream + Subscribable + Send + 'a,
{
    type Output =
        Result<WaitForCompletionResult<ProcessOutput<CollectedLines>>, WaitWithOutputError>;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let LineOutputOptions {
                stdout_collection_options,
                stderr_collection_options,
            } = self.output.options;
            let line_parsing_options = self.output.line_parsing_options;

            let (out_collector, err_collector) = self
                .handle
                .try_spawn_output_collectors(
                    |name, sub| {
                        spawn_line_collector(
                            name,
                            sub,
                            line_parsing_options,
                            stdout_collection_options,
                        )
                    },
                    |name, sub| {
                        spawn_line_collector(
                            name,
                            sub,
                            line_parsing_options,
                            stderr_collection_options,
                        )
                    },
                )
                .await?;

            wait_for_completion_with_collectors(
                self.handle,
                self.timeout,
                self.output.eof_timeout,
                out_collector,
                err_collector,
            )
            .await
        })
    }
}

impl<'a, Stdout, Stderr> IntoFuture
    for WaitForCompletion<'a, Stdout, Stderr, RawOutput, NoTerminate>
where
    Stdout: OutputStream + Subscribable + Send + 'a,
    Stderr: OutputStream + Subscribable + Send + 'a,
{
    type Output =
        Result<WaitForCompletionResult<ProcessOutput<CollectedBytes>>, WaitWithOutputError>;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let RawOutputOptions {
                stdout_collection_options,
                stderr_collection_options,
            } = self.output.options;

            let (out_collector, err_collector) = self
                .handle
                .try_spawn_output_collectors(
                    |name, sub| spawn_chunk_collector(name, sub, stdout_collection_options),
                    |name, sub| spawn_chunk_collector(name, sub, stderr_collection_options),
                )
                .await?;

            wait_for_completion_with_collectors(
                self.handle,
                self.timeout,
                self.output.eof_timeout,
                out_collector,
                err_collector,
            )
            .await
        })
    }
}

#[cfg(any(unix, windows))]
impl<'a, Stdout, Stderr> IntoFuture
    for WaitForCompletion<'a, Stdout, Stderr, LineOutput, WithTerminate>
where
    Stdout: OutputStream + Subscribable + Send + 'a,
    Stderr: OutputStream + Subscribable + Send + 'a,
{
    type Output = Result<
        WaitForCompletionOrTerminateResult<ProcessOutput<CollectedLines>>,
        WaitWithOutputError,
    >;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let LineOutputOptions {
                stdout_collection_options,
                stderr_collection_options,
            } = self.output.options;
            let line_parsing_options = self.output.line_parsing_options;

            let (out_collector, err_collector) = self
                .handle
                .try_spawn_output_collectors(
                    |name, sub| {
                        spawn_line_collector(
                            name,
                            sub,
                            line_parsing_options,
                            stdout_collection_options,
                        )
                    },
                    |name, sub| {
                        spawn_line_collector(
                            name,
                            sub,
                            line_parsing_options,
                            stderr_collection_options,
                        )
                    },
                )
                .await?;

            wait_for_completion_or_terminate_with_collectors(
                self.handle,
                self.timeout,
                self.terminate.shutdown,
                self.output.eof_timeout,
                out_collector,
                err_collector,
            )
            .await
        })
    }
}

#[cfg(any(unix, windows))]
impl<'a, Stdout, Stderr> IntoFuture
    for WaitForCompletion<'a, Stdout, Stderr, RawOutput, WithTerminate>
where
    Stdout: OutputStream + Subscribable + Send + 'a,
    Stderr: OutputStream + Subscribable + Send + 'a,
{
    type Output = Result<
        WaitForCompletionOrTerminateResult<ProcessOutput<CollectedBytes>>,
        WaitWithOutputError,
    >;
    type IntoFuture = BoxFut<'a, Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let RawOutputOptions {
                stdout_collection_options,
                stderr_collection_options,
            } = self.output.options;

            let (out_collector, err_collector) = self
                .handle
                .try_spawn_output_collectors(
                    |name, sub| spawn_chunk_collector(name, sub, stdout_collection_options),
                    |name, sub| spawn_chunk_collector(name, sub, stderr_collection_options),
                )
                .await?;

            wait_for_completion_or_terminate_with_collectors(
                self.handle,
                self.timeout,
                self.terminate.shutdown,
                self.output.eof_timeout,
                out_collector,
                err_collector,
            )
            .await
        })
    }
}
