//! Output stream backend implementations.

/// Multi-consumer broadcast output stream backend.
pub mod broadcast;

/// Single-consumer output stream backend.
pub mod single_subscriber;

pub(crate) mod factories;

#[cfg(test)]
mod test_support;
