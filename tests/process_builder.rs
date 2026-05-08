#![allow(missing_docs)]

mod common;

use assertr::prelude::*;
use common::*;
use std::time::Duration;
use tokio::process::Command;
use tokio_process_tools::{
    AutoName, AutoNameSettings, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_OUTPUT_EOF_TIMEOUT,
    DEFAULT_READ_CHUNK_SIZE, LossyWithoutBackpressure, NoReplay, NumBytesExt, OutputStream,
    Process, ProcessHandle, ProcessOutput, ProcessStreamBuilder, ReliableWithBackpressure,
    ReplayEnabled, SingleSubscriberOutputStream, Subscribable,
};

async fn assert_successful_completion<Stdout, Stderr>(mut process: ProcessHandle<Stdout, Stderr>)
where
    Stdout: OutputStream + Subscribable + Send,
    Stderr: OutputStream + Subscribable + Send,
{
    let ProcessOutput {
        status,
        stdout,
        stderr,
    } = process
        .wait_for_completion(Duration::from_secs(2))
        .with_line_output(
            DEFAULT_OUTPUT_EOF_TIMEOUT,
            line_parsing_options(),
            line_output_options(),
        )
        .await
        .unwrap()
        .expect_completed("process should complete");

    assert_that!(status.success()).is_true();
    assert_that!(stdout.lines().is_empty()).is_false();
    assert_that!(stderr.lines().is_empty()).is_true();
}

async fn assert_out_and_err_completion<Stdout, Stderr>(mut process: ProcessHandle<Stdout, Stderr>)
where
    Stdout: OutputStream + Subscribable + Send,
    Stderr: OutputStream + Subscribable + Send,
{
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
    assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
    assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
}

mod shared_config {
    use super::*;

    #[tokio::test]
    async fn shared_broadcast_config_applies_to_stdout_and_stderr() {
        let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stdout().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stderr().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_successful_completion(process).await;
    }

    #[tokio::test]
    async fn shared_single_subscriber_config_applies_to_stdout_and_stderr() {
        let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stdout().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_that!(process.stdout().replay_enabled()).is_false();
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stderr().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_that!(process.stderr().replay_enabled()).is_false();
        assert_successful_completion(process).await;
    }
}

mod split_config {
    use super::*;

