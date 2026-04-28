use super::*;
use crate::panic_on_drop::PanicOnDrop;

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
    assert_that!(process.cleanup_on_drop).is_true();
    assert_that!(
        process
            .panic_on_drop
            .as_ref()
            .is_some_and(PanicOnDrop::is_armed)
    )
    .is_true();

    process.kill().await.unwrap();

    assert_that!(process.stdin().is_open()).is_false();
    assert_that!(process.cleanup_on_drop).is_false();
    assert_that!(&process.panic_on_drop).is_none();

    drop(process);
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
        Some(signal::KILL_SIGNAL_NAME),
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    );
    assert_that!(process.cleanup_on_drop).is_true();
    assert_that!(
        process
            .panic_on_drop
            .as_ref()
            .is_some_and(PanicOnDrop::is_armed)
    )
    .is_true();

    process.kill().await.unwrap();
}
