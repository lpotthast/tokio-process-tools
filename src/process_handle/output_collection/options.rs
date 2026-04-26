use crate::{LineCollectionOptions, LineParsingOptions, RawCollectionOptions};

/// Options for line output collection from stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineOutputOptions {
    /// Options used for parsing stdout and stderr chunks into lines.
    pub line_parsing_options: LineParsingOptions,

    /// Collection options used for stdout.
    pub stdout_collection_options: LineCollectionOptions,

    /// Collection options used for stderr.
    pub stderr_collection_options: LineCollectionOptions,
}

/// Options for raw byte output collection from stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawOutputOptions {
    /// Collection options used for stdout.
    pub stdout_collection_options: RawCollectionOptions,

    /// Collection options used for stderr.
    pub stderr_collection_options: RawCollectionOptions,
}
