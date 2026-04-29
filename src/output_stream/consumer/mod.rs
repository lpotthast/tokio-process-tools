//! The [`Consumer`](self::consumer::Consumer) handle plus the
//! [`StreamVisitor`](self::visitor::StreamVisitor) abstraction and built-in visitors that
//! drive output stream events.

#[allow(clippy::module_inception)]
pub(crate) mod consumer;
pub(crate) mod line_waiter;
pub(crate) mod visitor;
pub(crate) mod visitors;

#[cfg(test)]
mod test_support;
