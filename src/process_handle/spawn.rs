use super::{ProcessHandle, Stdin};
use crate::error::SpawnError;
use crate::output_stream::OutputStream;
use crate::process::stream_config::ProcessStreamConfig;
use crate::signal;
use std::borrow::Cow;
use std::process::Stdio;
use tokio::process::Child;

const STDOUT_STREAM_NAME: &str = "stdout";
const STDERR_STREAM_NAME: &str = "stderr";

struct PipedChildIo {
    stdin: Stdin,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
}

fn take_piped_child_io(child: &mut Child) -> PipedChildIo {
    PipedChildIo {
        stdin: child.stdin.take().map_or(Stdin::Closed, Stdin::Open),
        stdout: child
            .stdout
            .take()
            .expect("Child process stdout wasn't captured"),
        stderr: child
            .stderr
            .take()
            .expect("Child process stderr wasn't captured"),
    }
}

fn armed_process_handle<Stdout, Stderr>(
    name: impl Into<Cow<'static, str>>,
    child: Child,
    std_in: Stdin,
    std_out_stream: Stdout,
    std_err_stream: Stderr,
) -> ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream,
    Stderr: OutputStream,
{
    ProcessHandle {
        name: name.into(),
        child,
        std_in,
        std_out_stream,
        std_err_stream,
        drop_mode: ProcessHandle::<Stdout, Stderr>::new_armed_drop_mode(),
    }
}

fn prepare_command(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    signal::prepare_command_for_signalling(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // ProcessHandle itself performs the last-resort cleanup while its panic-on-drop guard
        // is armed. Keeping Tokio's unconditional kill-on-drop disabled ensures that
        // `must_not_be_terminated()` can really opt out.
        .kill_on_drop(false)
}

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
        prepare_command(&mut cmd)
            .spawn()
            .map(|child| {
                Self::new_from_child_with_stream_configs(
                    process_name.clone(),
                    child,
                    stdout_config,
                    stderr_config,
                )
            })
            .map_err(|source| SpawnError::SpawnFailed {
                process_name,
                source,
            })
    }

    fn new_from_child_with_stream_configs<StdoutConfig, StderrConfig>(
        name: impl Into<Cow<'static, str>>,
        mut child: Child,
        stdout_config: StdoutConfig,
        stderr_config: StderrConfig,
    ) -> Self
    where
        StdoutConfig: ProcessStreamConfig<Stdout>,
        StderrConfig: ProcessStreamConfig<Stderr>,
    {
        let PipedChildIo {
            stdin,
            stdout,
            stderr,
        } = take_piped_child_io(&mut child);

        let std_out_stream = stdout_config.into_stream(stdout, STDOUT_STREAM_NAME);
        let std_err_stream = stderr_config.into_stream(stderr, STDERR_STREAM_NAME);

        armed_process_handle(name, child, stdin, std_out_stream, std_err_stream)
    }
}
