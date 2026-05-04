use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn terminate_future_can_be_spawned_on_tokio_task() {
    let mut process = spawn_long_running_process();

    let result = tokio::spawn(async move {
        process
            .terminate(Duration::from_secs(1), Duration::from_secs(1))
            .await
    })
    .await
    .unwrap();

    assert_that!(result).is_ok();
}

#[tokio::test]
async fn terminate_falls_back_to_kill_when_graceful_signal_sends_fail() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut process = spawn_long_running_process();
    let terminate_attempted = Arc::new(AtomicBool::new(false));
    let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

    let outcome = process
        .terminate_inner_detailed(
            Duration::from_millis(10),
            Duration::from_millis(10),
            |_| Err(io::Error::other("injected interrupt signal failure")),
            move |_| {
                terminate_attempted_in_sender.store(true, Ordering::SeqCst);
                Err(io::Error::other("injected terminate signal failure"))
            },
        )
        .await
        .unwrap();

    assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.exit_status.success()).is_false();
    assert_that!(outcome.output_collection_timeout_extension).is_equal_to(FORCE_KILL_WAIT_TIMEOUT);
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn terminate_detailed_reports_no_output_extension_when_graceful_phase_succeeds() {
    let mut process = spawn_long_running_process();

    let outcome = process
        .terminate_detailed(Duration::from_secs(1), Duration::from_secs(1))
        .await
        .unwrap();

    assert_that!(outcome.exit_status.success()).is_false();
    assert_that!(outcome.output_collection_timeout_extension).is_equal_to(Duration::ZERO);
    assert_that!(process.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn canceled_terminate_keeps_drop_guards_armed() {
    let mut process = spawn_long_running_process();

    let result = tokio::time::timeout(
        Duration::from_millis(50),
        process.terminate_inner(
            Duration::from_secs(5),
            Duration::from_secs(5),
            |_| Ok(()),
            |_| Ok(()),
        ),
    )
    .await;

    assert_that!(result.is_err()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[tokio::test]
async fn failed_terminate_result_keeps_drop_guards_armed() {
    let mut process = spawn_long_running_process();

    let result = process.disarm_after_successful_termination::<ExitStatus>(Err(
        TerminationError::TerminationFailed {
            process_name: Cow::Borrowed("long-running"),
            attempt_errors: vec![TerminationAttemptError {
                phase: TerminationAttemptPhase::Kill,
                operation: TerminationAttemptOperation::SendSignal,
                signal_name: Some(signal::KILL_SIGNAL_NAME),
                source: Box::new(io::Error::other("synthetic termination failure")),
            }],
        },
    ));

    assert_that!(result.is_err()).is_true();
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[tokio::test]
async fn terminate_stops_process() {
    let started_at = jiff::Zoned::now();
    let mut handle = crate::Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let exit_status = handle
        .terminate(Duration::from_secs(1), Duration::from_secs(1))
        .await
        .unwrap();
    let terminated_at = jiff::Zoned::now();

    let ran_for = started_at.duration_until(&terminated_at);
    assert_that!(ran_for.as_secs_f32()).is_close_to(0.1, 0.5);

    assert_that!(exit_status.code()).is_none();
    assert_that!(exit_status.success()).is_false();
    assert_that!(handle.is_drop_disarmed()).is_true();
}

#[tokio::test]
async fn terminate_returns_normal_exit_when_process_already_exited() {
    let mut handle = spawn_immediately_exiting_process();

    tokio::time::sleep(Duration::from_millis(50)).await;

    let exit_status = handle
        .terminate(Duration::from_millis(50), Duration::from_millis(50))
        .await
        .unwrap();

    assert_that!(exit_status.success()).is_true();
}

/// Regression test for the "signal send fails while the child has already exited" race.
///
/// Forces the preflight probe to miss the exit (`Ok(None)`) and both graceful signal sends to
/// fail. Without the bounded-wait recovery, this falls through to the kill phase and exits via
/// SIGKILL. With it, the 100 ms grace observes the natural exit and reports success.
#[tokio::test]
async fn terminate_succeeds_when_signal_send_fails_against_recently_exited_child() {
    let mut handle = spawn_immediately_exiting_process();

    let outcome = handle
        .terminate_inner_with_preflight_reaper(
            Duration::from_millis(50),
            Duration::from_millis(50),
            |_| Ok(None),
            |_| Err(io::Error::other("injected interrupt signal-send failure")),
            |_| Err(io::Error::other("injected terminate signal-send failure")),
        )
        .await
        .unwrap();

    assert_that!(outcome.exit_status.success()).is_true();
    assert_that!(handle.is_drop_disarmed()).is_true();
}
