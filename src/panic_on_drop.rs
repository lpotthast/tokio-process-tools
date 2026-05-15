use std::backtrace;
use std::borrow::Cow;

/// A type raising a panic when dropped.
///
/// Call [`PanicOnDrop::defuse`] to prevent the panic.
#[derive(Debug)]
pub struct PanicOnDrop {
    /// Name of the original resource that encloses this type.
    resource_name: Cow<'static, str>,
    /// Descriptive error, telling the user why the enclosing type should not have been dropped.
    error_msg: Cow<'static, str>,
    /// Descriptive message, telling the user what can, should or must be done to prevent the panic.
    help_msg: Cow<'static, str>,
    /// Internal flag to control the panic raise.
    armed: bool,
}

impl PanicOnDrop {
    /// Creates a new `PanicOnDrop` instance, raising a panic when dropped.
    ///
    /// This panic can only be prevented when [`PanicOnDrop::defuse`] was called before this type is
    /// dropped.
    pub fn new(
        resource_name: impl Into<Cow<'static, str>>,
        error_msg: impl Into<Cow<'static, str>>,
        help_msg: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            resource_name: resource_name.into(),
            error_msg: error_msg.into(),
            help_msg: help_msg.into(),
            armed: true,
        }
    }

    /// Calling this prevents the panic from being raised when this type is dropped.
    pub fn defuse(&mut self) {
        self.armed = false;
    }

    /// When armed, a panic is raised when dropped.
    pub fn is_armed(&self) -> bool {
        self.armed
    }
}

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        if !self.is_armed() {
            return;
        }
        let backtrace = backtrace::Backtrace::capture();
        let message = format!(
            "Resource '{}' should not have been dropped at this point!\n\nErr: {}\n\nHelp: {}\n\nBacktrace: {:#?}",
            self.resource_name, self.error_msg, self.help_msg, backtrace,
        );
        // If the current thread is already unwinding from an earlier panic, raising a second one
        // from this Drop impl would `abort` the process. That could hide the original failure and
        // potentially crashes test binaries instead of letting the test framework report the first
        // panic cleanly (e.g. a panic raised by a failed assertion).
        // Emit a warning so the cleanup-omission is still surfaced, then leave unwinding to the
        // panic that's already in flight.
        if std::thread::panicking() {
            tracing::warn!(message);
            tracing::warn!("Suppressing PanicOnDrop because the thread is already panicking");
            return;
        }
        panic!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::assert_that_type;
    use assertr::prelude::*;

    #[test]
    fn needs_drop() {
        assert_that_type::<PanicOnDrop>().needs_drop();
    }

    #[test]
    fn armed_drop_panics_with_expected_message() {
        assert_that_panic_by(|| {
            let _guard = PanicOnDrop::new("ResourceX", "must be defused", "call defuse()");
        })
        .has_type::<String>()
        .contains("ResourceX")
        .contains("must be defused")
        .contains("call defuse()");
    }

    #[test]
    fn defused_drop_does_not_panic() {
        let mut guard = PanicOnDrop::new("ResourceX", "err", "help");
        guard.defuse();
        assert_that!(guard.is_armed()).is_false();
        // Drop happens at end of scope; the test passes by not panicking.
    }

    #[test]
    fn defuse_is_idempotent() {
        let mut guard = PanicOnDrop::new("ResourceX", "err", "help");
        guard.defuse();
        guard.defuse();
        assert_that!(guard.is_armed()).is_false();
    }

    #[test]
    fn drop_during_panic_suppresses_secondary_panic() {
        // If this test reached a double-panic, the process would `abort` via Rust's no-double-panic
        // rule and `cargo test` would exit non-zero with no FAILED line. Reaching the assertions
        // below is itself part of the contract: only the first panic must escape.
        assert_that_panic_by(|| {
            let _guard = PanicOnDrop::new("ResourceX", "must be defused", "call defuse()");
            panic!("first panic, the guard drops while unwinding from this");
        })
        .has_type::<&str>()
        .is_equal_to("first panic, the guard drops while unwinding from this");
    }
}
