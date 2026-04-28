use super::name::{ProcessName, generate_name};
use super::stream_config::{ProcessStreamBuilder, ProcessStreamConfig};
use crate::error::SpawnError;
use crate::output_stream::OutputStream;
use crate::process_handle::ProcessHandle;
use std::marker::PhantomData;

#[doc(hidden)]
pub struct Unnamed;

#[doc(hidden)]
pub struct Named {
    name: ProcessName,
}

#[doc(hidden)]
pub struct Unset;

/// Typestate builder for configuring and spawning a process.
///
/// A process must be named before configuring output streams. This keeps public errors and tracing
/// fields intentional, while stdout and stderr stream configuration remains explicit at the spawn
/// call site.
///
/// # Examples
///
/// ```no_run
/// use tokio_process_tools::*;
/// use tokio_process_tools::SpawnError;
/// use tokio::process::Command;
///
/// # tokio_test::block_on(async {
/// let process = Process::new(Command::new("cargo"))
///     .name("test-runner")
///     .stdout_and_stderr(|stream| {
///         stream
///             .broadcast()
///             .best_effort_delivery()
///             .no_replay()
///             .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
///             .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
///     })
///     .spawn()?;
/// # Ok::<_, SpawnError>(())
/// # });
/// ```
pub struct Process<
    NameState = Unnamed,
    StdoutConfig = Unset,
    Stdout = Unset,
    StderrConfig = Unset,
    Stderr = Unset,
> {
    cmd: tokio::process::Command,
    name_state: NameState,
    stdout_config: StdoutConfig,
    stderr_config: StderrConfig,
    _streams: PhantomData<fn() -> (Stdout, Stderr)>,
}

impl Process {
    /// Creates a new process builder from a tokio command.
    #[must_use]
    pub fn new(cmd: tokio::process::Command) -> Self {
        Self {
            cmd,
            name_state: Unnamed,
            stdout_config: Unset,
            stderr_config: Unset,
            _streams: PhantomData,
        }
    }

    /// Sets how the process should be named.
    ///
    /// You can provide either an explicit name or configure automatic name generation.
    /// The name is used in public errors and tracing fields. By default, automatic
    /// naming captures only the program name. Prefer `.name(...)` for stable safe
    /// labels when command arguments or environment variables may contain secrets.
    #[must_use]
    pub fn name(self, name: impl Into<ProcessName>) -> Process<Named> {
        Process {
            cmd: self.cmd,
            name_state: Named { name: name.into() },
            stdout_config: Unset,
            stderr_config: Unset,
            _streams: PhantomData,
        }
    }
}

impl Process<Named> {
    /// Configures stdout and stderr with the same output stream settings.
    #[must_use]
    pub fn stdout_and_stderr<Config, Stream>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> Config,
    ) -> Process<Named, Config, Stream, Config, Stream>
    where
        Config: ProcessStreamConfig<Stream> + Copy,
        Stream: OutputStream,
    {
        let config = configure(ProcessStreamBuilder);
        Process {
            cmd: self.cmd,
            name_state: self.name_state,
            stdout_config: config,
            stderr_config: config,
            _streams: PhantomData,
        }
    }

    /// Configures stdout before configuring stderr.
    #[must_use]
    pub fn stdout<StdoutConfig, Stdout>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StdoutConfig,
    ) -> Process<Named, StdoutConfig, Stdout>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        Stdout: OutputStream,
    {
        Process {
            cmd: self.cmd,
            name_state: self.name_state,
            stdout_config: configure(ProcessStreamBuilder),
            stderr_config: Unset,
            _streams: PhantomData,
        }
    }

    /// Configures stderr before configuring stdout.
    #[must_use]
    pub fn stderr<StderrConfig, Stderr>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StderrConfig,
    ) -> Process<Named, Unset, Unset, StderrConfig, Stderr>
    where
        StderrConfig: ProcessStreamConfig<Stderr>,
        Stderr: OutputStream,
    {
        Process {
            cmd: self.cmd,
            name_state: self.name_state,
            stdout_config: Unset,
            stderr_config: configure(ProcessStreamBuilder),
            _streams: PhantomData,
        }
    }
}

