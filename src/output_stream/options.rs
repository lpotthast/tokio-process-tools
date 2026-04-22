/// Default chunk size read from the source stream. 16 kilobytes.
pub const DEFAULT_READ_CHUNK_SIZE: NumBytes = NumBytes(16 * 1024); // 16 kb

/// Default maximum buffered chunks for stdout and stderr streams. 128 slots.
pub const DEFAULT_MAX_BUFFERED_CHUNKS: usize = 128;

pub(crate) fn assert_max_buffered_chunks_non_zero(chunks: usize, parameter_name: &str) {
    assert!(chunks > 0, "{parameter_name} must be greater than zero");
}

/// A wrapper type representing a number of bytes.
///
/// Use the [`NumBytesExt`] trait to conveniently create instances:
/// ```
/// use tokio_process_tools::NumBytesExt;
/// let kb = 16.kilobytes();
/// let mb = 2.megabytes();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumBytes(pub(crate) usize);

impl NumBytes {
    /// Creates a `NumBytes` value of zero.
    #[must_use]
    pub fn zero() -> Self {
        Self(0)
    }

    pub(crate) fn assert_non_zero(self, parameter_name: &str) {
        assert!(
            self.0 > 0,
            "{parameter_name} must be greater than zero bytes"
        );
    }

    /// The amount of bytes represented by this instance.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.0
    }
}

/// Extension trait providing convenience-functions for creation of [`NumBytes`] of certain sizes.
pub trait NumBytesExt {
    /// Interprets the value as literal bytes.
    fn bytes(self) -> NumBytes;

    /// Interprets the value as kilobytes (value * 1024).
    fn kilobytes(self) -> NumBytes;

    /// Interprets the value as megabytes (value * 1024 * 1024).
    fn megabytes(self) -> NumBytes;
}

impl NumBytesExt for usize {
    fn bytes(self) -> NumBytes {
        NumBytes(self)
    }

    fn kilobytes(self) -> NumBytes {
        NumBytes(self * 1024)
    }

    fn megabytes(self) -> NumBytes {
        NumBytes(self * 1024 * 1024)
    }
}
