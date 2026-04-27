use super::ProcessHandle;
use crate::output_stream::OutputStream;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::policy::{Delivery, ReplayEnabled};

impl<StdoutD, Stderr> ProcessHandle<BroadcastOutputStream<StdoutD, ReplayEnabled>, Stderr>
where
    StdoutD: Delivery,
    Stderr: OutputStream,
{
    /// Seals stdout replay history for future broadcast replay subscribers.
    pub fn seal_stdout_replay(&self) {
        self.std_out_stream.seal_replay();
    }
}

impl<Stdout, StderrD> ProcessHandle<Stdout, BroadcastOutputStream<StderrD, ReplayEnabled>>
where
    Stdout: OutputStream,
    StderrD: Delivery,
{
    /// Seals stderr replay history for future broadcast replay subscribers.
    pub fn seal_stderr_replay(&self) {
        self.std_err_stream.seal_replay();
    }
}

impl<StdoutD, StderrD>
    ProcessHandle<
        BroadcastOutputStream<StdoutD, ReplayEnabled>,
        BroadcastOutputStream<StderrD, ReplayEnabled>,
    >
where
    StdoutD: Delivery,
    StderrD: Delivery,
{
    /// Seals stdout and stderr replay history for future broadcast replay subscribers.
    pub fn seal_output_replay(&self) {
        self.seal_stdout_replay();
        self.seal_stderr_replay();
    }
}

impl<Stderr> ProcessHandle<SingleSubscriberOutputStream, Stderr>
where
    Stderr: OutputStream,
{
    /// Seals stdout replay history for a replay-enabled single-subscriber stream.
    pub fn seal_stdout_replay(&self) {
        self.std_out_stream.seal_replay();
    }
}

impl<Stdout> ProcessHandle<Stdout, SingleSubscriberOutputStream>
where
    Stdout: OutputStream,
{
    /// Seals stderr replay history for a replay-enabled single-subscriber stream.
    pub fn seal_stderr_replay(&self) {
        self.std_err_stream.seal_replay();
    }
}

impl ProcessHandle<SingleSubscriberOutputStream> {
    /// Seals stdout and stderr replay history for replay-enabled single-subscriber streams.
    pub fn seal_output_replay(&self) {
        self.seal_stdout_replay();
        self.seal_stderr_replay();
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::long_running_command;
    use crate::{
        AutoName, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, NumBytesExt, Process,
    };
    use assertr::prelude::*;
    use std::time::Duration;

    #[tokio::test]
    async fn process_handle_seal_output_replay_seals_stdout_and_stderr() {
        let process = Process::new(long_running_command(Duration::from_millis(100)))
            .name(AutoName::program_only())
            .stdout_and_stderr(|stream| {
                stream
                    .broadcast()
                    .reliable_for_active_subscribers()
                    .replay_last_bytes(1.megabytes())
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
            .expect("Failed to spawn");

        assert_that!(process.stdout().is_replay_sealed()).is_false();
        assert_that!(process.stderr().is_replay_sealed()).is_false();

        process.seal_output_replay();

        assert_that!(process.stdout().is_replay_sealed()).is_true();
        assert_that!(process.stderr().is_replay_sealed()).is_true();

        let mut process = process;
        let _ = process.wait_for_completion(None).await.unwrap();
    }
}
