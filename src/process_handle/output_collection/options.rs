use crate::{LineCollectionOptions, RawCollectionOptions};
use std::time::Duration;

/// Default grace period for output consumers to observe EOF after process termination.
pub const DEFAULT_OUTPUT_EOF_TIMEOUT: Duration = Duration::from_secs(3);

/// Options for line output collection from stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineOutputOptions {
    /// Collection options used for stdout.
    pub stdout_collection_options: LineCollectionOptions,

    /// Collection options used for stderr.
    pub stderr_collection_options: LineCollectionOptions,
}

impl LineOutputOptions {
    /// Use the same [`LineCollectionOptions`] for both stdout and stderr.
    #[must_use]
    pub const fn symmetric(options: LineCollectionOptions) -> Self {
        Self {
            stdout_collection_options: options,
            stderr_collection_options: options,
        }
    }
}

/// Options for raw byte output collection from stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawOutputOptions {
    /// Collection options used for stdout.
    pub stdout_collection_options: RawCollectionOptions,

    /// Collection options used for stderr.
    pub stderr_collection_options: RawCollectionOptions,
}

impl RawOutputOptions {
    /// Use the same [`RawCollectionOptions`] for both stdout and stderr.
    #[must_use]
    pub const fn symmetric(options: RawCollectionOptions) -> Self {
        Self {
            stdout_collection_options: options,
            stderr_collection_options: options,
        }
    }
}
