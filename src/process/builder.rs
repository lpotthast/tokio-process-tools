use super::name::{ProcessName, generate_name};
use super::stream_config::{ProcessStreamBuilder, ProcessStreamConfig};
use crate::error::SpawnError;
use crate::output_stream::OutputStream;
use crate::process_handle::ProcessHandle;
use std::marker::PhantomData;

/// Initial builder stage for configuring and spawning a process.
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
pub struct Process {
    cmd: tokio::process::Command,
}

impl Process {
    /// Creates a new process builder from a tokio command.
    ///
    /// A following naming call is required before configuring stdout and stderr. Use
    /// [`Process::name`] with [`crate::AutoName::program_only`] to opt into the default safe
    /// auto-derived name.
    ///
    /// ```compile_fail
    /// use tokio::process::Command;
    /// use tokio_process_tools::Process;
    ///
    /// let _process = Process::new(Command::new("ls"))
    ///     .stdout_and_stderr(|stream| {
    ///         stream
    ///             .broadcast()
    ///             .best_effort_delivery()
    ///             .no_replay()
    ///     });
    /// ```
    #[must_use]
    pub fn new(cmd: tokio::process::Command) -> Self {
        Self { cmd }
    }

    /// Sets how the process should be named.
    ///
    /// You can provide either an explicit name or configure automatic name generation.
    /// The name is used in public errors and tracing fields. By default, automatic
    /// naming captures only the program name. Prefer `.name(...)` for stable safe
    /// labels when command arguments or environment variables may contain secrets.
    #[must_use]
    pub fn name(self, name: impl Into<ProcessName>) -> NamedProcess {
        NamedProcess {
            cmd: self.cmd,
            name: name.into(),
        }
    }
}

/// Builder stage after a process name has been selected.
///
/// Stream backend, delivery, replay, and stream options must be selected before spawning.
///
/// ```compile_fail
/// use tokio::process::Command;
/// use tokio_process_tools::{AutoName, Process};
///
/// let _process = Process::new(Command::new("ls"))
///     .name(AutoName::program_only())
///     .spawn();
/// ```
///
/// ```compile_fail
/// use tokio::process::Command;
/// use tokio_process_tools::{AutoName, Process};
///
/// let _process = Process::new(Command::new("ls"))
///     .name(AutoName::program_only())
///     .stdout_and_stderr(|stream| {
///         stream
///             .broadcast()
///             .best_effort_delivery()
///             .no_replay()
///     })
///     .spawn();
/// ```
pub struct NamedProcess {
    cmd: tokio::process::Command,
    name: ProcessName,
}

impl NamedProcess {
    /// Configures stdout and stderr with the same output stream settings.
    #[must_use]
    pub fn stdout_and_stderr<Config, Stream>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> Config,
    ) -> ConfiguredProcessBuilder<Config, Stream, Config, Stream>
    where
        Config: ProcessStreamConfig<Stream> + Copy,
        Stream: OutputStream,
    {
        let config = configure(ProcessStreamBuilder);
        ConfiguredProcessBuilder {
            cmd: self.cmd,
            name: self.name,
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
    ) -> ProcessBuilderWithStdout<StdoutConfig, Stdout>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        Stdout: OutputStream,
    {
        ProcessBuilderWithStdout {
            cmd: self.cmd,
            name: self.name,
            stdout_config: configure(ProcessStreamBuilder),
            _stdout: PhantomData,
        }
    }

    /// Configures stderr before configuring stdout.
    #[must_use]
    pub fn stderr<StderrConfig, Stderr>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StderrConfig,
    ) -> ProcessBuilderWithStderr<StderrConfig, Stderr>
    where
        StderrConfig: ProcessStreamConfig<Stderr>,
        Stderr: OutputStream,
    {
        ProcessBuilderWithStderr {
            cmd: self.cmd,
            name: self.name,
            stderr_config: configure(ProcessStreamBuilder),
            _stderr: PhantomData,
        }
    }
}

/// Builder stage with configured stdout and pending stderr configuration.
pub struct ProcessBuilderWithStdout<StdoutConfig, Stdout>
where
    Stdout: OutputStream,
{
    cmd: tokio::process::Command,
    name: ProcessName,
    stdout_config: StdoutConfig,
    _stdout: PhantomData<fn() -> Stdout>,
}

