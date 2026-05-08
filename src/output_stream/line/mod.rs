//! Line-aware streaming primitives: parsing options, the underlying parser, and the visitor
//! adapter that drives a sink across chunk events.
//!
//! - [`options`] holds [`options::LineParsingOptions`] and [`options::LineOverflowBehavior`]
//!   (the configuration surface).
//! - [`parser`] holds [`parser::LineParser`], the stateful state machine that turns byte
//!   chunks into lines.
//! - [`adapter`] holds [`adapter::ParseLines`] plus the [`adapter::LineVisitor`] and
//!   [`adapter::AsyncLineVisitor`] traits that user sinks implement to plug into the line-aware
//!   consumer pipeline.

pub(crate) mod adapter;
pub(crate) mod options;
pub(crate) mod parser;
