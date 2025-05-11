use std::backtrace;
use std::borrow::Cow;

#[derive(Debug)]
pub struct PanicOnDrop {
    pub(crate) resource_name: Cow<'static, str>,
    pub(crate) details: Cow<'static, str>,
    pub(crate) armed: bool,
}

impl PanicOnDrop {
    pub(crate) fn defuse(&mut self) {
        self.armed = false;
    }
}

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let backtrace = backtrace::Backtrace::capture();
            panic!(
                "{} must be terminated before being dropped: {}\n\nBacktrace: {:#?}",
                self.resource_name, self.details, backtrace,
            );
        }
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
}
