#![cfg(any(unix, windows))]
#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::io;
use std::process::ExitStatus;
use tokio_process_tools::{TerminationAction, TerminationError};

fn successful_exit_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }
}

mod preflight_reap {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn interrupt_reaps_already_exited_child_before_signalling() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        let result = process.send_signal_with_reaper(
            "SIGINT",
            |_| {
                signal_attempts += 1;
                Ok(())
            },
            |_| {
                reap_attempts += 1;
                // Lie about process being dead.
                Ok(Some(successful_exit_status()))
            },
        );

        assert_that!(result).is_ok();
        assert_that!(signal_attempts).is_equal_to(0);
        assert_that!(reap_attempts).is_equal_to(1);
        assert_that!(process.is_drop_disarmed()).is_true();

        // Fake reap left the process alive. We must kill it here.
        process.kill().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminate_reaps_already_exited_child_before_signalling() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        let result = process.send_signal_with_reaper(
            "SIGTERM",
            |_| {
                signal_attempts += 1;
                Ok(())
            },
            |_| {
                reap_attempts += 1;
                // Lie about process being dead.
                Ok(Some(successful_exit_status()))
            },
        );

        assert_that!(result).is_ok();
        assert_that!(signal_attempts).is_equal_to(0);
        assert_that!(reap_attempts).is_equal_to(1);
        assert_that!(process.is_drop_disarmed()).is_true();

        // Fake reap left the process alive. We must kill it here.
        process.kill().await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn ctrl_break_reaps_already_exited_child_before_signalling() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        let result = process.send_signal_with_reaper(
            "CTRL_BREAK_EVENT",
            |_| {
                signal_attempts += 1;
                Ok(())
            },
            |_| {
                reap_attempts += 1;
                // Lie about process being dead.
                Ok(Some(successful_exit_status()))
            },
        );

        assert_that!(result).is_ok();
        assert_that!(signal_attempts).is_equal_to(0);
        assert_that!(reap_attempts).is_equal_to(1);
        assert_that!(process.is_drop_disarmed()).is_true();

        // Fake reap left the process alive. We must kill it here.
        process.kill().await.unwrap();
    }

    #[tokio::test]
    async fn reaps_exit_observed_after_signal_failure() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        #[cfg(unix)]
        let label = "SIGINT";
        #[cfg(windows)]
        let label = "CTRL_BREAK_EVENT";
        let result = process.send_signal_with_reaper(
            label,
            |_| {
                signal_attempts += 1;
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "injected signal failure",
                ))
            },
            |process| {
                reap_attempts += 1;
                match reap_attempts {
                    1 => Ok(None),
                    2 => {
                        process.must_not_be_terminated();
                        // Lie about process being dead.
                        Ok(Some(successful_exit_status()))
                    }
                    _ => panic!("unexpected reap attempt"),
                }
            },
        );

        assert_that!(result).is_ok();
        assert_that!(signal_attempts).is_equal_to(1);
        assert_that!(reap_attempts).is_equal_to(2);
        assert_that!(process.is_drop_disarmed()).is_true();

        // Fake reap left the process alive. We must kill it here.
        process.kill().await.unwrap();
    }
}

mod signal_failures {
    use super::*;

    #[tokio::test]
    async fn returns_typed_error_when_child_is_still_running_after_signal_failure() {
        let mut process = spawn_long_running_process();
        let mut signal_attempts = 0;
        let mut reap_attempts = 0;

        #[cfg(unix)]
        let label = "SIGINT";
        #[cfg(windows)]
        let label = "CTRL_BREAK_EVENT";
        let error = process
            .send_signal_with_reaper(
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
            TerminationAction::SendSignal { signal_name: label },
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
    async fn reports_signal_and_reap_failures_in_order() {
        let mut process = spawn_long_running_process();
        let mut reap_attempts = 0;

        let error = process
            .send_signal_with_reaper(
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
            TerminationAction::SendSignal {
                signal_name: "SIGTERM",
            },
            io::ErrorKind::PermissionDenied,
            "injected signal failure",
        );
        assert_attempt_error(
            &error.attempt_errors()[1],
            TerminationAction::CheckStatus,
            io::ErrorKind::Other,
            "injected status failure",
        );

        process.kill().await.unwrap();
    }
}
