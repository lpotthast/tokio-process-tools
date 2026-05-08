use super::group::{self, ProcessGroup};
use super::{ProcessHandle, Stdin};
use crate::error::SpawnError;
use crate::output_stream::OutputStream;
use crate::process::stream_config::ProcessStreamConfig;
use std::borrow::Cow;
use std::process::Stdio;

const STDOUT_STREAM_NAME: &str = "stdout";
const STDERR_STREAM_NAME: &str = "stderr";

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    pub(crate) fn spawn_with_stream_configs<StdoutConfig, StderrConfig>(
        name: impl Into<Cow<'static, str>>,
        mut cmd: tokio::process::Command,
        stdout_config: StdoutConfig,
        stderr_config: StderrConfig,
    ) -> Result<Self, SpawnError>
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        StderrConfig: ProcessStreamConfig<Stderr>,
    {
        let process_name = name.into();
        let stdout_stdio = stdout_config.child_stdio();
        let stderr_stdio = stderr_config.child_stdio();

        let child = group::prepare_command(&mut cmd)
            .stdin(Stdio::piped())
            .stdout(stdout_stdio)
            .stderr(stderr_stdio)
            // ProcessHandle itself performs the last-resort cleanup while its panic-on-drop guard
            // is armed. Keeping Tokio's unconditional kill-on-drop disabled ensures that
            // `must_not_be_terminated()` can really opt out.
            .kill_on_drop(false)
            .spawn()
            .map_err(|source| SpawnError::SpawnFailed {
                process_name: process_name.clone(),
                source,
            })?;

        // On Windows, `from_spawned_child` may fail if JobObject creation/attachment fails. In that
        // case it has already best-effort-killed the just-spawned child before returning. On other
        // targets the call is infallible.
        #[cfg(windows)]
        let mut group = ProcessGroup::from_spawned_child(child).map_err(|source| {
            SpawnError::JobAttachmentFailed {
                process_name: process_name.clone(),
                source,
            }
        })?;
        #[cfg(not(windows))]
        let mut group = ProcessGroup::from_spawned_child(child)
            .expect("ProcessGroup::from_spawned_child is infallible on non-Windows targets.");

        let stdin: Stdin = group.take_stdin().map_or(Stdin::Closed, Stdin::Open);
        let stdout: Option<tokio::process::ChildStdout> = group.take_stdout();
        let stderr: Option<tokio::process::ChildStderr> = group.take_stderr();

        let std_out_stream = stdout_config.into_stream(stdout, STDOUT_STREAM_NAME);
        let std_err_stream = stderr_config.into_stream(stderr, STDERR_STREAM_NAME);

        Ok(ProcessHandle {
            name: process_name,
            group,
            std_in: stdin,
            std_out_stream,
            std_err_stream,
            drop_mode: ProcessHandle::<Stdout, Stderr>::new_armed_drop_mode(),
        })
    }
}
