use super::name::{AutoName, ProcessName, generate_name};
use super::stream_config::{ProcessStreamBuilder, ProcessStreamConfig};
use crate::error::SpawnError;
use crate::output_stream::OutputStream;
use crate::process_handle::ProcessHandle;
use std::borrow::Cow;
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
///     .with_name("test-runner")
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
    /// [`Process::auto_name`] to opt into the default safe auto-derived name.
    #[must_use]
    pub fn new(cmd: tokio::process::Command) -> Self {
        Self { cmd }
    }

    /// Sets how the process should be named.
    ///
    /// You can provide either an explicit name or configure automatic name generation.
    /// The name is used in public errors and tracing fields. By default, automatic
    /// naming captures only the program name. Prefer `.with_name(...)` for stable safe
    /// labels when command arguments or environment variables may contain secrets.
    #[must_use]
    pub fn name(self, name: impl Into<ProcessName>) -> NamedProcess {
        NamedProcess {
            cmd: self.cmd,
            name: name.into(),
        }
    }

    /// Convenience method to set an explicit process name.
    ///
    /// This is a shorthand for `.name(ProcessName::Explicit(...))`.
    #[must_use]
    pub fn with_name(self, name: impl Into<Cow<'static, str>>) -> NamedProcess {
        self.name(ProcessName::Explicit(name.into()))
    }

    /// Convenience method to configure automatic name generation.
    ///
    /// This is a shorthand for `.name(ProcessName::Auto(...))`. Generated names are
    /// used in public errors and tracing fields, so only opt into argument, environment,
    /// current-directory, or debug capture when those values are safe to log.
    #[must_use]
    pub fn with_auto_name(self, mode: AutoName) -> NamedProcess {
        self.name(ProcessName::Auto(mode))
    }

    /// Uses the default safe auto-derived process name.
    ///
    /// The default captures only the command's program name, not arguments or environment
    /// variables.
    #[must_use]
    pub fn auto_name(self) -> NamedProcess {
        self.name(ProcessName::default())
    }
}

