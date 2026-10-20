use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use tokio::runtime::{Handle, RuntimeFlavor};

/// Why [`try_run_future`] could not run its future on the current thread.
///
/// Returned to the caller so it can fall back to synchronous cleanup and report a precise reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnsupportedContext {
    /// The current thread is unwinding from a panic. The runtime probes or the future itself could
    /// panic, and a second panic during an unwind aborts the process, which is far worse than
    /// skipping the async path.
    Panicking,

    /// No tokio runtime is entered on the current thread, so there is no handle to drive the
    /// future with. A runtime may still be alive on other threads, but `Handle::try_current` can
    /// only discover the one entered on the current thread.
    NoRuntime,

    /// The tokio runtime entered on the current thread is single-threaded. `block_in_place` needs
    /// other workers to hand the thread's tasks to, so it cannot be used here.
    CurrentThreadRuntime,

    /// The tokio runtime entered on the current thread is shutting down. Its drivers no longer
    /// wake futures, so blocking on one could hang forever, and with it the shutdown itself.
    /// This is the case when the future's owner is dropped as part of the runtime shutdown, e.g.
    /// as the state of a task that is cancelled because the runtime is dropped.
    RuntimeShuttingDown,
}

impl UnsupportedContext {
    /// Describes where the drop happened, phrased to follow "dropped", e.g. in a log message.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Panicking => "while the thread was unwinding from a panic",
            Self::NoRuntime => "outside a tokio runtime context",
            Self::CurrentThreadRuntime => "inside a current-thread tokio runtime",
            Self::RuntimeShuttingDown => "while its tokio runtime was shutting down",
        }
    }
}

/// Drives `future` to completion from synchronous code, typically a `Drop` implementation.
///
/// Returns `Ok(())` once the future has completed. If the current thread cannot drive it, returns
/// the reason as `Err` and drops the future without polling it, leaving the caller to fall back to
/// synchronous cleanup. These checks never panic, but a panic from `future` or Tokio itself still
/// propagates to the caller.
///
/// # How it works
///
/// 1. We are typically in a Drop implementation. Drop is synchronous and cannot be async, but we
///    still need to execute an async operation from it.
/// 2. `block_on` runs an async operation to completion synchronously, which is how we can drive
///    it from a synchronous context.
/// 3. `block_on` on its own is not safe to call from within an async context, since the current
///    task may hold resources another task on the same worker is waiting for, leading to a
///    deadlock.
/// 4. `block_in_place` tells Tokio: "I am about to block this thread, please make sure other
///    threads are available to keep processing tasks." It moves other tasks to another worker,
///    so the current thread is free to be blocked for an unknown amount of time.
/// 5. `block_in_place` requires being inside a multi-threaded tokio runtime. Single-threaded
///    runtimes have no other workers to migrate tasks to, and a thread outside any runtime
///    context has no `Handle` to call `block_on` on in the first place. Both cases are reported
///    as `Err` instead of being attempted.
/// 6. The runtime must also still be running. Once it shuts down, its drivers stop waking
///    futures, so `block_on` would never return and the runtime shutdown, which waits for this
///    thread, would hang with it. This case is reported as `Err` as well.
pub(crate) fn try_run_future(future: impl Future<Output = ()>) -> Result<(), UnsupportedContext> {
    if std::thread::panicking() {
        return Err(UnsupportedContext::Panicking);
    }

    let Ok(handle) = Handle::try_current() else {
        return Err(UnsupportedContext::NoRuntime);
    };

    if matches!(handle.runtime_flavor(), RuntimeFlavor::CurrentThread) {
        return Err(UnsupportedContext::CurrentThreadRuntime);
    }

    if is_shutting_down(&handle) {
        return Err(UnsupportedContext::RuntimeShuttingDown);
    }

    tokio::task::block_in_place(|| handle.block_on(future));
    Ok(())
}