impl<StdoutConfig, Stdout> Process<Named, StdoutConfig, Stdout>
where
    Stdout: OutputStream,
{
    /// Configures stderr and completes the process builder.
    #[must_use]
    pub fn stderr<StderrConfig, Stderr>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StderrConfig,
    ) -> Process<Named, StdoutConfig, Stdout, StderrConfig, Stderr>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        StderrConfig: ProcessStreamConfig<Stderr>,
        Stderr: OutputStream,
    {
        Process {
            cmd: self.cmd,
            name_state: self.name_state,
            stdout_config: self.stdout_config,
            stderr_config: configure(ProcessStreamBuilder),
            _streams: PhantomData,
        }
    }
}

impl<StderrConfig, Stderr> Process<Named, Unset, Unset, StderrConfig, Stderr>
where
    Stderr: OutputStream,
{
    /// Configures stdout and completes the process builder.
    #[must_use]
    pub fn stdout<StdoutConfig, Stdout>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StdoutConfig,
    ) -> Process<Named, StdoutConfig, Stdout, StderrConfig, Stderr>
    where
        StderrConfig: ProcessStreamConfig<Stderr>,
        StdoutConfig: ProcessStreamConfig<Stdout>,
        Stdout: OutputStream,
    {
        Process {
            cmd: self.cmd,
            name_state: self.name_state,
            stdout_config: configure(ProcessStreamBuilder),
            stderr_config: self.stderr_config,
            _streams: PhantomData,
        }
    }
}

