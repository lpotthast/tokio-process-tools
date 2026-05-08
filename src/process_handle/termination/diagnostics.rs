//! Accumulator for graceful termination errors. The driver records each failed action here as it
//! goes and converts the buffer into a [`TerminationError`] (`TerminationFailed` or
//! `SignalFailed`) when the call ends in failure.

use crate::error::{TerminationAction, TerminationAttemptError, TerminationError};
use std::borrow::Cow;
use std::error::Error;

#[derive(Debug, Default)]
pub(in crate::process_handle) struct TerminationDiagnostics {
    attempt_errors: Vec<TerminationAttemptError>,
}

impl TerminationDiagnostics {
    pub(in crate::process_handle) fn record(
        &mut self,
        action: TerminationAction,
        error: impl Error + Send + Sync + 'static,
    ) {
        self.attempt_errors.push(TerminationAttemptError {
            action,
            source: Box::new(error),
        });
    }

    #[must_use]
    pub(super) fn into_termination_failed(
        self,
        process_name: Cow<'static, str>,
    ) -> TerminationError {
        assert!(
            !self.attempt_errors.is_empty(),
            "into_termination_failed must not be used when no error was recorded!",
        );

        TerminationError::TerminationFailed {
            process_name,
            attempt_errors: self.attempt_errors,
        }
    }

    #[cfg(any(unix, windows))]
    #[must_use]
    pub(in crate::process_handle) fn into_signal_failed(
        self,
        process_name: Cow<'static, str>,
    ) -> TerminationError {
        assert!(
            !self.attempt_errors.is_empty(),
            "into_signal_failed must not be used when no error was recorded!",
        );

        TerminationError::SignalFailed {
            process_name,
            attempt_errors: self.attempt_errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::tests::assert_attempt_error;
    use assertr::prelude::*;
    use std::io;

    #[cfg(unix)]
    #[test]
    fn termination_failed_preserves_recorded_attempt_errors_in_order() {
        let mut diagnostics = TerminationDiagnostics::default();
        diagnostics.record(
            TerminationAction::CheckStatus,
            io::Error::other("injected preflight status failure"),
        );
        diagnostics.record(
            TerminationAction::SendSignal {
                signal_name: "SIGINT",
            },
            io::Error::new(io::ErrorKind::Interrupted, "injected interrupt failure"),
        );
        diagnostics.record(
            TerminationAction::CheckStatus,
            io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected interrupt status failure",
            ),
        );
        diagnostics.record(
            TerminationAction::WaitForExit,
            io::Error::new(io::ErrorKind::TimedOut, "injected terminate wait failure"),
        );
        diagnostics.record(
            TerminationAction::SendSignal {
                signal_name: "SIGKILL",
            },
            io::Error::new(io::ErrorKind::PermissionDenied, "injected kill failure"),
        );
        diagnostics.record(
            TerminationAction::WaitForExit,
            io::Error::new(io::ErrorKind::TimedOut, "final wait failure"),
        );

        let error = diagnostics.into_termination_failed(Cow::Borrowed("diagnostic-test"));
        let attempt_errors = error.attempt_errors();

        assert_that!(attempt_errors.len()).is_equal_to(6);
        assert_attempt_error(
            &attempt_errors[0],
            TerminationAction::CheckStatus,
            io::ErrorKind::Other,
            "injected preflight status failure",
        );
        assert_attempt_error(
            &attempt_errors[1],
            TerminationAction::SendSignal {
                signal_name: "SIGINT",
            },
            io::ErrorKind::Interrupted,
            "injected interrupt failure",
        );
        assert_attempt_error(
            &attempt_errors[2],
            TerminationAction::CheckStatus,
            io::ErrorKind::ConnectionReset,
            "injected interrupt status failure",
        );
        assert_attempt_error(
            &attempt_errors[3],
            TerminationAction::WaitForExit,
            io::ErrorKind::TimedOut,
            "injected terminate wait failure",
        );
        assert_attempt_error(
            &attempt_errors[4],
            TerminationAction::SendSignal {
                signal_name: "SIGKILL",
            },
            io::ErrorKind::PermissionDenied,
            "injected kill failure",
        );
        assert_attempt_error(
            &attempt_errors[5],
            TerminationAction::WaitForExit,
            io::ErrorKind::TimedOut,
            "final wait failure",
        );
    }

    #[cfg(windows)]
    #[test]
    fn termination_failed_preserves_recorded_attempt_errors_in_order() {
        let mut diagnostics = TerminationDiagnostics::default();
        diagnostics.record(
            TerminationAction::CheckStatus,
            io::Error::other("injected preflight status failure"),
        );
        diagnostics.record(
            TerminationAction::SendSignal {
                signal_name: "CTRL_BREAK_EVENT",
            },
            io::Error::new(io::ErrorKind::Interrupted, "injected graceful failure"),
        );
        diagnostics.record(
            TerminationAction::CheckStatus,
            io::Error::new(
                io::ErrorKind::ConnectionReset,
                "injected graceful status failure",
            ),
        );
        diagnostics.record(
            TerminationAction::SendSignal {
                signal_name: "TerminateProcess",
            },
            io::Error::new(io::ErrorKind::PermissionDenied, "injected kill failure"),
        );
        diagnostics.record(
            TerminationAction::WaitForExit,
            io::Error::new(io::ErrorKind::TimedOut, "final wait failure"),
        );

        let error = diagnostics.into_termination_failed(Cow::Borrowed("diagnostic-test"));
        let attempt_errors = error.attempt_errors();

        assert_that!(attempt_errors.len()).is_equal_to(5);
        assert_attempt_error(
            &attempt_errors[0],
            TerminationAction::CheckStatus,
            io::ErrorKind::Other,
            "injected preflight status failure",
        );
        assert_attempt_error(
            &attempt_errors[1],
            TerminationAction::SendSignal {
                signal_name: "CTRL_BREAK_EVENT",
            },
            io::ErrorKind::Interrupted,
            "injected graceful failure",
        );
        assert_attempt_error(
            &attempt_errors[2],
            TerminationAction::CheckStatus,
            io::ErrorKind::ConnectionReset,
            "injected graceful status failure",
        );
        assert_attempt_error(
            &attempt_errors[3],
            TerminationAction::SendSignal {
                signal_name: "TerminateProcess",
            },
            io::ErrorKind::PermissionDenied,
            "injected kill failure",
        );
        assert_attempt_error(
            &attempt_errors[4],
            TerminationAction::WaitForExit,
            io::ErrorKind::TimedOut,
            "final wait failure",
        );
    }
}
