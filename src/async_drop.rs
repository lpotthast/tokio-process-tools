use std::future::Future;
use tokio::runtime::{Handle, RuntimeFlavor};

/// Enables executing async operations within synchronous Drop implementations.
///
/// # Safety requirements
///
/// **WARNING**: This function requires a multithreaded tokio runtime to function correctly!
///
/// # How it works
///
/// 1. We are typically in a Drop implementation which is synchronous, it can't be async,
///    but we still need to execute an async operation.
/// 2. `block_on` runs an async operation to completion synchronously, which is how we can drive
///    it from a synchronous context.
/// 3. `block_on` on its own is not safe to call from within an async context, since the current
///    task may hold resources another task on the same worker is waiting for, leading to a
///    deadlock.
/// 4. `block_in_place` tells Tokio: "I am about to block this thread, please make sure other
///    threads are available to keep processing tasks." It moves other tasks to another worker,
///    so the current thread is free to be blocked for an unknown amount of time.
/// 5. `block_in_place` requires a multi-threaded tokio runtime. Single-threaded runtimes do not
///    have other workers to migrate tasks to, so the call is rejected at runtime.
/// 6. `block_in_place` waits indefinitely for the closure body to complete when the executor is
///    being shut down, which is what makes it safe to use during drop on an active runtime.
///
/// # Example
///
/// ```ignore
/// use crate::async_drop::run_future;
///
/// struct PerformsAsyncDrop {
///     // ... fields
/// }
///
/// impl Drop for PerformsAsyncDrop {
///     fn drop(&mut self) {
///         run_future(async {
///             // Perform async cleanup operations here
///         });
///     }
/// }
/// ```
pub(crate) fn run_future(future: impl Future<Output = ()>) {
    // If we are already panicking, do not risk a double-panic from any of the runtime checks
    // below. A double-panic during unwind aborts the process, which is far worse than skipping a
    // best-effort cleanup attempt. This branch trades cleanup for not making a bad situation
    // catastrophic.
    if std::thread::panicking() {
        return;
    }

    let Ok(handle) = Handle::try_current() else {
        panic!(
            "TerminateOnDrop requires a tokio runtime to be active when the handle drops. \
             No runtime was found in the current thread context. Drop the handle from inside a \
             #[tokio::main] or #[tokio::test(flavor = \"multi_thread\")] context, or use \
             `must_not_be_terminated()` to opt out of automatic termination."
        )
    };

    if matches!(handle.runtime_flavor(), RuntimeFlavor::CurrentThread) {
        panic!(
            "TerminateOnDrop requires a multi-threaded tokio runtime to function correctly. \
             The current runtime is single-threaded, which cannot drive `block_in_place`. \
             Switch to `#[tokio::test(flavor = \"multi_thread\")]` or build the runtime with \
             `tokio::runtime::Builder::new_multi_thread()`."
        );
    }

    tokio::task::block_in_place(|| handle.block_on(future));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_silently<R>(f: impl FnOnce() -> R + std::panic::UnwindSafe) -> std::thread::Result<R> {
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_info| {}));
        let result = std::panic::catch_unwind(f);
        std::panic::set_hook(prev_hook);
        result
    }

    #[test]
    fn missing_runtime_panic_names_terminate_on_drop() {
        let payload = run_silently(|| run_future(async {})).expect_err("should panic");
        let message = payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        assert!(
            message.contains("TerminateOnDrop"),
            "panic message should name TerminateOnDrop, got: {message}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn single_threaded_runtime_panic_names_terminate_on_drop() {
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_future(async {});
        }));
        let payload = payload.expect_err("should panic on single-threaded runtime");
        let message = payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("");
        assert!(
            message.contains("TerminateOnDrop"),
            "panic message should name TerminateOnDrop, got: {message}"
        );
    }
}
