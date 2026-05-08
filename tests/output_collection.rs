#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_process_tools::{
    AutoName, DEFAULT_OUTPUT_EOF_TIMEOUT, GracefulShutdown, LineCollectionOptions,
    LineOutputOptions, UnixGracefulShutdown, WaitWithOutputError, WindowsGracefulShutdown,
};

fn exits_while_descendant_keeps_stdout_open_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg("printf 'ready\n'; sleep 0.5 &");
    cmd
}

#[cfg(unix)]
fn ignores_graceful_shutdown_and_keeps_stdout_open_until_force_killed_command()
-> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg("trap '' INT TERM; printf 'ready\n'; sleep 0.2 & wait");
    cmd
}

#[track_caller]
fn assert_eof_timeout(err: WaitWithOutputError, expected_eof: Duration) {
    match err {
        WaitWithOutputError::OutputCollectionTimeout {
            timeout: actual, ..
        } => {
            assert_that!(actual).is_equal_to(expected_eof);
        }
        other => assert_that!(&other).fail(format_args!(
            "expected output collection timeout, got {other:?}"
        )),
    }
}

mod with_line_output {
    use super::*;

    #[tokio::test]
    async fn collects_stdout_lines_and_empty_stderr() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("Line 1\nLine 2\nLine 3\n")
                .build(),
        );

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines()).contains_exactly(["Line 1", "Line 2", "Line 3"]);
        assert_that!(output.stderr.lines()).is_empty();
    }

    #[tokio::test]
    async fn captures_startup_output_with_replay_last_bytes() {
        let mut process = spawn_single_subscriber_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("startup-out\n")
                .stderr("startup-err\n")
                .build(),
        );

        tokio::time::sleep(Duration::from_millis(50)).await;

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines()).contains_exactly(["startup-out"]);
        assert_that!(output.stderr.lines()).contains_exactly(["startup-err"]);
    }

    #[tokio::test]
    async fn closes_stdin_before_waiting() {
        let mut process = spawn_broadcast_with_replay("cat", tokio::process::Command::new("cat"));

        let Some(stdin) = process.stdin().as_mut() else {
            assert_that!(process.stdin().is_open()).fail("stdin should start open");
            return;
        };
        stdin
            .write_all(b"collector wait closes stdin\n")
            .await
            .unwrap();
        stdin.flush().await.unwrap();

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(process.stdin().is_open()).is_false();
        assert_that!(output.stdout.lines()).contains_exactly(["collector wait closes stdin"]);
        assert_that!(output.stderr.lines()).is_empty();
    }

    #[tokio::test]
    async fn collects_trusted_unbounded_lines() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("out\nline\n")
                .stderr("err\nline\n")
                .build(),
        );

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                LineOutputOptions::symmetric(LineCollectionOptions::TrustedUnbounded),
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines()).contains_exactly(["out", "line"]);
        assert_that!(output.stdout.truncated()).is_false();
        assert_that!(output.stderr.lines()).contains_exactly(["err", "line"]);
        assert_that!(output.stderr.truncated()).is_false();
    }

    #[tokio::test]
    async fn times_out_when_descendant_keeps_output_open() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            exits_while_descendant_keeps_stdout_open_command(),
        );

        let timeout = Duration::from_secs(1);
        let eof_timeout = Duration::from_millis(100);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion(timeout).with_line_output(
                eof_timeout,
                line_parsing_options(),
                line_output_options(),
            ),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        assert_eof_timeout(err, eof_timeout);
    }

    #[tokio::test]
    async fn single_subscriber_timeout_releases_collector_claim() {
        let mut process = spawn_single_subscriber_with_replay(
            AutoName::program_only(),
            exits_while_descendant_keeps_stdout_open_command(),
        );

        let result = process
            .wait_for_completion(Duration::from_secs(1))
            .with_line_output(
                Duration::from_millis(100),
                line_parsing_options(),
                line_output_options(),
            )
            .await;
        assert_that!(result.is_err()).is_true();

        let collector = collect_lines_into_vec(process.stdout()).unwrap();
        let _collected = collector
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("collector should observe cancellation");
    }

    #[tokio::test]
    async fn single_subscriber_can_be_retried_after_process_finishes() {
        let mut process = spawn_single_subscriber_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("early-out\n")
                .stderr("early-err\n")
                .stdout_after(Duration::from_millis(200), "late-out\n")
                .stderr_after(Duration::from_millis(200), "late-err\n")
                .build(),
        );

        let first = process
            .wait_for_completion(Duration::from_millis(25))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await;
        assert_that!(first.unwrap().into_completed().is_none()).is_true();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines()).contains_exactly(["early-out", "late-out"]);
        assert_that!(output.stderr.lines()).contains_exactly(["early-err", "late-err"]);
    }

    #[tokio::test]
    async fn single_subscriber_active_stdout_consumer_returns_start_error() {
        let mut process = spawn_single_subscriber_with_replay(
            "scripted",
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        );

        let active = collect_chunks_into_vec(process.stdout()).unwrap();

        let err = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .expect_err("active stdout consumer should reject output collection");

        match err {
            WaitWithOutputError::OutputCollectionStartFailed { .. } => {}
            other => {
                assert_that!(&other).fail(format_args!("expected start failure, got {other:?}"));
            }
        }

        let _collected = active
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("collector should observe cancellation");
        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");
    }

    #[tokio::test]
    async fn single_subscriber_active_stderr_consumer_aborts_started_stdout_collector() {
        let mut process = spawn_single_subscriber_with_replay(
            "scripted",
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        );

        let active_stderr = collect_chunks_into_vec(process.stderr()).unwrap();

        let err = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .await
            .expect_err("active stderr consumer should reject output collection");

        match err {
            WaitWithOutputError::OutputCollectionStartFailed { .. } => {}
            other => {
                assert_that!(&other).fail(format_args!("expected start failure, got {other:?}"));
            }
        }

        let stdout = collect_chunks_into_vec(process.stdout()).unwrap();
        let _stdout = stdout
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("stdout collector should observe cancellation");
        let _stderr = active_stderr
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("stderr collector should observe cancellation");

        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");
    }
}

