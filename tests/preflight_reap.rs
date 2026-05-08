#![cfg(any(unix, windows))]
#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::io;
use std::time::Duration;

#[cfg(unix)]
#[tokio::test]
async fn preflight_status_error_does_not_stop_termination_escalation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_process_tools::{UnixGracefulPhase, UnixGracefulShutdown, UnixGracefulSignal};

    let mut process = spawn_long_running_process();
    let interrupt_attempted = Arc::new(AtomicBool::new(false));
    let interrupt_attempted_in_sender = Arc::clone(&interrupt_attempted);
    let terminate_attempted = Arc::new(AtomicBool::new(false));
    let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

    let sequence = UnixGracefulShutdown::from_phases([
        UnixGracefulPhase::interrupt(Duration::from_millis(10)),
        UnixGracefulPhase::terminate(Duration::from_millis(10)),
    ]);

    let outcome = process
        .terminate_with_hooks(
            &sequence,
            |_| Err(io::Error::other("injected preflight status failure")),
            move |_, signal| match signal {
                UnixGracefulSignal::Interrupt => {
                    interrupt_attempted_in_sender.store(true, Ordering::SeqCst);
                    Err(io::Error::other("injected interrupt signal failure"))
                }
                UnixGracefulSignal::Terminate => {
                    terminate_attempted_in_sender.store(true, Ordering::SeqCst);
                    Err(io::Error::other("injected terminate signal failure"))
                }
                _ => Err(io::Error::other("unexpected graceful signal")),
            },
        )
        .await
        .unwrap();

    assert_that!(interrupt_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[cfg(windows)]
#[tokio::test]
async fn preflight_status_error_does_not_stop_termination_escalation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_process_tools::WindowsGracefulShutdown;

    let mut process = spawn_long_running_process();
    let graceful_attempted = Arc::new(AtomicBool::new(false));
    let graceful_attempted_in_sender = Arc::clone(&graceful_attempted);

    let sequence = WindowsGracefulShutdown::new(Duration::from_millis(10));

    let outcome = process
        .terminate_with_hooks(
            &sequence,
            |_| Err(io::Error::other("injected preflight status failure")),
            move |_| {
                graceful_attempted_in_sender.store(true, Ordering::SeqCst);
                Err(io::Error::other("injected graceful signal failure"))
            },
        )
        .await
        .unwrap();

    assert_that!(graceful_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}
