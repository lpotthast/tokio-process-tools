//! Consumer factories (`inspect_*`, `collect_*`, `wait_for_line*`) and the macro that wires them
//! onto stream backends.

#[macro_use]
pub(crate) mod api;
pub(crate) mod collect;
pub(crate) mod inspect;
pub(crate) mod line_waiter;
pub(crate) mod visitor;
pub(crate) mod wait;
pub(crate) mod write;

#[cfg(test)]
mod test_support;
