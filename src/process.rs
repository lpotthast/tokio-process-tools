//! Builder API for spawning processes with explicit stream type selection.

use crate::output_stream::broadcast::BroadcastOutputStream;
use crate::output_stream::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::{DEFAULT_CHANNEL_CAPACITY, DEFAULT_CHUNK_SIZE};
use crate::{NumBytes, ProcessHandle};
use std::borrow::Cow;
use std::io;

/// Controls how the process name is automatically generated when not explicitly provided.
///
/// This determines what information is included in the auto-generated process name
/// used for logging and debugging purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoName {
    /// Capture a name from the command as specified by the provided settings.
    ///
    /// Example: `"ls -la"` from `Command::new("ls").arg("-la")`
    Using(AutoNameSettings),

    /// Capture the full Debug representation of the Command.
    ///
    /// Example: `"Command { std: \"ls\" \"-la\", kill_on_drop: false }"`
    ///
    /// Note: This includes internal implementation details and may change with tokio updates.
    Debug,
}

impl Default for AutoName {
    fn default() -> Self {
        Self::Using(AutoNameSettings::program_with_args())
    }
}

/// Controls in detail which parts of the command are automatically captured as the process name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoNameSettings {
    include_current_dir: bool,
    include_envs: bool,
    include_program: bool,
    include_args: bool,
}

impl AutoNameSettings {
    /// Capture the program name.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"ls"`.
    pub fn program_only() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: false,
            include_program: true,
            include_args: false,
        }
    }

    /// Capture the program name and all arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"ls -la"`.
    pub fn program_with_args() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: false,
            include_program: true,
            include_args: true,
        }
    }

    /// Capture the program name and all environment variables and arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"FOO=foo ls -la"`.
    pub fn program_with_env_and_args() -> Self {
        AutoNameSettings {
            include_current_dir: false,
            include_envs: true,
            include_program: true,
            include_args: true,
        }
    }

    /// Capture the directory and the program name and all environment variables and arguments.
    ///
    /// Example: `Command::new("ls").arg("-la").env("FOO", "foo)` is captured as `"/some/dir % FOO=foo ls -la"`.
    pub fn full() -> Self {
        AutoNameSettings {
            include_current_dir: true,
            include_envs: true,
            include_program: true,
            include_args: true,
        }
    }

    fn format_cmd(&self, cmd: &std::process::Command) -> String {
        let mut name = String::new();
        if self.include_current_dir {
            if let Some(current_dir) = cmd.get_current_dir() {
                name.push_str(current_dir.to_string_lossy().as_ref());
                name.push_str(" % ");
            }
        }
        if self.include_envs {
            let envs = cmd.get_envs();
            if envs.len() != 0 {
                for (key, value) in envs
                    .filter(|(_key, value)| value.is_some())
                    .map(|(key, value)| (key, value.expect("present")))
                {
                    name.push_str(key.to_string_lossy().as_ref());
                    name.push('=');
                    name.push_str(value.to_string_lossy().as_ref());
                    name.push(' ');
                }
            }
        }
        if self.include_program {
            name.push_str(cmd.get_program().to_string_lossy().as_ref());
            name.push(' ');
        }
        if self.include_args {
            let args = cmd.get_args();
            if args.len() != 0 {
                for arg in args {
                    name.push('"');
                    name.push_str(arg.to_string_lossy().as_ref());
                    name.push('"');
                    name.push(' ');
                }
            }
        }
        if name.ends_with(' ') {
            name.pop();
        }
        name
    }
}

