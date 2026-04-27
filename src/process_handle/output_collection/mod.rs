mod drain;
pub(crate) mod options;
#[cfg(test)]
mod tests;

use self::drain::{
    wait_for_completion_or_terminate_with_collectors, wait_for_completion_with_collectors,
};
use super::ProcessHandle;
use crate::error::{WaitForCompletionWithOutputError, WaitForCompletionWithOutputOrTerminateError};
use crate::output::ProcessOutput;
use crate::output_stream::Collectable;
use crate::process_handle::options::WaitForCompletionOrTerminateOptions;
use crate::process_handle::output_collection::options::{LineOutputOptions, RawOutputOptions};
use crate::{CollectedBytes, CollectedLines};

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: Collectable,
    Stderr: Collectable,
{
    /// Waits for the process to complete while collecting line output.
    ///
    /// Collectors are attached when this method is called. If the stream was configured with
    /// `.no_replay()`, output produced before attachment may be discarded; configure replay before
    /// spawning when startup output must be included.
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_output(
        &mut self,
        timeout: Option<std::time::Duration>,
        output_options: LineOutputOptions,
    ) -> Result<ProcessOutput<CollectedLines>, WaitForCompletionWithOutputError> {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let out_collector = self
            .stdout()
            .collect_lines_into_vec(line_parsing_options, stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_lines_into_vec(line_parsing_options, stderr_collection_options);

        let (status, stdout, stderr) =
            wait_for_completion_with_collectors(self, timeout, out_collector, err_collector)
                .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for the process to complete while collecting raw byte output.
    ///
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// If `timeout` is set, it bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputError`] if waiting for the process or collecting output
    /// fails.
    pub async fn wait_for_completion_with_raw_output(
        &mut self,
        timeout: Option<std::time::Duration>,
        output_options: RawOutputOptions,
    ) -> Result<ProcessOutput<CollectedBytes>, WaitForCompletionWithOutputError> {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let out_collector = self
            .stdout()
            .collect_chunks_into_vec(stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_chunks_into_vec(stderr_collection_options);

        let (status, stdout, stderr) =
            wait_for_completion_with_collectors(self, timeout, out_collector, err_collector)
                .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting line output.
    ///
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
        output_options: LineOutputOptions,
    ) -> Result<ProcessOutput<CollectedLines>, WaitForCompletionWithOutputOrTerminateError> {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let out_collector = self
            .stdout()
            .collect_lines_into_vec(line_parsing_options, stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_lines_into_vec(line_parsing_options, stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Waits for completion within `wait_timeout`, terminating the process if needed, while
    /// collecting raw byte output.
    ///
    /// Any still-open stdin handle is closed before the initial terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// Output collection is bounded by
    /// `wait_timeout + interrupt_timeout + terminate_timeout`, plus a fixed 3-second post-kill
    /// confirmation wait when force-kill fallback is required.
    ///
    /// # Errors
    ///
    /// Returns [`WaitForCompletionWithOutputOrTerminateError`] if waiting, termination, or output
    /// collection fails.
    pub async fn wait_for_completion_with_raw_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
        output_options: RawOutputOptions,
    ) -> Result<ProcessOutput<CollectedBytes>, WaitForCompletionWithOutputOrTerminateError> {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let out_collector = self
            .stdout()
            .collect_chunks_into_vec(stdout_collection_options);
        let err_collector = self
            .stderr()
            .collect_chunks_into_vec(stderr_collection_options);

        let (status, stdout, stderr) = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }
}
