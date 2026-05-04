use super::*;

#[tokio::test]
async fn send_signal_returns_typed_error_when_child_is_still_running_after_signal_failure() {
    let mut process = spawn_long_running_process();
    let mut signal_attempts = 0;
    let mut reap_attempts = 0;

    #[cfg(unix)]
    let (phase, label, expected_phase) = (
        GracefulTerminationPhase::Interrupt,
        "SIGINT",
        TerminationAttemptPhase::Interrupt,
    );
    #[cfg(windows)]
    let (phase, label, expected_phase) = (
        GracefulTerminationPhase::Terminate,
        "CTRL_BREAK_EVENT",
        TerminationAttemptPhase::Terminate,
    );
    let error = process
        .send_signal_with_reaper(
            phase,
            label,
            |_| {
                signal_attempts += 1;
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected signal failure",
                ))
            },
            |_| {
                reap_attempts += 1;
                Ok(None)
            },
        )
        .unwrap_err();

    assert_that!(error.process_name()).is_equal_to("long-running");
    assert_that!(matches!(&error, TerminationError::SignalFailed { .. })).is_true();
    assert_that!(error.attempt_errors().len()).is_equal_to(1);
    assert_attempt_error(
        &error.attempt_errors()[0],
        expected_phase,
        TerminationAttemptOperation::SendSignal,
        Some(label),
        io::ErrorKind::PermissionDenied,
        "injected signal failure",
    );
    assert_that!(signal_attempts).is_equal_to(1);
    assert_that!(reap_attempts).is_equal_to(2);
    assert_that!(process.is_drop_armed()).is_true();

    process.kill().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn send_signal_reports_signal_and_reap_failures_in_order() {
    let mut process = spawn_long_running_process();
    let mut reap_attempts = 0;

    let error = process
        .send_signal_with_reaper(
            GracefulTerminationPhase::Terminate,
            "SIGTERM",
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected signal failure",
                ))
            },
            |_| {
                reap_attempts += 1;
                match reap_attempts {
                    1 => Ok(None),
                    2 => Err(io::Error::other("injected status failure")),
                    _ => panic!("unexpected reap attempt"),
                }
            },
        )
        .unwrap_err();

    assert_that!(error.process_name()).is_equal_to("long-running");
    assert_that!(matches!(&error, TerminationError::SignalFailed { .. })).is_true();
    assert_that!(error.attempt_errors().len()).is_equal_to(2);
    assert_attempt_error(
        &error.attempt_errors()[0],
        TerminationAttemptPhase::Terminate,
        TerminationAttemptOperation::SendSignal,
        Some("SIGTERM"),
        io::ErrorKind::PermissionDenied,
        "injected signal failure",
    );
    assert_attempt_error(
        &error.attempt_errors()[1],
        TerminationAttemptPhase::Terminate,
        TerminationAttemptOperation::CheckStatus,
        Some("SIGTERM"),
        io::ErrorKind::Other,
        "injected status failure",
    );

    process.kill().await.unwrap();
}