/// Specifies how a process should be named.
///
/// This enum allows you to either provide an explicit name or configure automatic
/// name generation. Using this type ensures you cannot accidentally set both an
/// explicit name and an auto-naming mode at the same time.
#[derive(Debug, Clone)]
pub enum ProcessName {
    /// Use an explicit custom name.
    ///
    /// Example: `ProcessName::Explicit("my-server")`
    Explicit(Cow<'static, str>),

    /// Auto-generate the name based on the command.
    ///
    /// Example: `ProcessName::Auto(AutoName::ProgramWithArgs)`
    Auto(AutoName),
}

impl Default for ProcessName {
    fn default() -> Self {
        Self::Auto(AutoName::default())
    }
}

impl From<&'static str> for ProcessName {
    fn from(s: &'static str) -> Self {
        Self::Explicit(Cow::Borrowed(s))
    }
}

impl From<String> for ProcessName {
    fn from(s: String) -> Self {
        Self::Explicit(Cow::Owned(s))
    }
}

impl From<Cow<'static, str>> for ProcessName {
    fn from(s: Cow<'static, str>) -> Self {
        Self::Explicit(s)
    }
}

impl From<AutoName> for ProcessName {
    fn from(mode: AutoName) -> Self {
        Self::Auto(mode)
    }
}

/// A builder for configuring and spawning a process.
///
/// This provides an ergonomic API for spawning processes while keeping the stream type
/// (broadcast vs single subscriber) explicit at the spawn callsite.
///
/// # Examples
///
/// ```no_run
/// use tokio_process_tools::Process;
/// use tokio::process::Command;
///
/// # tokio_test::block_on(async {
/// // Simple case with auto-derived name
/// let process = Process::new(Command::new("ls"))
///     .spawn_broadcast()?;
///
/// // With explicit name (no allocation when using string literal)
/// let process = Process::new(Command::new("server"))
///     .name("my-server")
///     .spawn_single_subscriber()?;
///
/// // With custom capacities
/// let process = Process::new(Command::new("cargo"))
///     .name("test-runner")
///     .stdout_capacity(512)
///     .stderr_capacity(512)
///     .spawn_broadcast()?;
/// # Ok::<_, std::io::Error>(())
/// # });
/// ```
pub struct Process {
    cmd: tokio::process::Command,
    name: ProcessName,
    stdout_chunk_size: NumBytes,
    stderr_chunk_size: NumBytes,
    stdout_capacity: usize,
    stderr_capacity: usize,
}

impl Process {
    /// Creates a new process builder from a tokio command.
    ///
    /// If no name is explicitly set via [`Process::name`], the name will be auto-derived
    /// from the command's program name.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("ls"))
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn new(cmd: tokio::process::Command) -> Self {
        Self {
            cmd,
            name: ProcessName::default(),
            stdout_chunk_size: DEFAULT_CHUNK_SIZE,
            stderr_chunk_size: DEFAULT_CHUNK_SIZE,
            stdout_capacity: DEFAULT_CHANNEL_CAPACITY,
            stderr_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }

    /// Sets how the process should be named.
    ///
    /// You can provide either an explicit name or configure automatic name generation.
    /// The name is used for logging and debugging purposes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::{Process, ProcessName, AutoName, AutoNameSettings};
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// // Explicit name
    /// let process = Process::new(Command::new("server"))
    ///     .name(ProcessName::Explicit("my-server".into()))
    ///     .spawn_broadcast()?;
    ///
    /// // Auto-generated with arguments
    /// let mut cmd = Command::new("cargo");
    /// cmd.arg("test");
    /// let process = Process::new(cmd)
    ///     .name(ProcessName::Auto(AutoName::Using(AutoNameSettings::program_with_args())))
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn name(mut self, name: impl Into<ProcessName>) -> Self {
        self.name = name.into();
        self
    }

    /// Convenience method to set an explicit process name.
    ///
    /// This is a shorthand for `.name(ProcessName::Explicit(...))`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// // Static name (no allocation)
    /// let process = Process::new(Command::new("server"))
    ///     .with_name("my-server")
    ///     .spawn_broadcast()?;
    ///
    /// // Dynamic name (allocates)
    /// let id = 42;
    /// let process = Process::new(Command::new("worker"))
    ///     .with_name(format!("worker-{id}"))
    ///     .spawn_single_subscriber()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn with_name(self, name: impl Into<Cow<'static, str>>) -> Self {
        self.name(ProcessName::Explicit(name.into()))
    }