    #[tokio::test]
    async fn split_broadcast_config_applies_per_stream() {
        let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .broadcast()
                    .lossy_without_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(42.kilobytes())
                    .max_buffered_chunks(42)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .lossy_without_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(43.kilobytes())
                    .max_buffered_chunks(43)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(42.kilobytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(42);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(43.kilobytes());
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(43);
        assert_successful_completion(process).await;
    }

    #[tokio::test]
    async fn split_single_subscriber_config_applies_per_stream() {
        let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(42.kilobytes())
                    .max_buffered_chunks(42)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(43.kilobytes())
                    .max_buffered_chunks(43)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(42.kilobytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(42);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(43.kilobytes());
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(43);
        assert_successful_completion(process).await;
    }

    #[tokio::test]
    async fn split_broadcast_config_applies_per_stream_with_dual_outputs() {
        let process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout(|stdout| {
            stdout
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(21.bytes())
                .max_buffered_chunks(22)
        })
        .stderr(|stderr| {
            stderr
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(23.bytes())
                .max_buffered_chunks(24)
        })
        .spawn()
        .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(21.bytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(22);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(23.bytes());
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(24);
        assert_out_and_err_completion(process).await;
    }

    #[tokio::test]
    async fn split_broadcast_replay_can_be_sealed() {
        let process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout(|stdout| {
            stdout
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(|stderr| {
            stderr
                .broadcast()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

        assert_that!(process.stdout().is_replay_sealed()).is_false();
        process.seal_stdout_replay();
        assert_that!(process.stdout().is_replay_sealed()).is_true();
        assert_out_and_err_completion(process).await;
    }

    #[tokio::test]
    async fn split_with_broadcast_stdout_and_single_subscriber_stderr_completes() {
        let process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout(|stdout| {
            stdout
                .broadcast()
                .reliable_with_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(|stderr| {
            stderr
                .single_subscriber()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

        process.seal_stdout_replay();
        assert_that!(process.stdout().is_replay_sealed()).is_true();
        assert_out_and_err_completion(process).await;
    }
}

mod single_subscriber_delivery_and_replay {
    use super::*;
    use tokio_process_tools::{Delivery, Replay};

    fn assert_single_subscriber_stream_types<StdoutD, StdoutR, StderrD, StderrR>(
        _process: &ProcessHandle<
            SingleSubscriberOutputStream<StdoutD, StdoutR>,
            SingleSubscriberOutputStream<StderrD, StderrR>,
        >,
    ) where
        StdoutD: Delivery,
        StdoutR: Replay,
        StderrD: Delivery,
        StderrR: Replay,
    {
    }

    #[tokio::test]
    async fn split_delivery_modes_can_wait_for_completion() {
        let mut process = Process::new(Command::new("ls"))
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .reliable_with_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_single_subscriber_stream_types::<
            ReliableWithBackpressure,
            NoReplay,
            LossyWithoutBackpressure,
            NoReplay,
        >(&process);

        let _ = process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn split_replay_modes_preserve_stream_types() {
        let mut process = Process::new(Command::new("ls"))
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .reliable_with_backpressure()
                    .replay_last_chunks(1)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_single_subscriber_stream_types::<
            LossyWithoutBackpressure,
            NoReplay,
            ReliableWithBackpressure,
            ReplayEnabled,
        >(&process);

        process.seal_stderr_replay();
        let _ = process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn split_replay_enabled_streams_can_be_sealed() {
        let mut process = Process::new(Command::new("ls"))
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .lossy_without_backpressure()
                    .replay_last_chunks(1)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .reliable_with_backpressure()
                    .replay_last_chunks(1)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().replay_enabled()).is_true();
        assert_that!(process.stderr().replay_enabled()).is_true();

        process.seal_output_replay();
        assert_that!(process.stdout().is_replay_sealed()).is_true();
        assert_that!(process.stderr().is_replay_sealed()).is_true();

        let _ = process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap();
    }
}

mod discard {
    use super::*;
    use tokio_process_tools::OutputStream;
    use unwrap_infallible::UnwrapInfallible;

    #[tokio::test]
    async fn stdout_and_stderr_complete_with_wait_for_completion() {
        let mut process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout_and_stderr(ProcessStreamBuilder::discard)
        .spawn()
        .expect("Failed to spawn");

        assert_that!(process.stdout().name()).is_equal_to("stdout");
        assert_that!(process.stderr().name()).is_equal_to("stderr");

        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");
    }

    #[tokio::test]
    async fn can_discard_stdout_and_broadcast_stderr() {
        let mut process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout(ProcessStreamBuilder::discard)
        .stderr(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .expect("Failed to spawn");

        let collector = collect_lines_into_vec(process.stderr()).unwrap_infallible();

        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines()).contains_exactly(["err"]);
    }

    #[tokio::test]
    async fn can_broadcast_stdout_and_discard_stderr() {
        let mut process = Process::new(
            ScriptedOutput::builder()
                .stdout("out\n")
                .stderr("err\n")
                .build(),
        )
        .name(AutoName::program_only())
        .stdout(|stream| {
            stream
                .broadcast()
                .lossy_without_backpressure()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .stderr(ProcessStreamBuilder::discard)
        .spawn()
        .expect("Failed to spawn");

        let collector = collect_lines_into_vec(process.stdout()).unwrap_infallible();

        process
            .wait_for_completion(Duration::from_secs(2))
            .await
            .unwrap()
            .expect_completed("process should complete");

        let collected = collector.wait().await.unwrap();
        assert_that!(collected.lines()).contains_exactly(["out"]);
    }
}

mod spawn_errors {
    use super::*;

    #[tokio::test]
    async fn default_auto_name_does_not_capture_sensitive_args_in_spawn_error() {
        let sensitive_arg = "--token=secret-token-should-not-be-logged";
        let mut cmd = Command::new("tokio-process-tools-definitely-missing-command");
        cmd.arg(sensitive_arg);

        let error = match Process::new(cmd)
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
        {
            Ok(mut process) => {
                let _ = process.wait_for_completion(Duration::from_secs(2)).await;
                assert_that!(()).fail("command should fail to spawn");
                return;
            }
            Err(error) => error,
        };
        let error = error.to_string();

        assert_that!(error.as_str()).contains("tokio-process-tools-definitely-missing-command");
        assert_that!(error.as_str()).does_not_contain(sensitive_arg);
    }
}

mod names {
    use super::*;

    #[tokio::test]
    async fn auto_name_settings_include_current_dir_and_args() {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("IGNORED_ENV", "secret");
        cmd.current_dir("./");

        let mut process = Process::new(cmd)
            .name(
                AutoNameSettings::builder()
                    .include_current_dir(true)
                    .include_args(true)
                    .build(),
            )
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .lossy_without_backpressure()
                    .no_replay()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.name()).is_equal_to("./ % ls \"-la\"");

        let _ = process.wait_for_completion(Duration::from_secs(2)).await;
    }
}