/// Builder stage after a process name has been selected.
///
/// Stream backend, delivery, replay, and stream options must be selected before spawning.
///
/// ```compile_fail
/// use tokio::process::Command;
/// use tokio_process_tools::Process;
///
/// let _process = Process::new(Command::new("ls"))
///     .auto_name()
///     .spawn();
/// ```
///
/// ```compile_fail
/// use tokio::process::Command;
/// use tokio_process_tools::Process;
///
/// let _process = Process::new(Command::new("ls"))
///     .auto_name()
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
    use crate::{
        CollectionOverflowBehavior, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE,
        LineCollectionOptions, LineOutputOptions, LineOverflowBehavior, LineParsingOptions,
        NumBytesExt, ProcessOutput, SealedReplayBehavior, WaitForCompletionOptions,
        WaitForCompletionWithOutputOptions,
    };
    use assertr::prelude::*;
    use tokio::process::Command;

    #[tokio::test]
    async fn process_builder_broadcast() {
        let mut process = Process::new(Command::new("ls"))
            .auto_name()
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stdout().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_that!(process.stderr().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);

        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(None)
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
            })
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn process_builder_broadcast_with_custom_capacities() {
        let mut process = Process::new(Command::new("ls"))
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(42.kilobytes())
                    .max_buffered_chunks(42)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(43.kilobytes())
                    .max_buffered_chunks(43)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(42.kilobytes());
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(43.kilobytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(42);
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(43);

        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(None)
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
            })
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber_with_per_stream_delivery() {
        let mut process = Process::new(Command::new("ls"))
            .auto_name()
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
            .auto_name()
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
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_chunks(1)
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .reliable_for_active_subscribers()
                    .replay_last_chunks(1)
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
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

    #[tokio::test]
    async fn process_builder_single_subscriber() {
        let mut process = Process::new(Command::new("ls"))
            .auto_name()
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(DEFAULT_READ_CHUNK_SIZE);
        assert_that!(process.stdout().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);
        assert_that!(process.stderr().max_buffered_chunks())
            .is_equal_to(DEFAULT_MAX_BUFFERED_CHUNKS);

        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(None)
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
            })
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber_with_custom_capacities() {
        let mut process = Process::new(Command::new("ls"))
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(42.kilobytes())
                    .max_buffered_chunks(42)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(43.kilobytes())
                    .max_buffered_chunks(43)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(42.kilobytes());
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(43.kilobytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(42);
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(43);

        let ProcessOutput {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(None)
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
            })
            .await
            .unwrap();

        assert_that!(status.success()).is_true();
        assert_that!(stdout.lines().is_empty()).is_false();
        assert_that!(stderr.lines().is_empty()).is_true();
    }

    #[tokio::test]
    async fn process_builder_broadcast_mode_uses_inline_stream_sizing() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'out\n'; printf 'err\n' >&2");

        let mut process = Process::new(cmd)
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(21.bytes())
                    .max_buffered_chunks(22)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(23.bytes())
                    .max_buffered_chunks(24)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().read_chunk_size()).is_equal_to(21.bytes());
        assert_that!(process.stdout().max_buffered_chunks()).is_equal_to(22);
        assert_that!(process.stderr().read_chunk_size()).is_equal_to(23.bytes());
        assert_that!(process.stderr().max_buffered_chunks()).is_equal_to(24);

        let output = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(Some(std::time::Duration::from_secs(2)))
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
            })
            .await
            .unwrap();

        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    #[tokio::test]
    async fn process_builder_broadcast_supports_distinct_stdout_and_stderr_mode_types() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'out\n'; printf 'err\n' >&2");

        let mut process = Process::new(cmd)
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().is_replay_sealed()).is_false();
        process.seal_stdout_replay();
        assert_that!(process.stdout().is_replay_sealed()).is_true();

        let output = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(Some(std::time::Duration::from_secs(2)))
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
            })
            .await
            .unwrap();

        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    #[tokio::test]
    async fn process_builder_supports_broadcast_stdout_and_single_subscriber_stderr() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'out\n'; printf 'err\n' >&2");

        let mut process = Process::new(cmd)
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .single_subscriber()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        process.seal_stdout_replay();
        assert_that!(process.stdout().is_replay_sealed()).is_true();

        let output = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(Some(std::time::Duration::from_secs(2)))
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
            })
            .await
            .unwrap();

        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    #[tokio::test]
    async fn process_builder_supports_single_subscriber_stdout_and_broadcast_stderr() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printf 'out\n'; printf 'err\n' >&2");

        let mut process = Process::new(cmd)
            .auto_name()
            .stdout(|stdout| {
                stdout
                    .single_subscriber()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .stderr(|stderr| {
                stderr
                    .broadcast()
                    .best_effort_delivery()
                    .replay_last_bytes(1.megabytes())
                    .sealed_replay_behavior(SealedReplayBehavior::StartAtLiveOutput)
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        process.seal_stderr_replay();
        assert_that!(process.stderr().is_replay_sealed()).is_true();

        let output = process
            .wait_for_completion_with_output({
                let line_collection_options = LineCollectionOptions::builder()
                    .max_bytes(1.megabytes())
                    .max_lines(1024)
                    .overflow_behavior(CollectionOverflowBehavior::default())
                    .build();

                WaitForCompletionWithOutputOptions::builder()
                    .timeout(Some(std::time::Duration::from_secs(2)))
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
            })
            .await
            .unwrap();

        assert_that!(output.stdout.lines().iter().map(String::as_str)).contains_exactly(["out"]);
        assert_that!(output.stderr.lines().iter().map(String::as_str)).contains_exactly(["err"]);
    }

    #[tokio::test]
    async fn process_builder_default_spawn_error_does_not_capture_sensitive_args() {
        let sensitive_arg = "--token=secret-token-should-not-be-logged";
        let mut cmd = Command::new("tokio-process-tools-definitely-missing-command");
        cmd.arg(sensitive_arg);

        let error = match Process::new(cmd)
            .auto_name()
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
            .with_name(format!("worker-{id}"))
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
}
