//! Error types for process operations.

use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

use crate::ConsumerError;

/// Errors that can occur when terminating a process.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TerminationError {
    /// Failed to manually send a graceful signal to the process.
    #[error(
        "Failed to send signal to process '{process_name}'.{}",
        DisplayAttemptErrors(.attempt_errors.as_slice())
    )]
    SignalFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// Errors recorded while attempting to send the signal, in chronological order.
        attempt_errors: Vec<TerminationAttemptError>,
    },

    /// Failed to terminate the process after trying all platform termination signals.
    #[error(
        "Failed to terminate process '{process_name}'.{}",
        DisplayAttemptErrors(.attempt_errors.as_slice())
    )]
    TerminationFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// Errors recorded while attempting process termination, in chronological order.
        attempt_errors: Vec<TerminationAttemptError>,
    },
}

impl TerminationError {
    /// The name of the process involved in the termination error.
    #[must_use]
    pub fn process_name(&self) -> &str {
        match self {
            Self::SignalFailed { process_name, .. }
            | Self::TerminationFailed { process_name, .. } => process_name,
        }
    }

    /// Errors recorded while attempting the operation, in chronological order.
    #[must_use]
    pub fn attempt_errors(&self) -> &[TerminationAttemptError] {
        match self {
            Self::SignalFailed { attempt_errors, .. }
            | Self::TerminationFailed { attempt_errors, .. } => attempt_errors,
        }
    }
}

struct DisplayAttemptErrors<'a>(&'a [TerminationAttemptError]);

impl fmt::Display for DisplayAttemptErrors<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, " No attempt error was recorded.");
        }

        write!(f, " Attempt errors:")?;
        for (index, attempt_error) in self.0.iter().enumerate() {
            write!(f, " [{}] {attempt_error}", index + 1)?;
        }

        Ok(())
    }
}

struct DisplaySignalNameSuffix(Option<&'static str>);

impl fmt::Display for DisplaySignalNameSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(signal_name) = self.0 else {
            return Ok(());
        };

        write!(f, " for {signal_name}")
    }
}

/// A failed operation recorded while attempting to terminate a process.
#[derive(Debug, Error)]
#[error(
    "{phase} {operation} failed{}: {source}",
    DisplaySignalNameSuffix(*.signal_name)
)]
#[non_exhaustive]
pub struct TerminationAttemptError {
    /// Termination phase where the failure happened.
    pub phase: TerminationAttemptPhase,
    /// Operation that failed during the phase.
    pub operation: TerminationAttemptOperation,
    /// Platform signal involved in the failed operation, when applicable.
    pub signal_name: Option<&'static str>,
    /// Original source error.
    #[source]
    pub source: Box<dyn Error + Send + Sync + 'static>,
}

/// Termination phase where an attempt error was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationAttemptPhase {
    /// Initial process status check before any termination signal is sent.
    Preflight,

    /// Graceful interrupt phase.
    ///
    /// Only emitted on Unix, where this is the `SIGINT` step that begins the graceful escalation.
    /// Windows has no targetable `SIGINT` analogue: `GenerateConsoleCtrlEvent` cannot deliver
    /// `CTRL_C_EVENT` to a single child group, and `CTRL_BREAK_EVENT` is harsher than a Unix
    /// interrupt, so Windows skips this phase and goes straight to [`Self::Terminate`].
    Interrupt,

    /// Graceful terminate phase.
    ///
    /// On Unix this is the `SIGTERM` step that follows the `SIGINT` phase. On Windows this
    /// represents the single `CTRL_BREAK_EVENT` graceful step (the only console control event
    /// `GenerateConsoleCtrlEvent` can target at a nonzero process group); it sits in this phase
    /// rather than [`Self::Interrupt`] because `CTRL_BREAK_EVENT` is closer in severity to
    /// `SIGTERM` than to `SIGINT`.
    Terminate,

    /// Forceful kill phase.
    Kill,
}

impl fmt::Display for TerminationAttemptPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preflight => f.write_str("preflight"),
            Self::Interrupt => f.write_str("interrupt"),
            Self::Terminate => f.write_str("terminate"),
            Self::Kill => f.write_str("kill"),
        }
    }
}

