use super::ProcessHandle;
use crate::output_stream::OutputStream;
use crate::output_stream::backend::broadcast::BroadcastOutputStream;
use crate::output_stream::backend::single_subscriber::SingleSubscriberOutputStream;
use crate::output_stream::policy::{Delivery, ReplayEnabled};

/// Generates `seal_stdout_replay` for any `ProcessHandle` whose stdout is the named replay-enabled
/// backend. The stderr slot stays free in the type parameters so each backend has one impl block
/// regardless of what the caller paired it with on stderr.
macro_rules! impl_seal_stdout_replay {
    ($stdout:ident) => {
        impl<StdoutD, Stderr> ProcessHandle<$stdout<StdoutD, ReplayEnabled>, Stderr>
        where
            StdoutD: Delivery,
            Stderr: OutputStream,
        {
            /// Seals stdout replay history for future subscribers.
            pub fn seal_stdout_replay(&self) {
                self.std_out_stream.seal_replay();
            }
        }
    };
}

/// Mirror of [`impl_seal_stdout_replay`] for the stderr slot.
macro_rules! impl_seal_stderr_replay {
    ($stderr:ident) => {
        impl<Stdout, StderrD> ProcessHandle<Stdout, $stderr<StderrD, ReplayEnabled>>
        where
            Stdout: OutputStream,
            StderrD: Delivery,
        {
            /// Seals stderr replay history for future subscribers.
            pub fn seal_stderr_replay(&self) {
                self.std_err_stream.seal_replay();
            }
        }
    };
}

/// Generates `seal_output_replay` for the cartesian product of replay-enabled backends across
/// stdout and stderr. Delegates to the per-stream methods so the actual sealing logic stays in
/// one place.
macro_rules! impl_seal_output_replay {
    ($stdout:ident, $stderr:ident) => {
        impl<StdoutD, StderrD>
            ProcessHandle<$stdout<StdoutD, ReplayEnabled>, $stderr<StderrD, ReplayEnabled>>
        where
            StdoutD: Delivery,
            StderrD: Delivery,
        {
            /// Seals stdout and stderr replay history for replay-enabled streams.
            pub fn seal_output_replay(&self) {
                self.seal_stdout_replay();
                self.seal_stderr_replay();
            }
        }
    };
}

impl_seal_stdout_replay!(BroadcastOutputStream);
impl_seal_stdout_replay!(SingleSubscriberOutputStream);
impl_seal_stderr_replay!(BroadcastOutputStream);
impl_seal_stderr_replay!(SingleSubscriberOutputStream);
impl_seal_output_replay!(BroadcastOutputStream, BroadcastOutputStream);
impl_seal_output_replay!(BroadcastOutputStream, SingleSubscriberOutputStream);
impl_seal_output_replay!(SingleSubscriberOutputStream, BroadcastOutputStream);
impl_seal_output_replay!(SingleSubscriberOutputStream, SingleSubscriberOutputStream);
