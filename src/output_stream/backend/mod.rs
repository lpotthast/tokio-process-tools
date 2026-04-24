//! Output stream backend implementations.

mod consumer_api;

/// Multi-consumer broadcast output stream backend.
pub mod broadcast;

/// Single-consumer output stream backend.
pub mod single_subscriber;

#[cfg(test)]
mod test_support;
