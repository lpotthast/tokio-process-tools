//! Error types for process operations.

use std::borrow::Cow;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

use crate::CollectorError;

/// Errors that can occur when terminating a process.
#[derive(Debug, Error)]
pub enum TerminationError {
    /// Failed to send a signal to the process.
    #[error("Failed to send '{signal}' signal to process '{process_name}': {source}")]
    SignallingFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The underlying IO error.
        source: io::Error,
        /// The signal that could not be sent.
        signal: &'static str,
    },

    /// Failed to terminate the process after trying all platform termination signals.
    #[error(
        "Failed to terminate process '{process_name}'. Interrupt failed: {sigint_error}. Terminate failed: {sigterm_error}. Kill failed: {sigkill_error}"
    )]
    TerminationFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// Error from the interrupt attempt.
        sigint_error: String,
        /// Error from the terminate attempt.
        sigterm_error: String,
        /// Error from the kill attempt.
        #[source]
        sigkill_error: io::Error,
    },
}

/// Errors that can occur when waiting for process operations.
#[derive(Debug, Error)]
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

    /// Wait operation timed out.
    #[error("Process '{process_name}' did not complete within {timeout:?}")]
    Timeout {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },
}

/// Errors that can occur when waiting for a process with automatic termination on failure.
#[derive(Debug, Error)]
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
}

/// Errors that can occur when waiting for a process while collecting its output.
#[derive(Debug, Error)]
pub enum WaitForCompletionWithOutputError {
    /// Waiting for the process failed.
    #[error("Waiting for process completion failed: {0}")]
    WaitFailed(#[from] WaitError),

    /// Collecting stdout or stderr failed.
    #[error("Collector failed to collect output: {0}")]
    OutputCollectionFailed(#[from] CollectorError),
}

/// Errors that can occur when waiting for a process with automatic termination while collecting
/// its output.
#[derive(Debug, Error)]
pub enum WaitForCompletionWithOutputOrTerminateError {
    /// Waiting with automatic termination failed.
    #[error("Wait-or-terminate operation failed: {0}")]
    WaitOrTerminateFailed(#[from] WaitOrTerminateError),

    /// Collecting stdout or stderr failed.
    #[error("Collector failed to collect output: {0}")]
    OutputCollectionFailed(#[from] CollectorError),
}

/// Errors that can occur when spawning a process.
#[derive(Debug, Error)]
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
///
/// Note that not every function returning this enum can produce every variant:
/// [`crate::broadcast::BroadcastOutputStream::wait_for_line`] and
/// [`crate::single_subscriber::SingleSubscriberOutputStream::wait_for_line`]
/// never return [`WaitForLineResult::Timeout`]. The timeout variant is only produced by the
/// corresponding `*_with_timeout` methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitForLineResult {
    /// A matching line was observed before the stream ended or the timeout elapsed.
    Matched,

    /// The stream ended before any matching line was observed.
    StreamClosed,

    /// The timeout elapsed before a matching line was observed or the stream ended.
    Timeout,
}
