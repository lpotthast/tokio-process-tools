#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;

#[tokio::test]
async fn must_be_terminated_is_idempotent_when_already_armed() {
    let mut process = spawn_long_running_process();

    process.must_be_terminated();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[tokio::test]
async fn must_be_terminated_re_arms_safeguards_after_opt_out() {
    let mut process = spawn_long_running_process();

    process.must_not_be_terminated();
    assert_that!(process.is_drop_disarmed()).is_true();

    process.must_be_terminated();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn must_not_be_terminated_lets_child_outlive_dropped_handle() {
    use nix::errno::Errno;
    use nix::sys::signal::{self, Signal};
    use nix::sys::wait::waitpid;
    use nix::unistd::Pid;

    let mut process = spawn_long_running_process();
    let pid = process.id().unwrap();

    process.must_not_be_terminated();
    assert_that!(process.is_drop_disarmed()).is_true();
    drop(process);

    let pid = Pid::from_raw(pid.cast_signed());
    assert_that!(signal::kill(pid, None)).is_ok();

    signal::kill(pid, Signal::SIGKILL).unwrap();
    assert_that!(waitpid(pid, None)).is_matching(pattern!(Ok(_) | Err(Errno::ECHILD)));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn must_not_be_terminated_lets_child_outlive_dropped_terminate_on_drop_wrapper() {
    use nix::errno::Errno;
    use nix::sys::signal::{self, Signal};
    use nix::sys::wait::waitpid;
    use nix::unistd::Pid;

    let mut guard = spawn_long_running_process().terminate_on_drop(short_graceful_shutdown());
    let pid = guard.id().unwrap();

    // Detaching through the wrapper must win over its terminate-on-drop behavior, even in a
    // multithreaded runtime where the graceful shutdown could run.
    guard.must_not_be_terminated();
    drop(guard);

    let pid = Pid::from_raw(pid.cast_signed());
    assert_that!(signal::kill(pid, None)).is_ok();

    signal::kill(pid, Signal::SIGKILL).unwrap();
    assert_that!(waitpid(pid, None)).is_matching(pattern!(Ok(_) | Err(Errno::ECHILD)));
}

#[cfg(unix)]
#[tokio::test]
async fn must_not_be_terminated_still_closes_stdin_on_drop() {
    use nix::errno::Errno;
    use nix::sys::wait::waitpid;
    use nix::unistd::Pid;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio_process_tools::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process};

    let temp_dir = tempdir().unwrap();
    let output_file = temp_dir.path().join("stdin-result.txt");
    let output_file = output_file.to_str().unwrap().replace('\'', "'\"'\"'");

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(format!("cat >/dev/null; printf eof > '{output_file}'"));

    let mut process = Process::new(cmd)
        .name("sh")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    let pid = Pid::from_raw(process.id().unwrap().cast_signed());

    process.must_not_be_terminated();
    drop(process);

    let wait_result = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || waitpid(pid, None)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_that!(wait_result).is_matching(pattern!(Ok(_) | Err(Errno::ECHILD)));

    assert_that!(fs::read_to_string(temp_dir.path().join("stdin-result.txt")).unwrap())
        .is_equal_to("eof");
}

#[test]
fn terminate_on_drop_trace_fallback_outside_runtime_kills_child_without_panicking() {
    use tokio_process_tools::TerminateOnDropFallback;

    let rt = tokio::runtime::Runtime::new().unwrap();

    let guard = rt.block_on(async {
        spawn_long_running_process().terminate_on_drop_with_fallback(
            short_graceful_shutdown(),
            TerminateOnDropFallback::Trace,
        )
    });
    let pid = guard.id().unwrap();

    // The reported footgun shape: the runtime is still alive, but the dropping thread is not
    // inside its context (e.g. `main` after `Runtime::block_on` returned).
    drop(guard);

    drop(rt);
    assert_child_exited_after_forceful_drop(pid);
}

#[test]
fn terminate_on_drop_trace_fallback_inside_current_thread_kills_child_without_panicking() {
    use tokio_process_tools::TerminateOnDropFallback;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let pid = rt.block_on(async {
        let guard = spawn_long_running_process().terminate_on_drop_with_fallback(
            short_graceful_shutdown(),
            TerminateOnDropFallback::Trace,
        );
        let pid = guard.id().unwrap();
        drop(guard);
        pid
    });

    drop(rt);
    assert_child_exited_after_forceful_drop(pid);
}

#[test]
fn terminate_on_drop_default_fallback_kills_child_before_panicking() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let guard = rt.block_on(async {
        spawn_long_running_process().terminate_on_drop(short_graceful_shutdown())
    });
    let pid = guard.id().unwrap();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)))
        .expect_err("the default fallback should panic outside a runtime context");
    assert_that!(panic)
        .message()
        .contains("outside a tokio runtime context");

    drop(rt);
    assert_child_exited_after_forceful_drop(pid);
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_on_drop_panic_fallback_during_unwind_preserves_original_panic() {
    use tokio_process_tools::TerminateOnDropFallback;

    let process = spawn_long_running_process();
    let pid = process.id().unwrap();
    let guard = process
        .terminate_on_drop_with_fallback(short_graceful_shutdown(), TerminateOnDropFallback::Panic);

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = guard;
        panic!("intentional test panic");
    }));

    let panic = unwound.expect_err("the original panic should continue unwinding");
    assert_that!(panic)
        .message()
        .is_equal_to("intentional test panic");
    assert_child_exited_after_forceful_drop(pid);
}

