//! Error types for process operations.

use std::borrow::Cow;
use std::io;
use std::time::Duration;
use thiserror::Error;

use crate::{CollectorError, InspectorError};

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

    /// Failed to terminate the process after trying all signals (SIGINT, SIGTERM, SIGKILL).
    #[error(
        "Failed to terminate process '{process_name}'. SIGINT failed: {sigint_error}. SIGTERM failed: {sigterm_error}. SIGKILL failed: {sigkill_error}"
    )]
    TerminationFailed {
        /// The name of the process.
        process_name: Cow<'static, str>,
        /// Error from SIGINT attempt.
        sigint_error: String,
        /// Error from SIGTERM attempt.
        sigterm_error: String,
        /// Error from SIGKILL attempt.
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

    /// Could not terminate the process.
    #[error("Could not terminate process: {0}")]
    TerminationError(#[from] TerminationError),

    /// Collector failed to collect output.
    #[error("Collector failed to collect output: {0}")]
    CollectorFailed(#[from] CollectorError),
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

/// Errors that can occur when waiting for output patterns.
#[derive(Debug, Error)]
pub enum OutputError {
    /// Waiting for output pattern timed out.
    #[error("Timed out after {timeout:?} waiting for output pattern on {stream_name}")]
    Timeout {
        /// The name of the stream.
        stream_name: &'static str,
        /// The timeout duration that was exceeded.
        timeout: Duration,
    },

    /// Inspector task failed.
    #[error("Inspector failed: {0}")]
    InspectorFailed(#[from] InspectorError),
}
