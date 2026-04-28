//! Compile-time proof that public stream and handle types remain `Send + Sync`.
//!
//! `Send` matters because users should be able to move a `ProcessHandle` into a spawned task.
//!
//! `Sync` matters because output streams are accessed through shared references from
//! `ProcessHandle::stdout()` / `ProcessHandle::stderr()`.
//! Broadcast streams are explicitly multi-consumer. Single-subscriber streams still use interior
//! synchronization so concurrent attempts can be safely rejected rather than becoming unsound.
//!
//! These assertions mainly protect an API guarantee: future internal changes must not accidentally
//! add something like `Rc`, `RefCell`, or another non-thread-safe field that would make
//! handles/streams awkward or impossible to use in normal async task patterns.

#![cfg(test)]

use crate::output_stream::OutputStream;
use crate::output_stream::policy::{Delivery, Replay};
use crate::{
    BroadcastOutputStream, ProcessHandle, SingleSubscriberOutputStream, TerminationAttemptError,
    TerminationError, WaitOrTerminateError, WaitWithOutputError,
};

#[allow(dead_code)] // Never really used.
trait SendSync: Send + Sync {}

impl SendSync for TerminationAttemptError {}
impl SendSync for TerminationError {}
impl SendSync for WaitOrTerminateError {}
impl SendSync for WaitWithOutputError {}

impl<D, R> SendSync for SingleSubscriberOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
}

impl<D, R> SendSync for BroadcastOutputStream<D, R>
where
    D: Delivery,
    R: Replay,
{
}

impl<Stdout, Stderr> SendSync for ProcessHandle<Stdout, Stderr>
where
    Stdout: OutputStream + SendSync,
    Stderr: OutputStream + SendSync,
{
}