impl<StdoutConfig, Stdout> ProcessBuilderWithStdout<StdoutConfig, Stdout>
where
    Stdout: OutputStream,
{
    /// Configures stderr and completes the process builder.
    #[must_use]
    pub fn stderr<StderrConfig, Stderr>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StderrConfig,
    ) -> ConfiguredProcessBuilder<StdoutConfig, Stdout, StderrConfig, Stderr>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        StderrConfig: ProcessStreamConfig<Stderr>,
        Stderr: OutputStream,
    {
        ConfiguredProcessBuilder {
            cmd: self.cmd,
            name: self.name,
            stdout_config: self.stdout_config,
            stderr_config: configure(ProcessStreamBuilder),
            _streams: PhantomData,
        }
    }
}

/// Builder stage with configured stderr and pending stdout configuration.
pub struct ProcessBuilderWithStderr<StderrConfig, Stderr>
where
    Stderr: OutputStream,
{
    cmd: tokio::process::Command,
    name: ProcessName,
    stderr_config: StderrConfig,
    _stderr: PhantomData<fn() -> Stderr>,
}

impl<StderrConfig, Stderr> ProcessBuilderWithStderr<StderrConfig, Stderr>
where
    Stderr: OutputStream,
{
    /// Configures stdout and completes the process builder.
    #[must_use]
    pub fn stdout<StdoutConfig, Stdout>(
        self,
        configure: impl FnOnce(ProcessStreamBuilder) -> StdoutConfig,
    ) -> ConfiguredProcessBuilder<StdoutConfig, Stdout, StderrConfig, Stderr>
    where
        StderrConfig: ProcessStreamConfig<Stderr>,
        StdoutConfig: ProcessStreamConfig<Stdout>,
        Stdout: OutputStream,
    {
        ConfiguredProcessBuilder {
            cmd: self.cmd,
            name: self.name,
            stdout_config: configure(ProcessStreamBuilder),
            stderr_config: self.stderr_config,
            _streams: PhantomData,
        }
    }
}

/// Final builder stage for a configured process.
pub struct ConfiguredProcessBuilder<
    StdoutConfig,
    Stdout,
    StderrConfig = StdoutConfig,
    Stderr = Stdout,
> where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    cmd: tokio::process::Command,
    name: ProcessName,
    stdout_config: StdoutConfig,
    stderr_config: StderrConfig,
    _streams: PhantomData<fn() -> (Stdout, Stderr)>,
}

