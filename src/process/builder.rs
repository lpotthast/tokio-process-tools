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
///             .lossy_without_backpressure()
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
