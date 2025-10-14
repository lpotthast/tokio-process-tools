use std::future::Future;

/// Enables executing async operations within synchronous Drop implementations.
///
/// # Safety requirements
///
/// **WARNING**: This function requires a multithreaded tokio runtime to function correctly!
///
/// # How it works
///
/// 1. We are typically in a Drop implementation which is synchronous - it can't be async.
///    But we need to execute an async operation.
///
/// 2. `block_on` is needed because it takes an async operation and runs it to completion
///    synchronously - it's how we can execute async operations within synchronous contexts.
///
/// 3. However, block_on by itself isn't safe to call from within an async context
///    (which we are in since we're inside the Tokio runtime).
///    This is because it could lead to deadlocks - imagine if the current thread is needed to
///    process some task that our blocked async operation is waiting on.
///
/// 4. This is where `block_in_place` comes in - it tells Tokio:
///    "Hey, I'm about to block this thread, please make sure other threads are available to
///    still process tasks!". It essentially moves other tasks to another worker thread so that the
///    current thread becomes available to be blocked for an unknown amount of time.
///
/// 5. Note that `block_in_place` requires a multithreaded tokio runtime to be active!
///    So use `#[tokio::test(flavor = "multi_thread")]` in tokio-enabled tests.
///
/// 6. Also note that `block_in_place` enforces that the given closure is run to completion by
///    waiting indefinitely for it to finish when the async executor is terminated.
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
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(future);
    });
}