/// Termination operation that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationAttemptOperation {
    /// Checking whether the process has already exited failed.
    CheckStatus,
    /// Sending a graceful or forceful termination signal failed.
    SendSignal,
    /// Waiting for the process to exit after a termination signal failed.
    WaitForExit,
}

impl fmt::Display for TerminationAttemptOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckStatus => f.write_str("status check"),
            Self::SendSignal => f.write_str("signal send"),
            Self::WaitForExit => f.write_str("exit wait"),
        }
    }
}

/// Errors that can occur when waiting for process operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WaitError {
    /// A general IO error occurred.
    #[error("IO error occurred while waiting for process '{process_name}': {source}")]
    IoError {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The underlying IO error.
        #[source]
        source: io::Error,
    },
}

/// Result of waiting for a process to complete within an explicit timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForCompletionResult<T = ExitStatus> {
    /// The process completed before the timeout elapsed.
    Completed(T),

    /// The timeout elapsed before the process completed.
    Timeout {
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },
}

impl<T> WaitForCompletionResult<T> {
    /// Returns the completed value, or `None` if the wait timed out.
    #[must_use]
    pub fn into_completed(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            Self::Timeout { .. } => None,
        }
    }

    /// Returns the completed value, panicking with `message` if the wait timed out.
    ///
    /// # Panics
    ///
    /// Panics with `message` if this result is [`WaitForCompletionResult::Timeout`].
    pub fn expect_completed(self, message: &str) -> T {
        self.into_completed().expect(message)
    }

    /// Maps a completed value while preserving timeout outcomes.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> WaitForCompletionResult<U> {
        match self {
            Self::Completed(value) => WaitForCompletionResult::Completed(f(value)),
            Self::Timeout { timeout } => WaitForCompletionResult::Timeout { timeout },
        }
    }
}

/// Result of waiting for a process to complete, terminating it if the wait times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForCompletionOrTerminateResult<T = ExitStatus> {
    /// The process completed before the wait timeout elapsed.
    Completed(T),

    /// The wait timeout elapsed, then cleanup termination completed successfully.
    TerminatedAfterTimeout {
        /// The result observed after cleanup termination.
        result: T,
        /// The wait timeout duration that was exceeded before cleanup began.
        timeout: Duration,
    },
}

impl<T> WaitForCompletionOrTerminateResult<T> {
    /// Returns the terminal value, whether completion happened before timeout or after cleanup.
    #[must_use]
    pub fn into_result(self) -> T {
        match self {
            Self::Completed(value) | Self::TerminatedAfterTimeout { result: value, .. } => value,
        }
    }

    /// Returns the completed value, or `None` if cleanup termination was required after timeout.
    #[must_use]
    pub fn into_completed(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            Self::TerminatedAfterTimeout { .. } => None,
        }
    }

    /// Returns the completed value, panicking with `message` if cleanup termination was required.
    ///
    /// # Panics
    ///
    /// Panics with `message` if this result is
    /// [`WaitForCompletionOrTerminateResult::TerminatedAfterTimeout`].
    pub fn expect_completed(self, message: &str) -> T {
        self.into_completed().expect(message)
    }

    /// Maps a terminal value while preserving the timeout/cleanup outcome.
    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> WaitForCompletionOrTerminateResult<U> {
        match self {
            Self::Completed(value) => WaitForCompletionOrTerminateResult::Completed(f(value)),
            Self::TerminatedAfterTimeout { result, timeout } => {
                WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
                    result: f(result),
                    timeout,
                }
            }
        }
    }
}

/// Errors that can occur when waiting for a process with automatic termination on failure.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WaitOrTerminateError {
    /// Waiting failed, but the subsequent cleanup termination succeeded.
    #[error(
        "Waiting for process '{process_name}' failed with '{wait_error}', then cleanup termination completed with status {termination_status}"
    )]
    WaitFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The original error returned while waiting for the process.
        #[source]
        wait_error: Box<WaitError>,
        /// The status observed after cleanup termination.
        termination_status: ExitStatus,
    },

    /// Waiting failed, and the subsequent cleanup termination also failed.
    #[error(
        "Waiting for process '{process_name}' failed with '{wait_error}', then cleanup termination also failed: {termination_error}"
    )]
    TerminationFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The original error returned while waiting for the process.
        #[source]
        wait_error: Box<WaitError>,
        /// The error returned while trying to terminate the process after the wait failure.
        termination_error: TerminationError,
    },

    /// Waiting timed out, and the subsequent cleanup termination failed.
    #[error(
        "Process '{process_name}' did not complete within {timeout:?}, then cleanup termination failed: {termination_error}"
    )]
    TerminationAfterTimeoutFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The wait timeout duration that was exceeded before cleanup began.
        timeout: Duration,
        /// The error returned while trying to terminate the process after the timeout.
        #[source]
        termination_error: TerminationError,
    },
}

