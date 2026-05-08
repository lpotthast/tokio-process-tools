//! Bundled [`StreamVisitor`](crate::StreamVisitor) and
//! [`AsyncStreamVisitor`](crate::AsyncStreamVisitor) implementations covering the most common
//! consumption patterns.
//!
//! `consume(my_visitor)` / `consume_async(my_visitor)` on any
//! [`Consumable`](crate::Consumable) stream is the only entry point. Construct one of the
//! visitors below (or your own) and pass it. Each submodule corresponds to one family:
//!
//! - [`crate::visitors::collect`] retains observed output in a sink (`CollectedBytes`,
//!   `CollectedLines`, or any user [`Sink`](crate::Sink)).
//! - [`crate::visitors::inspect`] observes output without retaining it.
//! - [`crate::visitors::write`] forwards observed output to an `AsyncWrite` sink.
//! - [`crate::visitors::wait`] watches for a single matching line and returns whether it was
//!   seen.

/// Visitors that retain observed output in a sink.
pub mod collect;

/// Visitors that observe output without retaining it.
pub mod inspect;

/// Visitor that waits for a single matching line.
pub mod wait;

/// Visitors that forward observed output to an async writer.
pub mod write;
