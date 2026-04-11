use std::process::ExitStatus;

/// Full output of a process that terminated.
///
/// Both it's `stdout` and `stderr` streams were collected as individual lines. Depending on the
/// [`crate::LineParsingOptions`] used, content might have been lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Status the process exited with.
    pub status: ExitStatus,

    /// The processes entire output on its `stdout` stream, collected into individual lines.
    ///
    /// Depending on the [`crate::LineParsingOptions`] used, content might have been lost.
    pub stdout: Vec<String>,

    /// The processes entire output on its `stderr` stream, collected into individual lines.
    ///
    /// Depending on the [`crate::LineParsingOptions`] used, content might have been lost.
    pub stderr: Vec<String>,
}

/// Full raw byte output of a process that terminated.
///
/// Both its `stdout` and `stderr` streams were collected as bytes without line parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawOutput {
    /// Status the process exited with.
    pub status: ExitStatus,

    /// The process's entire output on its `stdout` stream, collected as raw bytes.
    pub stdout: Vec<u8>,

    /// The process's entire output on its `stderr` stream, collected as raw bytes.
    pub stderr: Vec<u8>,
}