    /// Convenience method to configure automatic name generation.
    ///
    /// This is a shorthand for `.name(ProcessName::Auto(...))`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::{AutoName, AutoNameSettings, Process};
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let mut cmd = Command::new("server");
    /// cmd.arg("--database").arg("sqlite");
    /// cmd.env("S3_ENDPOINT", "127.0.0.1:9000");
    ///
    /// let process = Process::new(cmd)
    ///     .with_auto_name(AutoName::Using(AutoNameSettings::program_with_env_and_args()))
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn with_auto_name(self, mode: AutoName) -> Self {
        self.name(ProcessName::Auto(mode))
    }

    /// Sets the stdout chunk size.
    ///
    /// This controls the size of the buffer used when reading from the process's stdout stream.
    /// Default is [DEFAULT_CHUNK_SIZE].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::{NumBytesExt, Process};
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .stdout_chunk_size(32.kilobytes())
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn stdout_chunk_size(mut self, chunk_size: NumBytes) -> Self {
        self.stdout_chunk_size = chunk_size;
        self
    }

    /// Sets the stderr chunk size.
    ///
    /// This controls the size of the buffer used when reading from the process's stderr stream.
    /// Default is [DEFAULT_CHUNK_SIZE].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::{Process, NumBytesExt};
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .stderr_chunk_size(32.kilobytes())
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn stderr_chunk_size(mut self, chunk_size: NumBytes) -> Self {
        self.stderr_chunk_size = chunk_size;
        self
    }

    /// Sets the stdout and stderr chunk sizes.
    ///
    /// This controls the size of the buffers used when reading from the process's stdout and
    /// stderr streams.
    /// Default is [DEFAULT_CHUNK_SIZE].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::{Process, NumBytesExt};
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .chunk_sizes(32.kilobytes())
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn chunk_sizes(mut self, chunk_size: NumBytes) -> Self {
        self.stdout_chunk_size = chunk_size;
        self.stderr_chunk_size = chunk_size;
        self
    }

    /// Sets the stdout channel capacity.
    ///
    /// This controls how many chunks can be buffered before backpressure is applied.
    /// Default is [DEFAULT_CHANNEL_CAPACITY].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .stdout_capacity(512)
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn stdout_capacity(mut self, capacity: usize) -> Self {
        self.stdout_capacity = capacity;
        self
    }

    /// Sets the stderr channel capacity.
    ///
    /// This controls how many chunks can be buffered before backpressure is applied.
    /// Default is [DEFAULT_CHANNEL_CAPACITY].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .stderr_capacity(256)
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn stderr_capacity(mut self, capacity: usize) -> Self {
        self.stderr_capacity = capacity;
        self
    }

    /// Sets the stdout and stderr channel capacity.
    ///
    /// This controls how many chunks can be buffered before backpressure is applied.
    /// Default is [DEFAULT_CHANNEL_CAPACITY].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let process = Process::new(Command::new("server"))
    ///     .capacities(256)
    ///     .spawn_broadcast()?;
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn capacities(mut self, capacity: usize) -> Self {
        self.stdout_capacity = capacity;
        self.stderr_capacity = capacity;
        self
    }