#[test]
fn terminate_on_drop_with_already_exited_child_closes_lifecycle_without_runtime_context() {
    use std::time::Duration;
    use tokio_process_tools::{RunningState, TerminateOnDropFallback};

    let rt = tokio::runtime::Runtime::new().unwrap();

    let guard = rt.block_on(async {
        let mut process = spawn_immediately_exiting_process();
        loop {
            match process.is_running() {
                RunningState::Terminated(_) => break,
                _ => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        process.terminate_on_drop_with_fallback(
            short_graceful_shutdown(),
            TerminateOnDropFallback::Panic,
        )
    });

    // The synchronous preflight observes the already-reaped exit and closes the lifecycle before
    // consulting the unsupported-context policy, so even the explicit panic fallback stays quiet.
    drop(guard);
    drop(rt);
}

#[test]
fn terminate_on_drop_kills_child_without_hanging_when_runtime_shuts_down() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (pid_tx, pid_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();

    // Drop the runtime on a separate thread, so a hanging shutdown fails this test instead of
    // blocking the whole test binary.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let guard = spawn_long_running_process().terminate_on_drop(short_graceful_shutdown());
            pid_tx.send(guard.id().unwrap()).unwrap();
            tokio::spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        // Cancels the task above, dropping the wrapper while the runtime is shutting down.
        drop(rt);
        dropped_tx.send(()).unwrap();
    });

    let pid = pid_rx.recv().unwrap();
    assert_that!(dropped_rx.recv_timeout(Duration::from_secs(10))).is_ok();
    assert_child_exited_after_forceful_drop(pid);
}

