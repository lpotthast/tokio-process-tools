use std::process::ExitStatus;

/// Full output of a process that terminated.
///
/// `Stdout` and `Stderr` describe the collected payload type for each stream. For example,
/// bounded line collection uses `ProcessOutput<CollectedLines>`, bounded raw byte collection uses
/// `ProcessOutput<CollectedBytes>`, and trusted unbounded collection uses `ProcessOutput<Vec<_>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput<Stdout, Stderr = Stdout> {
    /// Status the process exited with.
    pub status: ExitStatus,

    /// The process's collected output on its `stdout` stream.
    pub stdout: Stdout,

    /// The process's collected output on its `stderr` stream.
    pub stderr: Stderr,
}