    /// Generates a process name based on the configured naming strategy.
    fn generate_name(&self) -> Cow<'static, str> {
        match &self.name {
            ProcessName::Explicit(name) => name.clone(),
            ProcessName::Auto(auto_name) => match auto_name {
                AutoName::Using(settings) => settings.format_cmd(self.cmd.as_std()).into(),
                AutoName::Debug => format!("{:?}", self.cmd).into(),
            },
        }
    }

    /// Spawns the process with broadcast output streams.
    ///
    /// Broadcast streams support multiple concurrent consumers of stdout/stderr,
    /// which is useful when you need to inspect, collect, and process output
    /// simultaneously. This comes with slightly higher memory overhead due to cloning.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let mut process = Process::new(Command::new("ls"))
    ///     .spawn_broadcast()?;
    ///
    /// // Multiple consumers can read the same output
    /// let _logger = process.stdout().inspect_lines(|line| {
    ///     println!("{}", line);
    ///     tokio_process_tools::Next::Continue
    /// }, Default::default());
    ///
    /// let _collector = process.stdout().collect_lines_into_vec(Default::default());
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn spawn_broadcast(self) -> io::Result<ProcessHandle<BroadcastOutputStream>> {
        let name = self.generate_name();
        ProcessHandle::<BroadcastOutputStream>::spawn_with_capacity(
            name,
            self.cmd,
            self.stdout_chunk_size,
            self.stderr_chunk_size,
            self.stdout_capacity,
            self.stderr_capacity,
        )
    }

    /// Spawns the process with single subscriber output streams.
    ///
    /// Single subscriber streams are more efficient (lower memory, no cloning) but
    /// only allow one consumer of stdout/stderr at a time. Use this when you only
    /// need to either inspect OR collect output, not both simultaneously.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio_process_tools::Process;
    /// use tokio::process::Command;
    ///
    /// # tokio_test::block_on(async {
    /// let mut process = Process::new(Command::new("ls"))
    ///     .spawn_single_subscriber()?;
    ///
    /// // Only one consumer allowed
    /// let collector = process.stdout_mut().collect_lines_into_vec(Default::default());
    /// # Ok::<_, std::io::Error>(())
    /// # });
    /// ```
    pub fn spawn_single_subscriber(
        self,
    ) -> io::Result<ProcessHandle<SingleSubscriberOutputStream>> {
        let name = self.generate_name();
        ProcessHandle::<SingleSubscriberOutputStream>::spawn_with_capacity(
            name,
            self.cmd,
            self.stdout_chunk_size,
            self.stderr_chunk_size,
            self.stdout_capacity,
            self.stderr_capacity,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LineParsingOptions, NumBytesExt, Output, OutputStream};
    use assertr::prelude::*;
    use std::path::PathBuf;
    use tokio::process::Command;

    #[tokio::test]
    async fn process_builder_broadcast() {
        let mut process = Process::new(Command::new("ls"))
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(process.stdout().chunk_size()).is_equal_to(DEFAULT_CHUNK_SIZE);
        assert_that(process.stderr().chunk_size()).is_equal_to(DEFAULT_CHUNK_SIZE);
        assert_that(process.stdout().channel_capacity()).is_equal_to(DEFAULT_CHANNEL_CAPACITY);
        assert_that(process.stderr().channel_capacity()).is_equal_to(DEFAULT_CHANNEL_CAPACITY);

        let Output {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(None, LineParsingOptions::default())
            .await
            .unwrap();

        assert_that(status.success()).is_true();
        assert_that(stdout).is_not_empty();
        assert_that(stderr).is_empty();
    }

    #[tokio::test]
    async fn process_builder_broadcast_with_custom_capacities() {
        let mut process = Process::new(Command::new("ls"))
            .stdout_chunk_size(42.kilobytes())
            .stderr_chunk_size(43.kilobytes())
            .stdout_capacity(42)
            .stderr_capacity(43)
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(process.stdout().chunk_size()).is_equal_to(42.kilobytes());
        assert_that(process.stderr().chunk_size()).is_equal_to(43.kilobytes());
        assert_that(process.stdout().channel_capacity()).is_equal_to(42);
        assert_that(process.stderr().channel_capacity()).is_equal_to(43);

        let Output {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(None, LineParsingOptions::default())
            .await
            .unwrap();

        assert_that(status.success()).is_true();
        assert_that(stdout).is_not_empty();
        assert_that(stderr).is_empty();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber() {
        let mut process = Process::new(Command::new("ls"))
            .spawn_single_subscriber()
            .expect("Failed to spawn");

        assert_that(process.stdout().chunk_size()).is_equal_to(DEFAULT_CHUNK_SIZE);
        assert_that(process.stderr().chunk_size()).is_equal_to(DEFAULT_CHUNK_SIZE);
        assert_that(process.stdout().channel_capacity()).is_equal_to(DEFAULT_CHANNEL_CAPACITY);
        assert_that(process.stderr().channel_capacity()).is_equal_to(DEFAULT_CHANNEL_CAPACITY);

        let Output {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(None, LineParsingOptions::default())
            .await
            .unwrap();

        assert_that(status.success()).is_true();
        assert_that(stdout).is_not_empty();
        assert_that(stderr).is_empty();
    }

    #[tokio::test]
    async fn process_builder_single_subscriber_with_custom_capacities() {
        let mut process = Process::new(Command::new("ls"))
            .stdout_chunk_size(42.kilobytes())
            .stderr_chunk_size(43.kilobytes())
            .stdout_capacity(42)
            .stderr_capacity(43)
            .spawn_single_subscriber()
            .expect("Failed to spawn");

        assert_that(process.stdout().chunk_size()).is_equal_to(42.kilobytes());
        assert_that(process.stderr().chunk_size()).is_equal_to(43.kilobytes());
        assert_that(process.stdout().channel_capacity()).is_equal_to(42);
        assert_that(process.stderr().channel_capacity()).is_equal_to(43);

        let Output {
            status,
            stdout,
            stderr,
        } = process
            .wait_for_completion_with_output(None, LineParsingOptions::default())
            .await
            .unwrap();

        assert_that(status.success()).is_true();
        assert_that(stdout).is_not_empty();
        assert_that(stderr).is_empty();
    }

    #[tokio::test]
    async fn process_builder_auto_name_captures_command_with_args_if_not_otherwise_specified() {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));

        let mut process = Process::new(cmd)
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to("ls \"-la\"");

        let _ = process.wait_for_completion(None).await;
    }

    #[tokio::test]
    async fn process_builder_auto_name_only_captures_command_when_requested() {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));

        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Using(AutoNameSettings::program_only()))
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to("ls");

        let _ = process.wait_for_completion(None).await;
    }

    #[tokio::test]
    async fn process_builder_auto_name_captures_command_with_envs_and_args_when_requested() {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));

        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Using(
                AutoNameSettings::program_with_env_and_args(),
            ))
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to("FOO=foo ls \"-la\"");

        let _ = process.wait_for_completion(None).await;
    }

    #[tokio::test]
    async fn process_builder_auto_name_captures_command_with_current_dir_envs_and_args_when_requested()
     {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));

        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Using(AutoNameSettings::full()))
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to("./ % FOO=foo ls \"-la\"");

        let _ = process.wait_for_completion(None).await;
    }

    #[tokio::test]
    async fn process_builder_auto_name_captures_full_command_debug_string_when_requested() {
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        cmd.env("FOO", "foo");
        cmd.current_dir(PathBuf::from("./"));

        let mut process = Process::new(cmd)
            .with_auto_name(AutoName::Debug)
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to(
            "Command { std: cd \"./\" && FOO=\"foo\" \"ls\" \"-la\", kill_on_drop: false }",
        );

        let _ = process.wait_for_completion(None).await;
    }

    #[tokio::test]
    async fn process_builder_custom_name() {
        let id = 42;
        let mut process = Process::new(Command::new("ls"))
            .with_name(format!("worker-{id}"))
            .spawn_broadcast()
            .expect("Failed to spawn");

        assert_that(&process.name).is_equal_to("worker-42");

        let _ = process.wait_for_completion(None).await;
    }
}
