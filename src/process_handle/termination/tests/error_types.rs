use super::*;

#[cfg(unix)]
#[test]
fn termination_failed_preserves_recorded_attempt_errors_in_order() {
    let mut diagnostics = TerminationDiagnostics::default();
    diagnostics
        .record_preflight_status_error(io::Error::other("injected preflight status failure"));
    diagnostics.record_graceful_signal_error(
        GracefulTerminationPhase::Interrupt,
        "SIGINT",
        io::Error::new(io::ErrorKind::Interrupted, "injected interrupt failure"),
    );
    diagnostics.record_graceful_status_error(
        GracefulTerminationPhase::Interrupt,
        "SIGINT",
        io::Error::new(
            io::ErrorKind::ConnectionReset,
            "injected interrupt status failure",
        ),
    );
    diagnostics.record_graceful_wait_error(
        GracefulTerminationPhase::Terminate,
        "SIGTERM",
        io::Error::new(io::ErrorKind::TimedOut, "injected terminate wait failure"),
    );
    diagnostics.record_kill_signal_error(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    ));
    diagnostics.record_kill_wait_error(io::Error::new(
        io::ErrorKind::TimedOut,
        "final wait failure",
    ));

    let error = diagnostics.into_termination_failed(Cow::Borrowed("diagnostic-test"));
    let attempt_errors = error.attempt_errors();

    assert_that!(attempt_errors.len()).is_equal_to(6);
    assert_attempt_error(
        &attempt_errors[0],
        TerminationAttemptPhase::Preflight,
        TerminationAttemptOperation::CheckStatus,
        None,
        io::ErrorKind::Other,
        "injected preflight status failure",
    );
    assert_attempt_error(
        &attempt_errors[1],
        TerminationAttemptPhase::Interrupt,
        TerminationAttemptOperation::SendSignal,
        Some("SIGINT"),
        io::ErrorKind::Interrupted,
        "injected interrupt failure",
    );
    assert_attempt_error(
        &attempt_errors[2],
        TerminationAttemptPhase::Interrupt,
        TerminationAttemptOperation::CheckStatus,
        Some("SIGINT"),
        io::ErrorKind::ConnectionReset,
        "injected interrupt status failure",
    );
    assert_attempt_error(
        &attempt_errors[3],
        TerminationAttemptPhase::Terminate,
        TerminationAttemptOperation::WaitForExit,
        Some("SIGTERM"),
        io::ErrorKind::TimedOut,
        "injected terminate wait failure",
    );
    assert_attempt_error(
        &attempt_errors[4],
        TerminationAttemptPhase::Kill,
        TerminationAttemptOperation::SendSignal,
        Some("SIGKILL"),
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    );
    assert_attempt_error(
        &attempt_errors[5],
        TerminationAttemptPhase::Kill,
        TerminationAttemptOperation::WaitForExit,
        Some("SIGKILL"),
        io::ErrorKind::TimedOut,
        "final wait failure",
    );
}

#[cfg(windows)]
#[test]
fn termination_failed_preserves_recorded_attempt_errors_in_order() {
    let mut diagnostics = TerminationDiagnostics::default();
    diagnostics
        .record_preflight_status_error(io::Error::other("injected preflight status failure"));
    diagnostics.record_graceful_signal_error(
        GracefulTerminationPhase::Terminate,
        "CTRL_BREAK_EVENT",
        io::Error::new(io::ErrorKind::Interrupted, "injected graceful failure"),
    );
    diagnostics.record_graceful_status_error(
        GracefulTerminationPhase::Terminate,
        "CTRL_BREAK_EVENT",
        io::Error::new(
            io::ErrorKind::ConnectionReset,
            "injected graceful status failure",
        ),
    );
    diagnostics.record_kill_signal_error(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    ));
    diagnostics.record_kill_wait_error(io::Error::new(
        io::ErrorKind::TimedOut,
        "final wait failure",
    ));

    let error = diagnostics.into_termination_failed(Cow::Borrowed("diagnostic-test"));
    let attempt_errors = error.attempt_errors();

    assert_that!(attempt_errors.len()).is_equal_to(5);
    assert_attempt_error(
        &attempt_errors[0],
        TerminationAttemptPhase::Preflight,
        TerminationAttemptOperation::CheckStatus,
        None,
        io::ErrorKind::Other,
        "injected preflight status failure",
    );
    assert_attempt_error(
        &attempt_errors[1],
        TerminationAttemptPhase::Terminate,
        TerminationAttemptOperation::SendSignal,
        Some("CTRL_BREAK_EVENT"),
        io::ErrorKind::Interrupted,
        "injected graceful failure",
    );
    assert_attempt_error(
        &attempt_errors[2],
        TerminationAttemptPhase::Terminate,
        TerminationAttemptOperation::CheckStatus,
        Some("CTRL_BREAK_EVENT"),
        io::ErrorKind::ConnectionReset,
        "injected graceful status failure",
    );
    assert_attempt_error(
        &attempt_errors[3],
        TerminationAttemptPhase::Kill,
        TerminationAttemptOperation::SendSignal,
        Some("TerminateProcess"),
        io::ErrorKind::PermissionDenied,
        "injected kill failure",
    );
    assert_attempt_error(
        &attempt_errors[4],
        TerminationAttemptPhase::Kill,
        TerminationAttemptOperation::WaitForExit,
        Some("TerminateProcess"),
        io::ErrorKind::TimedOut,
        "final wait failure",
    );
}
