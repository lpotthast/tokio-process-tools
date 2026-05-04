use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn kill_future_can_be_spawned_on_tokio_task() {
    let mut process = spawn_long_running_process();

    let result = tokio::spawn(async move { process.kill().await })
        .await
        .unwrap();

    assert_that!(result).is_ok();
}

#[tokio::test]
async fn kill_disarms_cleanup_and_panic_guards() {
    let mut process = crate::Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    assert_that!(process.stdin().is_open()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();

    assert_that!(process.stdin().is_open()).is_false();
    assert_that!(process.is_drop_disarmed()).is_true();

    drop(process);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn kill_signals_grandchildren_via_process_group() {
    // Spawn `sh -c 'sleep 30 & echo $!; wait'`. The shell forks `sleep 30` into the background,
    // prints its PID on stdout, then waits. With process-group-targeted signal delivery,
    // killing the shell must also kill the grandchild `sleep`; a PID-targeted kill would only
    // reap the shell and leave the sleep behind, reparented to init and still ticking.
    use crate::{DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, LineParsingOptions, Next};
    use std::sync::Mutex;
    use std::time::Instant;
    use tokio::process::Command;
    use tokio::sync::oneshot;

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg("sleep 30 & echo $!; wait");

    let mut process = crate::Process::new(cmd)
        .name("group-kill-test")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .reliable_for_active_subscribers()
                .replay_last_bytes(64.bytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

    let (tx, rx) = oneshot::channel::<u32>();
    let tx = Mutex::new(Some(tx));
    let _consumer = process.stdout().inspect_lines(
        move |line| {
            if let Ok(pid) = line.trim().parse::<u32>()
                && let Some(tx) = tx.lock().unwrap().take()
            {
                let _ = tx.send(pid);
            }
            Next::Continue
        },
        LineParsingOptions::default(),
    );

    let grandchild_pid = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("grandchild should print its PID within 5 s")
        .expect("oneshot should resolve once the line is observed");

    process.kill().await.unwrap();

    // After group SIGKILL the grandchild becomes a zombie until init reaps it. Poll the
    // signal-probe until it returns ESRCH, which it does once init has cleaned up.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(grandchild_pid.cast_signed()),
            None,
        )
        .is_ok();
        if !alive {
            return;
        }
        assert_that!(Instant::now() <= deadline)
            .with_detail_message(format!(
                "grandchild PID {grandchild_pid} still answers signal probes 5 s after kill"
            ))
            .is_true();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn kill_reports_start_kill_failure_as_termination_error() {
    let mut process = spawn_long_running_process();

    let error = process
        .kill_inner(|_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected kill failure",
            ))
        })
        .await
        .unwrap_err();

    assert_that!(error.process_name()).is_equal_to("long-running");
    assert_that!(matches!(&error, TerminationError::TerminationFailed { .. })).is_true();
    assert_that!(error.attempt_errors().len()).is_equal_to(1);
    assert_attempt_error(
        &error.attempt_errors()[0],
        TerminationAttemptPhase::Kill,
        TerminationAttemptOperation::SendSignal,
        Some(if cfg!(unix) {
            "SIGKILL"
        } else {
            "TerminateProcess"
        }),
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    );
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}
