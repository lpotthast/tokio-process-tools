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
///
/// # Brief grace window after a failed signal send
///
/// During each graceful phase of [`crate::ProcessHandle::terminate`], if the signal send itself
/// fails (`EPERM` on macOS, `ESRCH` on Linux against a not-yet-reaped process group on Unix; the
/// equivalent `ERROR_INVALID_HANDLE` / `ERROR_ACCESS_DENIED` window on Windows), the library
/// applies a fixed 100 ms grace and re-checks for child exit before escalating. This covers the
/// small race where the child has already exited but Tokio's SIGCHLD reaper has not yet observed
/// it. The 100 ms grace replaces, never adds to, the user timeout for that phase. Real permission
/// denials and other genuine signal failures still surface here as `SignalFailed` or
/// `TerminationFailed` with the underlying `io::Error` preserved on each
/// [`TerminationAttemptError`].
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

/// A failed action recorded while attempting to terminate a process.
#[derive(Debug, Error)]
#[error("{action} failed")]
#[non_exhaustive]
pub struct TerminationAttemptError {
    /// Action that failed while attempting termination.
    pub action: TerminationAction,
    /// Original source error.
    #[source]
    pub source: Box<dyn Error + Send + Sync + 'static>,
}

/// Termination action that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TerminationAction {
    /// Checking whether the process has already exited failed.
    CheckStatus,

    /// Sending a signal failed.
    SendSignal {
        /// The name of the dispatched signal.
        signal_name: &'static str,
    },

    /// Waiting for the process to exit failed.
    WaitForExit,
}

impl fmt::Display for TerminationAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CheckStatus => f.write_str("status check"),
            Self::SendSignal { signal_name } => write!(f, "send signal {signal_name}"),
            Self::WaitForExit => f.write_str("exit wait"),
        }
    }
}

/// Errors that can occur when waiting for process operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WaitError {
    /// A general IO error occurred.
    #[error("IO error occurred while waiting for process '{process_name}'")]
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
#[must_use = "Discarding the result hides whether the process completed or the wait timed out; \
              match on the variants or call `into_completed`/`expect_completed`."]
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
}

/// Result of waiting for a process to complete, terminating it if the wait times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "Discarding the result hides whether the process completed naturally or was \
              terminated after the wait timeout; match on the variants or call `into_result`."]
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
        "Waiting for process '{process_name}' failed; cleanup termination completed with status {termination_status}"
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
    #[error("Termination of process '{process_name}' failed after '{wait_error}'.")]
    TerminationFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The original error returned while waiting for the process.
        wait_error: Box<WaitError>,
        /// The error returned while trying to terminate the process after the wait failure.
        #[source]
        termination_error: TerminationError,
    },

    /// Waiting timed out, and the subsequent cleanup termination failed.
    #[error(
        "Process '{process_name}' did not complete within {timeout:?}; cleanup termination failed"
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
/// `WaitFailed` is emitted from staged-builder chains without `.or_terminate(...)`.
/// `WaitOrTerminateFailed` is emitted from chains that include `.or_terminate(...)`. The
/// remaining variants can be emitted by either family.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WaitWithOutputError {
    /// Waiting for the process failed.
    #[error("Waiting for process completion failed")]
    WaitFailed(#[from] WaitError),

    /// Waiting with automatic termination failed.
    #[error("Wait-or-terminate operation failed")]
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
    #[error("Output collection for process '{process_name}' failed")]
    OutputCollectionFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The collector error that caused output collection to fail.
        #[source]
        source: ConsumerError,
    },

    /// Starting stdout or stderr output collection failed.
    #[error("Output collection for process '{process_name}' could not start")]
    OutputCollectionStartFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The stream consumer error that prevented output collection from starting.
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

/// Errors that can occur when spawning a process.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// Failed to spawn the process.
    #[error("Failed to spawn process '{process_name}'")]
    SpawnFailed {
        /// The name or description of the process being spawned.
        process_name: Cow<'static, str>,
        /// The underlying IO error.
        #[source]
        source: io::Error,
    },

    /// Failed to attach the spawned child to a Windows Job Object.
    ///
    /// On Windows, the crate creates an anonymous Job Object after spawn and assigns the child to
    /// it so that [`crate::ProcessHandle::kill`] (and the forceful-kill fallback inside
    /// [`crate::ProcessHandle::terminate`]) can reach the entire process tree via
    /// `TerminateJobObject`. If the post-spawn `CreateJobObjectW`, `OpenProcess`, or
    /// `AssignProcessToJobObject` call fails, the just-spawned child is killed and the spawn is
    /// rejected with this error rather than handed back without process-tree termination
    /// guarantees.
    #[cfg(windows)]
    #[error("Failed to attach spawned process '{process_name}' to a Windows Job Object")]
    JobAttachmentFailed {
        /// The name or description of the process being spawned.
        process_name: Cow<'static, str>,
        /// The underlying IO error from the failed `CreateJobObjectW`, `OpenProcess`, or
        /// `AssignProcessToJobObject` call.
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
#[error("Could not read from stream '{stream_name}'")]
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use assertr::prelude::*;

    pub(crate) fn assert_attempt_error(
        attempt_error: &TerminationAttemptError,
        expected_action: TerminationAction,
        expected_kind: io::ErrorKind,
        expected_message: &str,
    ) {
        assert_that!(attempt_error.action).is_equal_to(expected_action);

        let io_error = attempt_error
            .source
            .downcast_ref::<io::Error>()
            .expect("diagnostic should preserve the original io::Error");

        assert_that!(io_error.kind()).is_equal_to(expected_kind);
        assert_that!(io_error.to_string().as_str()).contains(expected_message);
    }
}