impl<StdoutConfig, Stdout, StderrConfig, Stderr>
    Process<Named, StdoutConfig, Stdout, StderrConfig, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    /// Spawns the process with the configured output streams.
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError::SpawnFailed`] if the process cannot be spawned.
    pub fn spawn(self) -> Result<ProcessHandle<Stdout, Stderr>, SpawnError>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        StderrConfig: ProcessStreamConfig<Stderr>,
    {
        let name = generate_name(&self.name_state.name, &self.cmd);
        ProcessHandle::<Stdout, Stderr>::spawn_with_stream_configs(
            name,
            self.cmd,
            self.stdout_config,
            self.stderr_config,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output_stream::TrySubscribable;
    use crate::test_support::{ScriptedOutput, line_output_options};
    use crate::{
        AutoName, AutoNameSettings, BestEffortDelivery, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, NoReplay, NumBytesExt, ProcessHandle, ProcessOutput,
        ReliableDelivery, ReplayEnabled, SingleSubscriberOutputStream,
    };
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::process::Command;

    async fn assert_successful_completion<Stdout, Stderr>(
        mut process: ProcessHandle<Stdout, Stderr>,
    ) where
        Stdout: TrySubscribable,
        Stderr: TrySubscribable,
    {
        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    async fn assert_out_and_err_completion<Stdout, Stderr>(
        mut process: ProcessHandle<Stdout, Stderr>,
    ) where
        Stdout: TrySubscribable,
        Stderr: TrySubscribable,
    {
        let output = process
            .wait_for_completion_with_output(Duration::from_secs(2), line_output_options())
            .await
            .unwrap()
            .expect_completed("process should complete");

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    mod same_backend {
        use super::*;

        #[tokio::test]
        async fn shared_broadcast_config_applies_to_stdout_and_stderr() {
            let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
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
        async fn split_broadcast_config_applies_per_stream() {
            let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
                .name(AutoName::program_only())
                .stdout(|stdout| {
                    stdout
                        .broadcast()
                        .best_effort_delivery()
                        .replay_last_bytes(1.megabytes())
                        .read_chunk_size(42.kilobytes())
                        .max_buffered_chunks(42)
                })
                .stderr(|stderr| {
                    stderr
                        .broadcast()
                        .best_effort_delivery()
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
        async fn shared_single_subscriber_config_applies_to_stdout_and_stderr() {
            let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
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
        async fn split_single_subscriber_config_applies_per_stream() {
            let process = Process::new(ScriptedOutput::builder().stdout("out\n").build())
                .name(AutoName::program_only())
                .stdout(|stdout| {
                    stdout
                        .single_subscriber()
                        .best_effort_delivery()
                        .replay_last_bytes(1.megabytes())
                        .read_chunk_size(42.kilobytes())
                        .max_buffered_chunks(42)
                })
                .stderr(|stderr| {
                    stderr
                        .single_subscriber()
                        .best_effort_delivery()
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
    }

    mod single_subscriber_delivery_and_replay {
        use super::*;

        fn assert_single_subscriber_stream_types<StdoutD, StdoutR, StderrD, StderrR>(
            _process: &ProcessHandle<
                SingleSubscriberOutputStream<StdoutD, StdoutR>,
                SingleSubscriberOutputStream<StderrD, StderrR>,
            >,
        ) where
            StdoutD: crate::Delivery,
            StdoutR: crate::Replay,
            StderrD: crate::Delivery,
            StderrR: crate::Replay,
        {
        }

        #[tokio::test]
        async fn split_delivery_modes_can_wait_for_completion() {
            let mut process = Process::new(Command::new("ls"))
                .name(AutoName::program_only())
                .stdout(|stdout| {
                    stdout
                        .single_subscriber()
                        .reliable_for_active_subscribers()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .stderr(|stderr| {
                    stderr
                        .single_subscriber()
                        .best_effort_delivery()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .spawn()
                .expect("Failed to spawn");

            assert_single_subscriber_stream_types::<
                ReliableDelivery,
                NoReplay,
                BestEffortDelivery,
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
                        .best_effort_delivery()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .stderr(|stderr| {
                    stderr
                        .single_subscriber()
                        .reliable_for_active_subscribers()
                        .replay_last_chunks(1)
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .spawn()
                .expect("Failed to spawn");

            assert_single_subscriber_stream_types::<
                BestEffortDelivery,
                NoReplay,
                ReliableDelivery,
                ReplayEnabled,
            >(&process);

            process.seal_stderr_replay();
            let _ = process
                .wait_for_completion(Duration::from_secs(2))
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn shared_no_replay_disables_replay_on_both_streams() {
            let mut process = Process::new(Command::new("ls"))
                .name(AutoName::program_only())
                .stdout_and_stderr(|stream| {
                    stream
                        .single_subscriber()
                        .reliable_for_active_subscribers()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .spawn()
                .expect("Failed to spawn");

            assert_that!(process.stdout().replay_enabled()).is_false();
            assert_that!(process.stderr().replay_enabled()).is_false();

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
                        .best_effort_delivery()
                        .replay_last_chunks(1)
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .stderr(|stderr| {
                    stderr
                        .single_subscriber()
                        .reliable_for_active_subscribers()
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

    mod mixed_backend {
        use super::*;

        #[tokio::test]
        async fn split_broadcast_config_applies_per_stream() {
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
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(21.bytes())
                    .max_buffered_chunks(22)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .reliable_for_active_subscribers()
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
        async fn broadcast_stdout_replay_can_be_sealed() {
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
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .best_effort_delivery()
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
        async fn broadcast_stdout_and_single_subscriber_stderr_can_complete() {
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
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .best_effort_delivery()
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

        #[tokio::test]
        async fn single_subscriber_stdout_and_broadcast_stderr_can_complete() {
            let process = Process::new(
                ScriptedOutput::builder()
                    .stdout("out\n")
                    .stderr("err\n")
                    .build(),
            )
            .name(AutoName::program_only())
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

            process.seal_stderr_replay();
            assert_that!(process.stderr().is_replay_sealed()).is_true();
            assert_out_and_err_completion(process).await;
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
                        .best_effort_delivery()
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
        async fn explicit_name_is_used_for_process_handle() {
            let id = 42;
            let mut process = Process::new(Command::new("ls"))
                .name(format!("worker-{id}"))
                .stdout_and_stderr(|stream| {
                    stream
                        .broadcast()
                        .best_effort_delivery()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .spawn()
                .expect("Failed to spawn");

            assert_that!(&process.name).is_equal_to("worker-42");

            let _ = process.wait_for_completion(Duration::from_secs(2)).await;
        }

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
                        .best_effort_delivery()
                        .no_replay()
                        .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                        .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
                })
                .spawn()
                .expect("Failed to spawn");

            assert_that!(&process.name).is_equal_to("./ % ls \"-la\"");

            let _ = process.wait_for_completion(Duration::from_secs(2)).await;
        }
    }
}