/// Errors that can occur when waiting for a process while collecting its output, with or
/// without automatic termination on timeout.
///
/// `WaitFailed` is emitted by APIs without automatic termination
/// (`wait_for_completion_with_output`, `wait_for_completion_with_raw_output`).
/// `WaitOrTerminateFailed` is emitted by the `*_or_terminate` variants. The remaining variants
/// can be emitted by either family.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WaitWithOutputError {
    /// Waiting for the process failed.
    #[error("Waiting for process completion failed: {0}")]
    WaitFailed(#[from] WaitError),

    /// Waiting with automatic termination failed.
    #[error("Wait-or-terminate operation failed: {0}")]
    WaitOrTerminateFailed(#[from] WaitOrTerminateError),

    /// Output collection did not complete before the operation timeout elapsed.
    #[error("Output collection for process '{process_name}' did not complete within {timeout:?}")]
    OutputCollectionTimeout {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },

    /// Collecting stdout or stderr failed.
    #[error("Output collection for process '{process_name}' failed: {source}")]
    OutputCollectionFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The collector error that caused output collection to fail.
        #[source]
        source: ConsumerError,
    },

    /// Starting stdout or stderr output collection failed.
    #[error("Output collection for process '{process_name}' could not start: {source}")]
    OutputCollectionStartFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The stream consumer error that prevented output collection from starting.
        #[source]
        source: StreamConsumerError,
    },
}

/// Errors that can occur when spawning a process.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// Failed to spawn the process.
    #[error("Failed to spawn process '{process_name}': {source}")]
    SpawnFailed {
        /// The name or description of the process being spawned.
        process_name: Cow<'static, str>,
        /// The underlying IO error.
        #[source]
        source: io::Error,
    },
}

/// Errors that can occur when creating a stream consumer.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamConsumerError {
    /// A single-subscriber stream already has an active consumer.
    #[error("Stream '{stream_name}' already has an active consumer")]
    ActiveConsumer {
        /// The name of the stream that rejected the consumer.
        stream_name: &'static str,
    },
}

impl StreamConsumerError {
    /// The name of the stream that rejected the consumer.
    #[must_use]
    pub fn stream_name(&self) -> &'static str {
        match self {
            Self::ActiveConsumer { stream_name } => stream_name,
        }
    }
}

/// Error emitted when an output stream cannot be read to completion.
#[derive(Debug, Clone, Error)]
#[error("Could not read from stream '{stream_name}': {source}")]
pub struct StreamReadError {
    stream_name: &'static str,
    #[source]
    source: Arc<io::Error>,
}

impl StreamReadError {
    /// Creates a stream read error from the stream name and underlying IO error.
    #[must_use]
    pub fn new(stream_name: &'static str, source: io::Error) -> Self {
        Self {
            stream_name,
            source: Arc::new(source),
        }
    }

    /// The name of the stream that failed.
    #[must_use]
    pub fn stream_name(&self) -> &'static str {
        self.stream_name
    }

    /// The [`io::ErrorKind`] of the underlying read failure.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        self.source.kind()
    }

    /// The underlying IO error.
    #[must_use]
    pub fn source_io_error(&self) -> &io::Error {
        self.source.as_ref()
    }
}

impl PartialEq for StreamReadError {
    fn eq(&self, other: &Self) -> bool {
        self.stream_name == other.stream_name && self.kind() == other.kind()
    }
}

impl Eq for StreamReadError {}

/// Result of waiting for an output line matching a predicate.
///
/// This enum is returned inside a `Result`; stream read failures are surfaced as
/// [`StreamReadError`] rather than as a variant of this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForLineResult {
    /// A matching line was observed before the stream ended or the timeout elapsed.
    Matched,

    /// The stream ended before any matching line was observed.
    StreamClosed,

    /// The timeout elapsed before a matching line was observed or the stream ended.
    Timeout,
}
