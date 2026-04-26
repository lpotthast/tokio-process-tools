use std::time::Duration;

/// Options for waiting until a process exits, terminating it if waiting fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitForCompletionOrTerminateOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,
}