mod with_raw_output {
    use super::*;

    #[tokio::test]
    async fn preserves_bytes_broadcast() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("out\nraw")
                .stderr("err\nraw")
                .build(),
        );

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_raw_output(DEFAULT_OUTPUT_EOF_TIMEOUT, raw_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"out\nraw".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"err\nraw".to_vec());
    }
}

mod with_line_output_or_terminate {
    use super::*;

    #[tokio::test]
    async fn collects_lines() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("line-out\n")
                .stderr("line-err\n")
                .build(),
        );

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_line_output(
                DEFAULT_OUTPUT_EOF_TIMEOUT,
                line_parsing_options(),
                line_output_options(),
            )
            .or_terminate(default_graceful_shutdown())
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines()).contains_exactly(["line-out"]);
        assert_that!(output.stderr.lines()).contains_exactly(["line-err"]);
    }

    #[tokio::test]
    async fn times_out_when_descendant_keeps_output_open() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            exits_while_descendant_keeps_stdout_open_command(),
        );

        let wait_timeout = Duration::from_millis(100);
        let eof_timeout = Duration::from_millis(100);
        let shutdown = GracefulShutdown::builder()
            .unix(UnixGracefulShutdown::terminate_only(Duration::from_millis(
                1,
            )))
            .windows(WindowsGracefulShutdown::new(Duration::from_millis(2)))
            .build();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process
                .wait_for_completion(wait_timeout)
                .with_line_output(eof_timeout, line_parsing_options(), line_output_options())
                .or_terminate(shutdown),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        assert_eof_timeout(err, eof_timeout);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_kill_allows_configured_eof_drain() {
        use tokio_process_tools::{UnixGracefulPhase, WaitForCompletionOrTerminateResult};

        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ignores_graceful_shutdown_and_keeps_stdout_open_until_force_killed_command(),
        );

        let wait_timeout = Duration::from_millis(25);
        // The fixture installs traps for both SIGINT and SIGTERM, so a single-signal sequence
        // would not exercise both graceful phases. Use a multi-phase sequence to drive the
        // executor through SIGINT, SIGTERM, and the SIGKILL fallback.
        let shutdown = GracefulShutdown {
            unix: UnixGracefulShutdown::from_phases([
                UnixGracefulPhase::interrupt(Duration::from_millis(25)),
                UnixGracefulPhase::terminate(Duration::from_millis(25)),
            ]),
        };
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            process
                .wait_for_completion(wait_timeout)
                .with_line_output(
                    DEFAULT_OUTPUT_EOF_TIMEOUT,
                    line_parsing_options(),
                    line_output_options(),
                )
                .or_terminate(shutdown),
        )
        .await
        .expect("output wait should return before the outer guard")
        .expect("configured EOF grace should let output collection finish after force-kill");
        let output = match output {
            WaitForCompletionOrTerminateResult::TerminatedAfterTimeout { result, timeout } => {
                assert_that!(timeout).is_equal_to(wait_timeout);
                result
            }
            other @ WaitForCompletionOrTerminateResult::Completed(_) => {
                assert_that!(&other).fail(format_args!(
                    "expected timeout-driven termination, got {other:?}"
                ));
                return;
            }
        };

        assert_that!(output.status.success()).is_false();
        assert_that!(output.stdout.lines()).contains_exactly(["ready"]);
    }
}

mod with_raw_output_or_terminate {
    use super::*;

    #[tokio::test]
    async fn collects_bytes() {
        let mut process = spawn_broadcast_with_replay(
            AutoName::program_only(),
            ScriptedOutput::builder()
                .stdout("raw-out")
                .stderr("raw-err")
                .build(),
        );

        let output = process
            .wait_for_completion(Duration::from_secs(2))
            .with_raw_output(DEFAULT_OUTPUT_EOF_TIMEOUT, raw_output_options())
            .or_terminate(default_graceful_shutdown())
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"raw-out".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"raw-err".to_vec());
    }
}
