use crate::{LineCollectionOptions, LineParsingOptions, RawCollectionOptions};
use std::time::Duration;
use typed_builder::TypedBuilder;

/// Options for waiting until a process exits.
///
/// The builder requires the timeout to be set explicitly, even when no timeout is desired:
///
/// ```compile_fail
/// use tokio_process_tools::WaitForCompletionOptions;
///
/// let _ = WaitForCompletionOptions::builder().build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionOptions {
    /// Maximum time to wait, or `None` to wait without a timeout.
    pub timeout: Option<Duration>,
}

/// Options for waiting until a process exits while collecting bounded line output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionWithOutputOptions {
    /// Maximum time to wait, or `None` to wait without a timeout.
    pub timeout: Option<Duration>,

    /// Line output collection options.
    pub line_output_options: LineOutputOptions,
}

/// Options for waiting until a process exits while collecting trusted line output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionWithTrustedLineOutputOptions {
    /// Maximum time to wait, or `None` to wait without a timeout.
    pub timeout: Option<Duration>,

    /// Options used for parsing stdout and stderr chunks into lines.
    pub line_parsing_options: LineParsingOptions,
}

/// Options for waiting until a process exits while collecting bounded raw byte output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionWithRawOutputOptions {
    /// Maximum time to wait, or `None` to wait without a timeout.
    pub timeout: Option<Duration>,

    /// Raw output collection options.
    pub raw_output_options: RawOutputOptions,
}

/// Options for waiting until a process exits, terminating it if waiting fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionOrTerminateOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,
}

/// Options for waiting until a process exits while collecting bounded line output, terminating it
/// if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionOrTerminateWithOutputOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,

    /// Line output collection options.
    pub line_output_options: LineOutputOptions,
}

/// Options for waiting until a process exits while collecting trusted line output, terminating it
/// if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionOrTerminateWithTrustedLineOutputOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,

    /// Options used for parsing stdout and stderr chunks into lines.
    pub line_parsing_options: LineParsingOptions,
}

/// Options for waiting until a process exits while collecting bounded raw byte output,
/// terminating it if needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct WaitForCompletionOrTerminateWithRawOutputOptions {
    /// Maximum time to wait before attempting termination.
    pub wait_timeout: Duration,

    /// Maximum time to wait after sending the interrupt signal.
    pub interrupt_timeout: Duration,

    /// Maximum time to wait after sending the terminate signal.
    pub terminate_timeout: Duration,

    /// Raw output collection options.
    pub raw_output_options: RawOutputOptions,
}

/// Options for bounded line output collection from stdout and stderr.
///
/// The builder requires both per-stream collection settings:
///
/// ```compile_fail
/// use tokio_process_tools::{
///     CollectionOverflowBehavior, LineCollectionOptions, LineOutputOptions,
///     LineOverflowBehavior, LineParsingOptions, NumBytesExt,
/// };
///
/// let line_collection_options = LineCollectionOptions::builder()
///     .max_bytes(1.megabytes())
///     .max_lines(1024)
///     .overflow_behavior(CollectionOverflowBehavior::DropAdditionalData)
///     .build();
///
/// let _ = LineOutputOptions::builder()
///     .line_parsing_options(
///         LineParsingOptions::builder()
///             .max_line_length(16.kilobytes())
///             .overflow_behavior(LineOverflowBehavior::DropAdditionalData)
///             .build(),
///     )
///     .stdout_collection_options(line_collection_options)
///     .build();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct LineOutputOptions {
    /// Options used for parsing stdout and stderr chunks into lines.
    pub line_parsing_options: LineParsingOptions,

    /// Bounded collection options used for stdout.
    pub stdout_collection_options: LineCollectionOptions,

    /// Bounded collection options used for stderr.
    pub stderr_collection_options: LineCollectionOptions,
}

/// Options for bounded raw byte output collection from stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, TypedBuilder)]
pub struct RawOutputOptions {
    /// Bounded collection options used for stdout.
    pub stdout_collection_options: RawCollectionOptions,

    /// Bounded collection options used for stderr.
    pub stderr_collection_options: RawCollectionOptions,
}
