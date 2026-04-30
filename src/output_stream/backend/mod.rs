//! Output stream backend implementations. Tokio-bound; both backends ingest any
//! [`tokio::io::AsyncRead`] (not just process pipes).

/// Multi-consumer broadcast output stream backend.
pub(crate) mod broadcast;

/// Single-consumer output stream backend.
pub(crate) mod single_subscriber;
