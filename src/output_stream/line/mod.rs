//! Line-aware streaming primitives: parsing options, the underlying parser, and the visitor
//! adapter that drives a sink across chunk events.
//!
//! - [`options`] — [`LineParsingOptions`] and [`LineOverflowBehavior`] (the configuration
//!   surface).
//! - [`parser`] — [`LineParser`], the stateful state machine that turns byte chunks into
//!   lines.
//! - [`adapter`] — [`LineAdapter`] plus the [`LineSink`] / [`AsyncLineSink`] traits that user
//!   sinks implement to plug into the line-aware consumer pipeline.

pub mod adapter;
pub mod options;
pub mod parser;

pub use adapter::{AsyncLineSink, LineAdapter, LineSink};
pub use options::{LineOverflowBehavior, LineParsingOptions};
pub use parser::LineParser;