impl<StdoutConfig, Stdout, StderrConfig, Stderr>
    ConfiguredProcessBuilder<StdoutConfig, Stdout, StderrConfig, Stderr>
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
        let name = generate_name(&self.name, &self.cmd);
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
    use crate::output_stream::collectable::CollectableOutputStream;
    use crate::{
        AutoName, AutoNameSettings, CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS,
        DEFAULT_READ_CHUNK_SIZE, LineCollectionOptions, LineOutputOptions, LineOverflowBehavior,
        LineParsingOptions, NumBytes, NumBytesExt, ProcessHandle, ProcessOutput,
        WaitForCompletionOptions, WaitForCompletionWithOutputOptions,
    };
    use assertr::prelude::*;
    use std::time::Duration;
    use tokio::process::Command;

    #[derive(Debug, Clone, Copy)]
    struct StreamSizing {
        read_chunk_size: NumBytes,
        max_buffered_chunks: usize,
    }

    #[derive(Debug, Clone, Copy)]
    struct StreamSizingPair {
        stdout: StreamSizing,
        stderr: StreamSizing,
    }

    impl StreamSizing {
        fn new(read_chunk_size: NumBytes, max_buffered_chunks: usize) -> Self {
            Self {
                read_chunk_size,
                max_buffered_chunks,
            }
        }
    }

    impl StreamSizingPair {
        fn same(read_chunk_size: NumBytes, max_buffered_chunks: usize) -> Self {
            let sizing = StreamSizing::new(read_chunk_size, max_buffered_chunks);
            Self {
                stdout: sizing,
                stderr: sizing,
            }
        }

        fn split(stdout: StreamSizing, stderr: StreamSizing) -> Self {
            Self { stdout, stderr }
        }
    }

    fn line_output_options(timeout: Option<Duration>) -> WaitForCompletionWithOutputOptions {
        let line_collection_options = LineCollectionOptions::builder()
            .max_bytes(1.megabytes())
            .max_lines(1024)
            .overflow_behavior(CollectionOverflowBehavior::default())
            .build();

        WaitForCompletionWithOutputOptions::builder()
            .timeout(timeout)
            .line_output_options(
                LineOutputOptions::builder()
                    .line_parsing_options(
                        LineParsingOptions::builder()
                            .max_line_length(16.kilobytes())
                            .overflow_behavior(LineOverflowBehavior::default())
                            .build(),
                    )
                    .stdout_collection_options(line_collection_options)
                    .stderr_collection_options(line_collection_options)
                    .build(),
            )
            .build()
    }

    fn out_and_err_command() -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'out\n'; printf 'err\n' >&2");
        cmd
    }

    fn assert_process_sizing<Stdout, Stderr>(
        process: &ProcessHandle<Stdout, Stderr>,
        expected: StreamSizingPair,
    ) where
        Stdout: OutputStream,
        Stderr: OutputStream,
    {
        assert_that!(process.stdout().read_chunk_size())
            .is_equal_to(expected.stdout.read_chunk_size);
        assert_that!(process.stdout().max_buffered_chunks())
            .is_equal_to(expected.stdout.max_buffered_chunks);
        assert_that!(process.stderr().read_chunk_size())
            .is_equal_to(expected.stderr.read_chunk_size);
        assert_that!(process.stderr().max_buffered_chunks())
            .is_equal_to(expected.stderr.max_buffered_chunks);
    }

    async fn assert_listing_completion<Stdout, Stderr>(mut process: ProcessHandle<Stdout, Stderr>)
    where
        Stdout: CollectableOutputStream,
        Stderr: CollectableOutputStream,
    {
        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(line_output_options(None))
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    async fn assert_out_and_err_completion<Stdout, Stderr>(
        mut process: ProcessHandle<Stdout, Stderr>,
    ) where
        Stdout: CollectableOutputStream,
        Stderr: CollectableOutputStream,
    {
        let output = process
            .wait_for_completion_with_output(line_output_options(Some(Duration::from_secs(2))))
            .await
            .unwrap();

        assert_that!(output.status.success()).is_true();
        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    macro_rules! same_backend_smoke_test {
        ($name:ident, $backend:ident, same_stream, $read_chunk_size:expr, $max_buffered_chunks:expr) => {
            #[tokio::test]
            async fn $name() {
                let process = Process::new(Command::new("ls"))
                    .name(AutoName::program_only())
                    .stdout_and_stderr(|stream| {
                        stream
                            .$backend()
                            .best_effort_delivery()
                            .replay_last_bytes(1.megabytes())
                            .read_chunk_size($read_chunk_size)
                            .max_buffered_chunks($max_buffered_chunks)
                    })
                    .spawn()
                    .expect("Failed to spawn");

                assert_process_sizing(
                    &process,
                    StreamSizingPair::same($read_chunk_size, $max_buffered_chunks),
                );
                assert_listing_completion(process).await;
            }
        };
        (
            $name:ident,
            $backend:ident,
            split_streams,
            stdout: ($stdout_read_chunk_size:expr, $stdout_max_buffered_chunks:expr),
            stderr: ($stderr_read_chunk_size:expr, $stderr_max_buffered_chunks:expr)
        ) => {
            #[tokio::test]
            async fn $name() {
                let process = Process::new(Command::new("ls"))
                    .name(AutoName::program_only())
                    .stdout(|stdout| {
                        stdout
                            .$backend()
                            .best_effort_delivery()
                            .replay_last_bytes(1.megabytes())
                            .read_chunk_size($stdout_read_chunk_size)
                            .max_buffered_chunks($stdout_max_buffered_chunks)
                    })
                    .stderr(|stderr| {
                        stderr
                            .$backend()
                            .best_effort_delivery()
                            .replay_last_bytes(1.megabytes())
                            .read_chunk_size($stderr_read_chunk_size)
                            .max_buffered_chunks($stderr_max_buffered_chunks)
                    })
                    .spawn()
                    .expect("Failed to spawn");

                assert_process_sizing(
                    &process,
                    StreamSizingPair::split(
                        StreamSizing::new($stdout_read_chunk_size, $stdout_max_buffered_chunks),
                        StreamSizing::new($stderr_read_chunk_size, $stderr_max_buffered_chunks),
                    ),
                );
                assert_listing_completion(process).await;
            }
        };
    }

    same_backend_smoke_test!(
        process_builder_broadcast,
        broadcast,
        same_stream,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS
    );

    same_backend_smoke_test!(
        process_builder_broadcast_with_custom_capacities,
        broadcast,
        split_streams,
        stdout: (42.kilobytes(), 42),
        stderr: (43.kilobytes(), 43)
    );

    #[tokio::test]
    async fn process_builder_single_subscriber_with_per_stream_delivery() {
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

        let _ = process
            .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber_with_typed_same_delivery() {
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
            .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber_with_typed_per_stream_replay() {
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
            .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
            .await
            .unwrap();
    }

    same_backend_smoke_test!(
        process_builder_single_subscriber,
        single_subscriber,
        same_stream,
        DEFAULT_READ_CHUNK_SIZE,
        DEFAULT_MAX_BUFFERED_CHUNKS
    );

    same_backend_smoke_test!(
        process_builder_single_subscriber_with_custom_capacities,
        single_subscriber,
        split_streams,
        stdout: (42.kilobytes(), 42),
        stderr: (43.kilobytes(), 43)
    );

    macro_rules! backend_matrix_test {
        (
            $name:ident,
            stdout: $stdout_backend:ident.$stdout_delivery:ident($stdout_read_chunk_size:expr, $stdout_max_buffered_chunks:expr),
            stderr: $stderr_backend:ident.$stderr_delivery:ident($stderr_read_chunk_size:expr, $stderr_max_buffered_chunks:expr),
            before_wait: |$process:ident| $before_wait:block
        ) => {
            #[tokio::test]
            async fn $name() {
                let $process = Process::new(out_and_err_command())
                    .name(AutoName::program_only())
                    .stdout(|stdout| {
                        stdout
                            .$stdout_backend()
                            .$stdout_delivery()
                            .replay_last_bytes(1.megabytes())
                            .read_chunk_size($stdout_read_chunk_size)
                            .max_buffered_chunks($stdout_max_buffered_chunks)
                    })
                    .stderr(|stderr| {
                        stderr
                            .$stderr_backend()
                            .$stderr_delivery()
                            .replay_last_bytes(1.megabytes())
                            .read_chunk_size($stderr_read_chunk_size)
                            .max_buffered_chunks($stderr_max_buffered_chunks)
                    })
                    .spawn()
                    .expect("Failed to spawn");

                assert_process_sizing(
                    &$process,
                    StreamSizingPair::split(
                        StreamSizing::new($stdout_read_chunk_size, $stdout_max_buffered_chunks),
                        StreamSizing::new($stderr_read_chunk_size, $stderr_max_buffered_chunks),
                    ),
                );

                $before_wait

                assert_out_and_err_completion($process).await;
            }
        };
    }

    backend_matrix_test!(
        process_builder_broadcast_mode_uses_inline_stream_sizing,
        stdout: broadcast.reliable_for_active_subscribers(21.bytes(), 22),
        stderr: broadcast.reliable_for_active_subscribers(23.bytes(), 24),
        before_wait: |_process| {}
    );

    backend_matrix_test!(
        process_builder_broadcast_supports_distinct_stdout_and_stderr_mode_types,
        stdout: broadcast.reliable_for_active_subscribers(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        stderr: broadcast.best_effort_delivery(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        before_wait: |process| {
            assert_that!(process.stdout().is_replay_sealed()).is_false();
            process.seal_stdout_replay();
            assert_that!(process.stdout().is_replay_sealed()).is_true();
        }
    );

    backend_matrix_test!(
        process_builder_supports_broadcast_stdout_and_single_subscriber_stderr,
        stdout: broadcast.reliable_for_active_subscribers(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        stderr: single_subscriber.best_effort_delivery(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        before_wait: |process| {
            process.seal_stdout_replay();
            assert_that!(process.stdout().is_replay_sealed()).is_true();
        }
    );

    backend_matrix_test!(
        process_builder_supports_single_subscriber_stdout_and_broadcast_stderr,
        stdout: single_subscriber.reliable_for_active_subscribers(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        stderr: broadcast.best_effort_delivery(
            DEFAULT_READ_CHUNK_SIZE,
            DEFAULT_MAX_BUFFERED_CHUNKS
        ),
        before_wait: |process| {
            process.seal_stderr_replay();
            assert_that!(process.stderr().is_replay_sealed()).is_true();
        }
    );

    #[tokio::test]
    async fn process_builder_default_spawn_error_does_not_capture_sensitive_args() {
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
                let _ = process
                    .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
                    .await;
                assert_that!(()).fail("command should fail to spawn");
                return;
            }
            Err(error) => error,
        };
        let error = error.to_string();

        assert_that!(error.as_str()).contains("tokio-process-tools-definitely-missing-command");
        assert_that!(error.as_str()).does_not_contain(sensitive_arg);
    }

    #[tokio::test]
    async fn process_builder_custom_name() {
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

        let _ = process
            .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
            .await;
    }

    #[tokio::test]
    async fn process_builder_accepts_auto_name_settings_directly() {
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

        let _ = process
            .wait_for_completion(WaitForCompletionOptions::builder().timeout(None).build())
            .await;
    }
}
