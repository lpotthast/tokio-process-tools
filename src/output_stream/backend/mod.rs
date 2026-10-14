//! Output stream backend implementations. All are tokio-bound, and the broadcast and
//! single-subscriber backends ingest any [`tokio::io::AsyncRead`] (not just process pipes).

/// Multi-consumer broadcast output stream backend.
pub(crate) mod broadcast;

/// Discard backend for stdio configured as `Stdio::null()`.
pub(crate) mod discard;

/// Single-consumer output stream backend.
pub(crate) mod single_subscriber;
