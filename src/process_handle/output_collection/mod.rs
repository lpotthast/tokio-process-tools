//! Terminal `wait_for_completion_with_*output*` methods that drain stdout/stderr alongside
//! process exit.

mod drain;
pub(crate) mod options;
pub(crate) mod output;
#[cfg(test)]
mod tests;

use self::drain::{
    wait_for_completion_or_terminate_with_collectors, wait_for_completion_with_collectors,
};
use self::output::ProcessOutput;
use super::ProcessHandle;
use crate::error::{
    WaitForCompletionOrTerminateResult, WaitForCompletionResult, WaitWithOutputError,
};
use crate::output_stream::TrySubscribable;
use crate::output_stream::consumer::collect;
use crate::process_handle::options::WaitForCompletionOrTerminateOptions;
use crate::process_handle::output_collection::options::{LineOutputOptions, RawOutputOptions};
use crate::{CollectedBytes, CollectedLines};

impl<Stdout, Stderr> ProcessHandle<Stdout, Stderr>
where
    Stdout: TrySubscribable,
    Stderr: TrySubscribable,
{
    /// Waits for the process to complete while collecting line output.
    ///
    /// Collectors are attached when this method is called. If the stream was configured with
    /// `.no_replay()`, output produced before attachment may be discarded; configure replay before
    /// spawning when startup output must be included.
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// `timeout` bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitWithOutputError`] if waiting for the process or collecting output
    /// fails after the process completes. Process timeout is returned as
    /// [`WaitForCompletionResult::Timeout`].
    pub async fn wait_for_completion_with_output(
        &mut self,
        timeout: std::time::Duration,
        output_options: LineOutputOptions,
    ) -> Result<WaitForCompletionResult<ProcessOutput<CollectedLines>>, WaitWithOutputError> {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let stdout = self.stdout();
        let out_subscription = stdout.try_subscribe().map_err(|source| {
            WaitWithOutputError::OutputCollectionStartFailed {
                process_name: self.name.clone(),
                source,
            }
        })?;
        let out_collector = collect::collect_lines_into_vec(
            stdout.name(),
            out_subscription,
            line_parsing_options,
            stdout_collection_options,
        );
        let stderr = self.stderr();
        let err_subscription = match stderr.try_subscribe() {
            Ok(subscription) => subscription,
            Err(source) => {
                out_collector.abort().await;
                return Err(WaitWithOutputError::OutputCollectionStartFailed {
                    process_name: self.name.clone(),
                    source,
                });
            }
        };
        let err_collector = collect::collect_lines_into_vec(
            stderr.name(),
            err_subscription,
            line_parsing_options,
            stderr_collection_options,
        );

        let result =
            wait_for_completion_with_collectors(self, timeout, out_collector, err_collector)
                .await?;

        Ok(result.map(|(status, stdout, stderr)| ProcessOutput {
            status,
            stdout,
            stderr,
        }))
    }

    /// Waits for the process to complete while collecting raw byte output.
    ///
    /// Any still-open stdin handle is closed before the terminal wait begins, matching
    /// [`tokio::process::Child::wait`].
    /// `timeout` bounds both process completion and stdout/stderr collection.
    ///
    /// # Errors
    ///
    /// Returns [`WaitWithOutputError`] if waiting for the process or collecting output
    /// fails after the process completes. Process timeout is returned as
    /// [`WaitForCompletionResult::Timeout`].
    pub async fn wait_for_completion_with_raw_output(
        &mut self,
        timeout: std::time::Duration,
        output_options: RawOutputOptions,
    ) -> Result<WaitForCompletionResult<ProcessOutput<CollectedBytes>>, WaitWithOutputError> {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let stdout = self.stdout();
        let out_subscription = stdout.try_subscribe().map_err(|source| {
            WaitWithOutputError::OutputCollectionStartFailed {
                process_name: self.name.clone(),
                source,
            }
        })?;
        let out_collector = collect::collect_chunks_into_vec(
            stdout.name(),
            out_subscription,
            stdout_collection_options,
        );
        let stderr = self.stderr();
        let err_subscription = match stderr.try_subscribe() {
            Ok(subscription) => subscription,
            Err(source) => {
                out_collector.abort().await;
                return Err(WaitWithOutputError::OutputCollectionStartFailed {
                    process_name: self.name.clone(),
                    source,
                });
            }
        };
        let err_collector = collect::collect_chunks_into_vec(
            stderr.name(),
            err_subscription,
            stderr_collection_options,
        );

        let result =
            wait_for_completion_with_collectors(self, timeout, out_collector, err_collector)
                .await?;

        Ok(result.map(|(status, stdout, stderr)| ProcessOutput {
            status,
            stdout,
            stderr,
        }))
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
    /// Returns [`WaitWithOutputError`] if waiting, termination, or output
    /// collection fails. Timeout-triggered cleanup success is returned as
    /// [`WaitForCompletionOrTerminateResult::TerminatedAfterTimeout`].
    pub async fn wait_for_completion_with_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
        output_options: LineOutputOptions,
    ) -> Result<
        WaitForCompletionOrTerminateResult<ProcessOutput<CollectedLines>>,
        WaitWithOutputError,
    > {
        let LineOutputOptions {
            line_parsing_options,
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let stdout = self.stdout();
        let out_subscription = stdout.try_subscribe().map_err(|source| {
            WaitWithOutputError::OutputCollectionStartFailed {
                process_name: self.name.clone(),
                source,
            }
        })?;
        let out_collector = collect::collect_lines_into_vec(
            stdout.name(),
            out_subscription,
            line_parsing_options,
            stdout_collection_options,
        );
        let stderr = self.stderr();
        let err_subscription = match stderr.try_subscribe() {
            Ok(subscription) => subscription,
            Err(source) => {
                out_collector.abort().await;
                return Err(WaitWithOutputError::OutputCollectionStartFailed {
                    process_name: self.name.clone(),
                    source,
                });
            }
        };
        let err_collector = collect::collect_lines_into_vec(
            stderr.name(),
            err_subscription,
            line_parsing_options,
            stderr_collection_options,
        );

        let result = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(result.map(|(status, stdout, stderr)| ProcessOutput {
            status,
            stdout,
            stderr,
        }))
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
    /// Returns [`WaitWithOutputError`] if waiting, termination, or output
    /// collection fails. Timeout-triggered cleanup success is returned as
    /// [`WaitForCompletionOrTerminateResult::TerminatedAfterTimeout`].
    pub async fn wait_for_completion_with_raw_output_or_terminate(
        &mut self,
        options: WaitForCompletionOrTerminateOptions,
        output_options: RawOutputOptions,
    ) -> Result<
        WaitForCompletionOrTerminateResult<ProcessOutput<CollectedBytes>>,
        WaitWithOutputError,
    > {
        let RawOutputOptions {
            stdout_collection_options,
            stderr_collection_options,
        } = output_options;
        let stdout = self.stdout();
        let out_subscription = stdout.try_subscribe().map_err(|source| {
            WaitWithOutputError::OutputCollectionStartFailed {
                process_name: self.name.clone(),
                source,
            }
        })?;
        let out_collector = collect::collect_chunks_into_vec(
            stdout.name(),
            out_subscription,
            stdout_collection_options,
        );
        let stderr = self.stderr();
        let err_subscription = match stderr.try_subscribe() {
            Ok(subscription) => subscription,
            Err(source) => {
                out_collector.abort().await;
                return Err(WaitWithOutputError::OutputCollectionStartFailed {
                    process_name: self.name.clone(),
                    source,
                });
            }
        };
        let err_collector = collect::collect_chunks_into_vec(
            stderr.name(),
            err_subscription,
            stderr_collection_options,
        );

        let result = wait_for_completion_or_terminate_with_collectors(
            self,
            options.wait_timeout,
            options.interrupt_timeout,
            options.terminate_timeout,
            out_collector,
            err_collector,
        )
        .await?;

        Ok(result.map(|(status, stdout, stderr)| ProcessOutput {
            status,
            stdout,
            stderr,
        }))
    }
}