/// Returns whether the runtime behind `handle` has started shutting down.
///
/// Tokio offers no direct API for this, so this spawns a no-op probe task and polls its
/// `JoinHandle` once without blocking. A runtime that is shutting down cancels newly spawned tasks
/// right away, while a running runtime never cancels a task that was not aborted. The probe is
/// polled unconstrained so that an exhausted cooperative budget cannot hide the cancellation.
///
/// # Limitations
///
/// - The answer can be outdated as soon as it is returned, because a shutdown may start right
///   after the probe. This is harmless for the case the probe exists for. Blocking only hangs
///   forever when the drop runs as part of the shutdown itself, i.e. while a worker drops the
///   cancelled tasks: the shutdown then waits for the very thread that is waiting for the
///   runtime. Tokio closes its task list before dropping any task, so in that case the shutdown
///   has always started before the probe runs, and the probe reliably sees it.
/// - If a shutdown instead starts while an earlier `block_on` is already waiting, it does not
///   hang. The blocked thread has handed its worker over via `block_in_place`, so the shutdown
///   completes without it, and the runtime's timers then fail. The future panics with Tokio's
///   "being shutdown" message, and the panic unwinds out of the caller's `Drop`.
/// - That a task spawned during shutdown is cancelled immediately is observed Tokio behavior,
///   not a documented guarantee. The `reports_runtime_shutting_down_and_drops_future_unexecuted`
///   test pins it, so a Tokio release that changes it fails the test suite instead of silently
///   bringing back the hang.
/// - The probe is a real task. It is allocated, scheduled onto a worker, and visible to runtime
///   metrics and task instrumentation. It is only spawned right before a graceful shutdown is
///   driven, which costs far more.
fn is_shutting_down(handle: &Handle) -> bool {
    let probe = pin!(tokio::task::unconstrained(handle.spawn(async {})));
    let mut cx = Context::from_waker(Waker::noop());
    matches!(probe.poll(&mut cx), Poll::Ready(Err(err)) if err.is_cancelled())
}

#[cfg(test)]
mod tests {
    mod try_run_future {
        use crate::async_drop::{UnsupportedContext, try_run_future};
        use assertr::prelude::*;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[test]
        fn reports_no_runtime_and_drops_future_unexecuted_without_runtime() {
            let executed = AtomicBool::new(false);

            let result = try_run_future(async {
                executed.store(true, Ordering::SeqCst);
            });

            assert_that!(result)
                .err()
                .is_equal_to(UnsupportedContext::NoRuntime);
            assert_that!(executed.load(Ordering::SeqCst)).is_false();
        }

        #[tokio::test(flavor = "current_thread")]
        async fn reports_current_thread_runtime_err_and_drops_future_unexecuted() {
            let executed = AtomicBool::new(false);

            let result = try_run_future(async {
                executed.store(true, Ordering::SeqCst);
            });

            assert_that!(result)
                .err()
                .is_equal_to(UnsupportedContext::CurrentThreadRuntime);
            assert_that!(executed.load(Ordering::SeqCst)).is_false();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn executes_future_to_completion_on_multithreaded_runtime() {
            let executed = AtomicBool::new(false);

            let result = try_run_future(async {
                executed.store(true, Ordering::SeqCst);
            });

            assert_that!(result).is_ok();
            assert_that!(executed.load(Ordering::SeqCst)).is_true();
        }

        #[test]
        fn reports_runtime_shutting_down_and_drops_future_unexecuted() {
            use std::sync::{Arc, Mutex};
            use std::time::Duration;

            struct ProbeOnDrop {
                executed: Arc<AtomicBool>,
                observed: Arc<Mutex<Option<Result<(), UnsupportedContext>>>>,
            }
            impl Drop for ProbeOnDrop {
                fn drop(&mut self) {
                    let executed = Arc::clone(&self.executed);
                    let result = try_run_future(async move {
                        executed.store(true, Ordering::SeqCst);
                    });
                    *self.observed.lock().unwrap() = Some(result);
                }
            }

            let executed = Arc::new(AtomicBool::new(false));
            let observed = Arc::new(Mutex::new(None));
            let probe = ProbeOnDrop {
                executed: Arc::clone(&executed),
                observed: Arc::clone(&observed),
            };

            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.spawn(async move {
                let _probe = probe;
                std::future::pending::<()>().await;
            });
            // Let a worker start the task, so the probe is dropped by the shutdown below.
            rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
            drop(rt);

            assert_that!(*observed.lock().unwrap())
                .some()
                .err()
                .is_equal_to(UnsupportedContext::RuntimeShuttingDown);
            assert_that!(executed.load(Ordering::SeqCst)).is_false();
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn propagates_panic_from_future_on_multithreaded_runtime() {
            assert_that!(|| try_run_future(async { panic!("injected future panic") })).panics();
        }

        #[test]
        fn reports_panicking_without_double_panic_while_thread_is_unwinding() {
            use std::cell::Cell;

            struct ProbeOnDrop<'a>(&'a Cell<Option<Result<(), UnsupportedContext>>>);
            impl Drop for ProbeOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.set(Some(try_run_future(async {})));
                }
            }

            let observed = Cell::new(None);

            assert_that!(|| {
                let _probe = ProbeOnDrop(&observed);
                panic!("intentional test panic");
            })
            .panics();

            assert_that!(observed.get())
                .some()
                .err()
                .is_equal_to(UnsupportedContext::Panicking);
        }
    }
}
