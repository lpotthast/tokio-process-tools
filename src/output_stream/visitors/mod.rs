//! User-replaceable convenience layer: built-in
//! [`StreamVisitor`](crate::StreamVisitor) and [`AsyncStreamVisitor`](crate::AsyncStreamVisitor)
//! implementations plus the factory macro that wires them as inherent methods on each backend.
//!
//! `consume_with(my_visitor)` is enough to use the library; everything in this module is sugar
//! for the common cases. Each submodule corresponds to one family of visitors:
//! - [`collect`] retains observed output in a sink (`Vec`, `CollectedBytes`, `CollectedLines`, ...).
//! - [`inspect`] observes output without retaining it.
//! - [`write`] forwards observed output to an `AsyncWrite` sink.
//! - [`wait`] watches for a single matching line and returns.
//!
//! [`factories`] holds the `impl_consumer_factories!` macro that emits the `inspect_*` /
//! `collect_*` inherent methods on each backend.

pub(crate) mod collect;
pub(crate) mod factories;
pub(crate) mod inspect;
pub(crate) mod wait;
pub(crate) mod write;
