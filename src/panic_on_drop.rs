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
        if self.is_armed() {
            let backtrace = backtrace::Backtrace::capture();
            panic!(
                "Resource '{}' should not have been dropped at this point!\n\nErr: {}\n\nHelp: {}\n\nBacktrace: {:#?}",
                self.resource_name, self.error_msg, self.help_msg, backtrace,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assertr::assert_that_type;
    use assertr::prelude::*;
    use std::panic;
    use std::sync::Mutex;

    #[test]
    fn needs_drop() {
        assert_that_type::<PanicOnDrop>().needs_drop();
    }

    /// Serialize panic-hook manipulation so concurrent tests don't see each other's silenced hook.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    fn run_silently(f: impl FnOnce() + std::panic::UnwindSafe) -> std::thread::Result<()> {
        let _guard = PANIC_HOOK_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev_hook = panic::take_hook();
        panic::set_hook(Box::new(|_info| {}));
        let result = panic::catch_unwind(f);
        panic::set_hook(prev_hook);
        result
    }

    #[test]
    fn armed_drop_panics_with_expected_message() {
        let result = run_silently(|| {
            let _guard = PanicOnDrop::new("ResourceX", "must be defused", "call defuse()");
        });

        let payload = result.expect_err("dropping an armed PanicOnDrop must panic");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&'static str>().copied())
            .expect("panic payload should be a string");

        assert_that!(message).contains("ResourceX");
        assert_that!(message).contains("must be defused");
        assert_that!(message).contains("call defuse()");
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
}
