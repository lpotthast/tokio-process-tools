use crate::test_support::{
    ScriptedOutput, line_collection_options, line_output_options, line_parsing_options,
    raw_output_options,
};
use crate::{
    AutoName, CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
    LineCollectionOptions, LineOutputOptions, NumBytesExt, RawCollectionOptions, RawOutputOptions,
    StreamConsumerError, WaitForCompletionOrTerminateOptions, WaitWithOutputError,
};
use assertr::prelude::*;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn wait_or_terminate_options(wait_timeout: Duration) -> WaitForCompletionOrTerminateOptions {
    WaitForCompletionOrTerminateOptions {
        wait_timeout,
        interrupt_timeout: Duration::from_secs(1),
        terminate_timeout: Duration::from_secs(1),
    }
}

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

mod wait_for_completion_with_output {
    use super::*;

    #[tokio::test]
    async fn preserves_unterminated_final_line() {
        let mut process = crate::Process::new(ScriptedOutput::builder().stdout("tail").build())
            .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["tail"]);
        assert_that!(output.stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn collects_stdout_lines_and_empty_stderr() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("Line 1\nLine 2\nLine 3\n")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["Line 1", "Line 2", "Line 3"]);
        assert_that!(output.stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn captures_startup_output_with_replay_last_bytes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("startup-out\n")
                .stderr("startup-err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;

        let output = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["startup-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["startup-err"]);
    }

    #[tokio::test]
    async fn closes_stdin_before_waiting() {
        let cmd = tokio::process::Command::new("cat");
        let mut process = crate::Process::new(cmd)
            .name("cat")
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
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(process.stdin().is_open()).is_false();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["collector wait closes stdin"]);
        assert_that!(output.stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn collects_trusted_unbounded_lines() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("out\nline\n")
                .stderr("err\nline\n")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_output(
                Duration::from_secs(2),
                LineOutputOptions {
                    line_parsing_options: line_parsing_options(),
                    stdout_collection_options: LineCollectionOptions::TrustedUnbounded,
                    stderr_collection_options: LineCollectionOptions::TrustedUnbounded,
                },
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["out", "line"]);
        assert_that!(output.stdout.truncated()).is_false();
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["err", "line"]);
        assert_that!(output.stderr.truncated()).is_false();
    }

    #[tokio::test]
    async fn times_out_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(exits_while_descendant_keeps_stdout_open_command())
            .name(AutoName::program_only())
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

        let timeout = Duration::from_millis(100);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion_with_output(timeout, line_output_options()),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitWithOutputError::OutputCollectionTimeout {
                timeout: actual, ..
            } => {
                assert_that!(actual).is_equal_to(timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn single_subscriber_timeout_releases_collector_claim() {
        let mut process = crate::Process::new(exits_while_descendant_keeps_stdout_open_command())
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .unwrap();

        let result = process
            .wait_for_completion_with_output(Duration::from_millis(100), line_output_options())
            .await;
        assert_that!(result.is_err()).is_true();

        let collector = process
            .stdout()
            .collect_lines_into_vec(line_parsing_options(), line_collection_options())
            .unwrap();
        let _collected = collector
            .cancel(Duration::from_secs(1))
            .await
            .unwrap()
            .expect_cancelled("collector should observe cancellation");
    }

    #[tokio::test]
    async fn single_subscriber_can_be_retried_after_process_finishes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("early-out\n")
                .stderr("early-err\n")
                .stdout_after(Duration::from_millis(200), "late-out\n")
                .stderr_after(Duration::from_millis(200), "late-err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        let first = process
            .wait_for_completion_with_output(Duration::from_millis(25), line_output_options())
            .await;
        assert_that!(first.unwrap().into_completed().is_none()).is_true();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let output = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["early-out", "late-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["early-err", "late-err"]);
    }

    #[tokio::test]
    async fn single_subscriber_active_stdout_consumer_returns_start_error() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name("scripted")
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        let active = process
            .stdout()
            .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
            .unwrap();

        let err = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .expect_err("active stdout consumer should reject output collection");

        match err {
            WaitWithOutputError::OutputCollectionStartFailed {
                source:
                    StreamConsumerError::ActiveConsumer {
                        stream_name: "stdout",
                    },
                ..
            } => {}
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
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name("scripted")
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        let active_stderr = process
            .stderr()
            .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
            .unwrap();

        let err = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .expect_err("active stderr consumer should reject output collection");

        match err {
            WaitWithOutputError::OutputCollectionStartFailed {
                source:
                    StreamConsumerError::ActiveConsumer {
                        stream_name: "stderr",
                    },
                ..
            } => {}
            other => {
                assert_that!(&other).fail(format_args!("expected start failure, got {other:?}"));
            }
        }

        let stdout = process
            .stdout()
            .collect_chunks_into_vec(RawCollectionOptions::TrustedUnbounded)
            .unwrap();
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

mod wait_for_completion_with_raw_output {
    use super::*;

    #[tokio::test]
    async fn preserves_bytes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("out\nraw")
                .stderr("err\nraw")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_raw_output(Duration::from_secs(2), raw_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"out\nraw".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"err\nraw".to_vec());

        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("out\nraw")
                .stderr("err\nraw")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        let output = process
            .wait_for_completion_with_raw_output(Duration::from_secs(2), raw_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"out\nraw".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"err\nraw".to_vec());
    }

    #[tokio::test]
    async fn reports_truncation() {
        let mut process = crate::Process::new(ScriptedOutput::builder().stdout("abcdef").build())
            .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_raw_output(
                Duration::from_secs(2),
                RawOutputOptions {
                    stdout_collection_options: RawCollectionOptions::Bounded {
                        max_bytes: 3.bytes(),
                        overflow_behavior: CollectionOverflowBehavior::default(),
                    },
                    stderr_collection_options: RawCollectionOptions::Bounded {
                        max_bytes: 3.bytes(),
                        overflow_behavior: CollectionOverflowBehavior::default(),
                    },
                },
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"abc".to_vec());
        assert_that!(output.stdout.truncated).is_true();
        assert_that!(output.stderr.truncated).is_false();
    }

    #[tokio::test]
    async fn collects_trusted_unbounded_bytes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("trusted-out")
                .stderr("trusted-err")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_raw_output(
                Duration::from_secs(2),
                RawOutputOptions {
                    stdout_collection_options: RawCollectionOptions::TrustedUnbounded,
                    stderr_collection_options: RawCollectionOptions::TrustedUnbounded,
                },
            )
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"trusted-out".to_vec());
        assert_that!(output.stdout.truncated).is_false();
        assert_that!(output.stderr.bytes).is_equal_to(b"trusted-err".to_vec());
        assert_that!(output.stderr.truncated).is_false();
    }

    #[tokio::test]
    async fn times_out_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(exits_while_descendant_keeps_stdout_open_command())
            .name(AutoName::program_only())
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

        let timeout = Duration::from_millis(100);
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion_with_raw_output(timeout, raw_output_options()),
        )
        .await
        .expect("raw output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitWithOutputError::OutputCollectionTimeout {
                timeout: actual, ..
            } => {
                assert_that!(actual).is_equal_to(timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[tokio::test]
    async fn single_subscriber_can_be_retried_after_process_finishes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("early-out")
                .stderr("early-err")
                .stdout_after(Duration::from_millis(200), "late-out")
                .stderr_after(Duration::from_millis(200), "late-err")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout_and_stderr(|stream| {
            stream
                .single_subscriber()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap();

        let first = process
            .wait_for_completion_with_raw_output(Duration::from_millis(25), raw_output_options())
            .await;
        assert_that!(first.unwrap().into_completed().is_none()).is_true();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let output = process
            .wait_for_completion_with_raw_output(Duration::from_secs(2), raw_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"early-outlate-out".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"early-errlate-err".to_vec());
    }
}

mod wait_for_completion_with_output_or_terminate {
    use super::*;

    #[tokio::test]
    async fn collects_lines() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("line-out\n")
                .stderr("line-err\n")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_output_or_terminate(
                wait_or_terminate_options(Duration::from_secs(2)),
                line_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["line-out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["line-err"]);
    }

    #[tokio::test]
    async fn times_out_when_descendant_keeps_output_open() {
        let mut process = crate::Process::new(exits_while_descendant_keeps_stdout_open_command())
            .name(AutoName::program_only())
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

        let wait_timeout = Duration::from_millis(100);
        let interrupt_timeout = Duration::from_millis(1);
        let terminate_timeout = Duration::from_millis(1);
        let operation_timeout = wait_timeout + interrupt_timeout + terminate_timeout;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            process.wait_for_completion_with_output_or_terminate(
                WaitForCompletionOrTerminateOptions {
                    wait_timeout,
                    interrupt_timeout,
                    terminate_timeout,
                },
                line_output_options(),
            ),
        )
        .await
        .expect("output wait should return before the outer guard");

        let err = result.expect_err("inherited output pipe should hit the operation deadline");
        match err {
            WaitWithOutputError::OutputCollectionTimeout {
                timeout: actual, ..
            } => {
                assert_that!(actual).is_equal_to(operation_timeout);
            }
            other => {
                assert_that!(&other).fail(format_args!(
                    "expected output collection timeout, got {other:?}"
                ));
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn force_kill_extends_output_collection_budget() {
        let mut process = crate::Process::new(
            ignores_graceful_shutdown_and_keeps_stdout_open_until_force_killed_command(),
        )
        .name(AutoName::program_only())
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

        let wait_timeout = Duration::from_millis(25);
        let interrupt_timeout = Duration::from_millis(25);
        let terminate_timeout = Duration::from_millis(25);
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            process.wait_for_completion_with_output_or_terminate(
                WaitForCompletionOrTerminateOptions {
                    wait_timeout,
                    interrupt_timeout,
                    terminate_timeout,
                },
                line_output_options(),
            ),
        )
        .await
        .expect("output wait should return before the outer guard")
        .expect("extended force-kill budget should let output collection finish");
        let output = match output {
            crate::WaitForCompletionOrTerminateResult::TerminatedAfterTimeout {
                result,
                timeout,
            } => {
                assert_that!(timeout).is_equal_to(wait_timeout);
                result
            }
            other @ crate::WaitForCompletionOrTerminateResult::Completed(_) => {
                assert_that!(&other).fail(format_args!(
                    "expected timeout-driven termination, got {other:?}"
                ));
                return;
            }
        };

        assert_that!(output.status.success()).is_false();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["ready"]);
    }

    #[tokio::test]
    async fn collects_trusted_unbounded_lines() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("trusted-line-out\n")
                .stderr("trusted-line-err\n")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_output_or_terminate(
                wait_or_terminate_options(Duration::from_secs(2)),
                LineOutputOptions {
                    line_parsing_options: line_parsing_options(),
                    stdout_collection_options: LineCollectionOptions::TrustedUnbounded,
                    stderr_collection_options: LineCollectionOptions::TrustedUnbounded,
                },
            )
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str))
            .contains_exactly(["trusted-line-out"]);
        assert_that!(output.stdout.truncated()).is_false();
        assert_that!(output.stderr.lines().iter().map(String::as_str))
            .contains_exactly(["trusted-line-err"]);
        assert_that!(output.stderr.truncated()).is_false();
    }
}

mod wait_for_completion_with_raw_output_or_terminate {
    use super::*;

    #[tokio::test]
    async fn collects_bytes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("raw-out")
                .stderr("raw-err")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_raw_output_or_terminate(
                wait_or_terminate_options(Duration::from_secs(2)),
                raw_output_options(),
            )
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"raw-out".to_vec());
        assert_that!(output.stderr.bytes).is_equal_to(b"raw-err".to_vec());
    }

    #[tokio::test]
    async fn collects_trusted_unbounded_bytes() {
        let mut process = crate::Process::new(
            ScriptedOutput::builder()
                .stdout("trusted-raw-out")
                .stderr("trusted-raw-err")
                .build(),
        )
        .name(AutoName::program_only())
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

        let output = process
            .wait_for_completion_with_raw_output_or_terminate(
                wait_or_terminate_options(Duration::from_secs(2)),
                RawOutputOptions {
                    stdout_collection_options: RawCollectionOptions::TrustedUnbounded,
                    stderr_collection_options: RawCollectionOptions::TrustedUnbounded,
                },
            )
            .await
            .unwrap()
            .expect_completed("process should complete before timeout");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.bytes).is_equal_to(b"trusted-raw-out".to_vec());
        assert_that!(output.stdout.truncated).is_false();
        assert_that!(output.stderr.bytes).is_equal_to(b"trusted-raw-err".to_vec());
        assert_that!(output.stderr.truncated).is_false();
    }
}
