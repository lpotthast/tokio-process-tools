use super::*;
use crate::error::TerminationError;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::test_support::{
    ScriptedOutput, default_graceful_timeouts, long_running_command, short_graceful_timeouts,
};
use crate::{
    BestEffortDelivery, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NoReplay, NumBytesExt,
};
use assertr::prelude::*;

mod error_types;
mod graceful_timeouts_builder;
mod kill;
mod preflight_reap;
mod signal_failures;
mod terminate;

pub(super) fn immediately_exiting_command() -> tokio::process::Command {
    ScriptedOutput::builder().build()
}

pub(super) fn spawn_long_running_process()
-> ProcessHandle<BroadcastOutputStream<BestEffortDelivery, NoReplay>> {
    crate::Process::new(long_running_command(Duration::from_secs(5)))
        .name("long-running")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub(super) fn spawn_immediately_exiting_process()
-> ProcessHandle<BroadcastOutputStream<BestEffortDelivery, NoReplay>> {
    crate::Process::new(immediately_exiting_command())
        .name("immediate-exit")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .no_replay()
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
}

pub(super) fn successful_exit_status() -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        ExitStatus::from_raw(0)
    }

    #[cfg(all(not(windows), not(unix)))]
    {
        unimplemented!("test exit status construction is only implemented on Unix and Windows")
    }
}

pub(super) fn assert_attempt_error(
    attempt_error: &TerminationAttemptError,
    expected_phase: TerminationAttemptPhase,
    expected_operation: TerminationAttemptOperation,
    expected_signal_name: Option<&'static str>,
    expected_kind: io::ErrorKind,
    expected_message: &str,
) {
    assert_that!(attempt_error.phase).is_equal_to(expected_phase);
    assert_that!(attempt_error.operation).is_equal_to(expected_operation);
    assert_that!(attempt_error.signal_name).is_equal_to(expected_signal_name);

    let io_error = attempt_error
        .source
        .downcast_ref::<io::Error>()
        .expect("diagnostic should preserve the original io::Error");

    assert_that!(io_error.kind()).is_equal_to(expected_kind);
    assert_that!(io_error.to_string().as_str()).contains(expected_message);
}
