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
    /// The largest representable byte count (`usize::MAX`).
    ///
    /// Use this when an API requires an explicit byte limit but you don't actually want to cap
    /// it — for example, line-parsing options that require a non-zero `max_line_length` but you
    /// trust the source to produce reasonable lines.
    pub const MAX: NumBytes = NumBytes(usize::MAX);

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

/// Extension trait providing convenience constructors for [`NumBytes`].
///
/// Implemented for `usize`, so integer literals can be used directly:
///
/// ```
/// use tokio_process_tools::{NumBytes, NumBytesExt};
///
/// let small: NumBytes = 512.bytes();
/// let medium: NumBytes = 16.kilobytes();
/// let large: NumBytes = 2.megabytes();
///
/// assert_eq!(small.bytes(), 512);
/// assert_eq!(medium.bytes(), 16 * 1024);
/// assert_eq!(large.bytes(), 2 * 1024 * 1024);
/// ```
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
