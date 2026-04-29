//! Built-in [`StreamVisitor`](crate::StreamVisitor) and
//! [`AsyncStreamVisitor`](crate::AsyncStreamVisitor) implementations.
//!
//! Each submodule corresponds to one family of visitors:
//! - [`collect`] retains observed output in a sink (`Vec`, `CollectedBytes`, `CollectedLines`, ...).
//! - [`inspect`] observes output without retaining it.
//! - [`write`] forwards observed output to an `AsyncWrite` sink.
//! - [`wait`] watches for a single matching line and returns.

pub(crate) mod collect;
pub(crate) mod inspect;
pub(crate) mod wait;
pub(crate) mod write;