#[cfg(unix)]
#[test]
fn terminate_on_drop_kills_child_without_hanging_when_runtime_shuts_down_during_graceful_drop() {
    use std::sync::mpsc;
    use std::time::Duration;
    use tokio_process_tools::{GracefulShutdown, WaitForLineResult};

    const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(20);

    let (pid_tx, pid_rx) = mpsc::channel();
    let (dropped_tx, dropped_rx) = mpsc::channel();

    // Drop the runtime on a separate thread, so a hanging shutdown fails this test instead of
    // blocking the whole test binary.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // Ignores the graceful signal, so the graceful drop keeps waiting until either its
            // timeout elapses or the runtime shuts down.
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c")
                .arg("trap '' INT TERM; echo ready; sleep 30 & wait");
            let process = spawn_broadcast_with_replay("ignores-graceful-signal", cmd);
            let readiness = process
                .stdout()
                .wait_for_line(
                    Duration::from_secs(5),
                    |line| line == "ready",
                    line_parsing_options(),
                )
                .await
                .unwrap();
            assert_that!(readiness).is_equal_to(WaitForLineResult::Matched);

            let guard = process.terminate_on_drop(
                GracefulShutdown::builder()
                    .unix_sigterm(GRACEFUL_TIMEOUT)
                    .windows_ctrl_break(GRACEFUL_TIMEOUT)
                    .build(),
            );
            pid_tx.send(guard.id().unwrap()).unwrap();

            let (dropping_tx, dropping_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                dropping_tx.send(()).unwrap();
                // The runtime is still running here, so this starts the graceful shutdown and
                // blocks in it.
                drop(guard);
            });
            dropping_rx.await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        // Starts the shutdown while the graceful drop above is still waiting. Tokio's timers
        // then fail and its "being shutdown" panic, printed to stderr, unwinds out of the drop.
        drop(rt);
        dropped_tx.send(()).unwrap();
    });

    let pid = pid_rx.recv().unwrap();
    // Well below the graceful timeout, so passing proves that the shutdown cut the graceful
    // drop short instead of waiting it out.
    assert_that!(dropped_rx.recv_timeout(Duration::from_secs(10))).is_ok();
    assert_child_exited_after_forceful_drop(pid);
}

#[tokio::test]
async fn terminate_on_drop_into_inner_returns_armed_handle_without_terminating() {
    use tokio_process_tools::RunningState;

    let guard = spawn_long_running_process().terminate_on_drop(short_graceful_shutdown());

    // A current-thread runtime cannot run the graceful shutdown, so any drop of the wrapper here
    // would kill the child and panic.
    let mut process = guard.into_inner();

    assert_that!(process.is_running()).is_matching(pattern!(RunningState::Running));
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn terminate_on_drop_inside_multithreaded_runtime_honors_graceful_shutdown() {
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio_process_tools::{TerminateOnDropFallback, WaitForLineResult};

    let temp_dir = tempdir().unwrap();
    let marker = temp_dir.path().join("graceful-signal.txt");

    let mut cmd = signal_responsive_long_running_command();
    cmd.env("TOKIO_PROCESS_TOOLS_GRACEFUL_MARKER", &marker);
    let process = spawn_broadcast_with_replay("graceful-on-drop", cmd);

    let readiness = process
        .stdout()
        .wait_for_line(
            Duration::from_secs(5),
            |line| line == "ready",
            line_parsing_options(),
        )
        .await
        .unwrap();
    assert_that!(readiness).is_equal_to(WaitForLineResult::Matched);

    // The fallback policy is irrelevant in a supported multithreaded context; choosing Panic here
    // makes that boundary explicit and guards against accidentally consulting it on success.
    let guard = process.terminate_on_drop_with_fallback(
        default_graceful_shutdown(),
        TerminateOnDropFallback::Panic,
    );
    drop(guard);

    // The graceful drop blocks until termination completes, and the child writes the marker
    // before exiting, so the file is present here if and only if the graceful signal was
    // delivered instead of an immediate SIGKILL.
    assert_that!(fs::read_to_string(&marker).unwrap()).is_equal_to("graceful");
}

#[cfg(unix)]
#[tokio::test]
async fn must_not_be_terminated_still_closes_stdout_pipe_on_drop() {
    use nix::errno::Errno;
    use nix::sys::wait::waitpid;
    use nix::unistd::Pid;
    use std::time::Duration;
    use tokio_process_tools::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, Process};

    let mut cmd = tokio::process::Command::new("yes");
    cmd.arg("tick");

    let mut process = Process::new(cmd)
        .name("yes")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    let pid = Pid::from_raw(process.id().unwrap().cast_signed());

    process.must_not_be_terminated();
    drop(process);

    let wait_result = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || waitpid(pid, None)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_that!(wait_result).is_matching(pattern!(Ok(_) | Err(Errno::ECHILD)));
}
