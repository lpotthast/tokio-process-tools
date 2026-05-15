#![cfg(any(unix, windows))]
#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::io;
use std::time::Duration;
use tokio_process_tools::{
    DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt, Process, ProcessHandle,
    WindowsGracefulShutdown,
};

#[cfg(unix)]
use tokio_process_tools::{UnixGracefulPhase, UnixGracefulShutdown, UnixGracefulSignal};

#[tokio::test(flavor = "multi_thread")]
async fn terminate_future_can_be_spawned_on_tokio_task() {
    let mut process = spawn_long_running_process();

    let result = tokio::spawn(async move { process.terminate(default_graceful_shutdown()).await })
        .await
        .unwrap();

    assert_that!(result).is_ok();
}

#[cfg(unix)]
#[tokio::test]
async fn terminate_falls_back_to_kill_when_graceful_signal_sends_fail() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut process = spawn_long_running_process();
    let terminate_attempted = Arc::new(AtomicBool::new(false));
    let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

    let sequence = UnixGracefulShutdown::from_phases([
        UnixGracefulPhase::interrupt(Duration::from_millis(10)),
        UnixGracefulPhase::terminate(Duration::from_millis(10)),
    ]);

    let outcome = process
        .terminate_with_hooks(
            &sequence,
            ProcessHandle::try_reap_exit_status,
            move |_, signal| match signal {
                UnixGracefulSignal::Interrupt => {
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

    assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[cfg(windows)]
#[tokio::test]
async fn terminate_falls_back_to_kill_when_graceful_signal_send_fails() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut process = spawn_long_running_process();
    let graceful_attempted = Arc::new(AtomicBool::new(false));
    let graceful_attempted_in_sender = Arc::clone(&graceful_attempted);

    let sequence = WindowsGracefulShutdown::new(Duration::from_millis(10));

    let outcome = process
        .terminate_with_hooks(&sequence, ProcessHandle::try_reap_exit_status, move |_| {
            graceful_attempted_in_sender.store(true, Ordering::SeqCst);
            Err(io::Error::other("injected graceful signal failure"))
        })
        .await
        .unwrap();

    assert_that!(graceful_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn terminate_succeeds_when_graceful_phase_succeeds() {
    let mut process = spawn_long_running_process();

    let outcome = process
        .terminate(default_graceful_shutdown())
        .await
        .unwrap();

    assert_that!(outcome.success()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[cfg(unix)]
#[tokio::test]
async fn canceled_terminate_keeps_drop_guards_armed() {
    let mut process = spawn_long_running_process();

    let sequence = UnixGracefulShutdown::from_phases([
        UnixGracefulPhase::interrupt(Duration::from_secs(5)),
        UnixGracefulPhase::terminate(Duration::from_secs(5)),
    ]);

    let result =
        tokio::time::timeout(
            Duration::from_millis(50),
            process.terminate_with_hooks(&sequence, ProcessHandle::try_reap_exit_status, |_, _| {
                Ok(())
            }),
        )
        .await;

    assert_that!(result.is_err()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn canceled_terminate_keeps_drop_guards_armed() {
    let mut process = spawn_long_running_process();

    let sequence = WindowsGracefulShutdown::new(Duration::from_secs(10));

    let result = tokio::time::timeout(
        Duration::from_millis(50),
        process.terminate_with_hooks(&sequence, ProcessHandle::try_reap_exit_status, |_| Ok(())),
    )
    .await;

    assert_that!(result.is_err()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[tokio::test]
async fn terminate_stops_process() {
    let started_at = jiff::Zoned::now();
    let mut handle = Process::new(signal_responsive_long_running_command())
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let exit_status = handle.terminate(default_graceful_shutdown()).await.unwrap();
    let terminated_at = jiff::Zoned::now();

    let ran_for = started_at.duration_until(&terminated_at);
    assert_that!(ran_for.as_secs_f32()).is_close_to(0.1, 0.5);

    // The helper installs a graceful-signal handler and exits via `process::exit(1)`, so the
    // child reports a clean exit with code 1 on both platforms.
    assert_that!(exit_status.code()).is_equal_to(Some(1));
    assert_that!(exit_status.success()).is_false();

    assert_that!(handle.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn terminate_returns_normal_exit_when_process_already_exited() {
    let mut handle = spawn_immediately_exiting_process();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let exit_status = handle.terminate(short_graceful_shutdown()).await.unwrap();

    assert_that!(exit_status.success()).is_true();
}

/// Regression test for the "signal send fails while the child has already exited" race.
///
/// Forces the preflight probe to miss the exit (`Ok(None)`) and the graceful signal sends to
/// fail. Without the bounded-wait recovery, this falls through to the kill phase and exits via
/// SIGKILL/`TerminateProcess`. With it, the 100 ms grace observes the natural exit and reports
/// success.
#[tokio::test]
async fn terminate_succeeds_when_signal_send_fails_against_recently_exited_child() {
    let mut handle = spawn_immediately_exiting_process();

    #[cfg(unix)]
    let outcome = {
        let sequence = UnixGracefulShutdown::from_phases([
            UnixGracefulPhase::interrupt(Duration::from_millis(50)),
            UnixGracefulPhase::terminate(Duration::from_millis(50)),
        ]);
        handle
            .terminate_with_hooks(
                &sequence,
                |_| Ok(None),
                |_, signal| match signal {
                    UnixGracefulSignal::Interrupt => {
                        Err(io::Error::other("injected interrupt signal-send failure"))
                    }
                    UnixGracefulSignal::Terminate => {
                        Err(io::Error::other("injected terminate signal-send failure"))
                    }
                    _ => Err(io::Error::other("unexpected graceful signal")),
                },
            )
            .await
            .unwrap()
    };
    #[cfg(windows)]
    let outcome = {
        let sequence = WindowsGracefulShutdown::new(Duration::from_millis(100));
        handle
            .terminate_with_hooks(
                &sequence,
                |_| Ok(None),
                |_| Err(io::Error::other("injected graceful signal-send failure")),
            )
            .await
            .unwrap()
    };

    assert_that!(outcome.success()).is_true();
    assert_that!(handle.is_drop_disarmed()).is_true();
}

#[cfg(unix)]
#[tokio::test]
async fn single_signal_sequence_dispatches_only_that_signal() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for expected in [UnixGracefulSignal::Terminate, UnixGracefulSignal::Interrupt] {
        let mut process = spawn_long_running_process();

        let interrupt_attempts = Arc::new(AtomicUsize::new(0));
        let terminate_attempts = Arc::new(AtomicUsize::new(0));
        let interrupt_attempts_in_sender = Arc::clone(&interrupt_attempts);
        let terminate_attempts_in_sender = Arc::clone(&terminate_attempts);

        let sequence = match expected {
            UnixGracefulSignal::Terminate => {
                UnixGracefulShutdown::terminate_only(Duration::from_millis(10))
            }
            UnixGracefulSignal::Interrupt => {
                UnixGracefulShutdown::interrupt_only(Duration::from_millis(10))
            }
            _ => unreachable!("unexpected graceful signal variant"),
        };

        let outcome = process
            .terminate_with_hooks(
                &sequence,
                ProcessHandle::try_reap_exit_status,
                move |_, signal| match signal {
                    UnixGracefulSignal::Interrupt => {
                        interrupt_attempts_in_sender.fetch_add(1, Ordering::SeqCst);
                        Err(io::Error::other("injected interrupt signal failure"))
                    }
                    UnixGracefulSignal::Terminate => {
                        terminate_attempts_in_sender.fetch_add(1, Ordering::SeqCst);
                        Err(io::Error::other("injected terminate signal failure"))
                    }
                    _ => Err(io::Error::other("unexpected graceful signal")),
                },
            )
            .await
            .unwrap();

        let (expected_interrupt, expected_terminate) = match expected {
            UnixGracefulSignal::Interrupt => (1, 0),
            UnixGracefulSignal::Terminate => (0, 1),
            _ => unreachable!("unexpected graceful signal variant"),
        };
        assert_that!(interrupt_attempts.load(Ordering::SeqCst))
            .with_detail_message(format!("interrupt count for {expected:?}"))
            .is_equal_to(expected_interrupt);
        assert_that!(terminate_attempts.load(Ordering::SeqCst))
            .with_detail_message(format!("terminate count for {expected:?}"))
            .is_equal_to(expected_terminate);
        assert_that!(outcome.success()).is_false();
        assert_that!(process.is_drop_disarmed()).is_true();
    }
}

/// Documents the upper-bound nature of per-phase timeouts: a child that has no signal handler
/// installed is killed immediately by the kernel's default disposition (`Term`), so a generous
/// 30s timeout never fires for it. This test would take 30 seconds to run if the timeout were
/// a fixed delay rather than an upper bound.
#[cfg(unix)]
#[tokio::test]
async fn handler_less_child_exits_in_milliseconds_under_long_timeout() {
    use tokio_process_tools::GracefulShutdown;

    let mut handle = spawn_long_running_process();

    let shutdown = GracefulShutdown::builder()
        .unix(UnixGracefulShutdown::terminate_only(Duration::from_secs(
            30,
        )))
        .windows(WindowsGracefulShutdown::new(Duration::from_secs(30)))
        .build();

    let started = std::time::Instant::now();
    let exit_status = handle.terminate(shutdown).await.unwrap();
    let elapsed = started.elapsed();

    assert_that!(elapsed < Duration::from_secs(2))
        .with_detail_message(format!(
            "handler-less child should exit within milliseconds, took {elapsed:?}"
        ))
        .is_true();
    assert_that!(exit_status.success()).is_false();
    assert_that!(handle.is_drop_disarmed()).is_true();
}

/// Regression test for the documented footgun. A two-phase `SIGINT` -> `SIGTERM` sequence does
/// NOT cover children that handle only `SIGTERM`. The kernel default disposition kills the
/// child during the `SIGINT` phase before the `SIGTERM` phase can dispatch, so the SIGTERM
/// handler that the user set up never runs.
///
/// We encode the footgun behavior via the signal-injection pattern: when SIGINT delivery
/// causes the child to exit (modeled by SIGINT "succeeding" and the wait observing exit), the
/// SIGTERM phase is never dispatched.
#[cfg(unix)]
#[tokio::test]
async fn two_phase_sigint_then_sigterm_does_not_dispatch_sigterm_when_child_exits_during_sigint_phase()
 {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut handle = spawn_long_running_process();

    let interrupt_attempts = Arc::new(AtomicUsize::new(0));
    let terminate_attempts = Arc::new(AtomicUsize::new(0));
    let interrupt_attempts_in_sender = Arc::clone(&interrupt_attempts);
    let terminate_attempts_in_sender = Arc::clone(&terminate_attempts);

    let sequence = UnixGracefulShutdown::from_phases([
        UnixGracefulPhase::interrupt(Duration::from_millis(50)),
        UnixGracefulPhase::terminate(Duration::from_millis(50)),
    ]);

    // SIGINT delivery actually reaches `sleep` (which has no SIGINT handler) and the kernel
    // default disposition kills it immediately. The wait future resolves before the timeout,
    // so the SIGTERM phase never fires - even though we explicitly configured one.
    let outcome = handle
        .terminate_with_hooks(
            &sequence,
            ProcessHandle::try_reap_exit_status,
            move |this, signal| {
                let pid = Pid::from_raw(
                    this.id()
                        .expect("process should still have a PID at signal time")
                        .cast_signed(),
                );
                match signal {
                    UnixGracefulSignal::Interrupt => {
                        interrupt_attempts_in_sender.fetch_add(1, Ordering::SeqCst);
                        killpg(pid, Signal::SIGINT)
                            .map_err(|err| io::Error::from_raw_os_error(err as i32))
                    }
                    UnixGracefulSignal::Terminate => {
                        terminate_attempts_in_sender.fetch_add(1, Ordering::SeqCst);
                        killpg(pid, Signal::SIGTERM)
                            .map_err(|err| io::Error::from_raw_os_error(err as i32))
                    }
                    _ => Err(io::Error::other("unexpected graceful signal")),
                }
            },
        )
        .await
        .unwrap();

    assert_that!(interrupt_attempts.load(Ordering::SeqCst)).is_equal_to(1);
    assert_that!(terminate_attempts.load(Ordering::SeqCst))
        .with_detail_message(
            "SIGTERM phase must not run when the child has already exited via the SIGINT phase \
             (this is the documented footgun: a SIGTERM-only-handler child gets killed during \
             phase 1 and the user's SIGTERM handler never runs)",
        )
        .is_equal_to(0);
    assert_that!(outcome.success()).is_false();
}
