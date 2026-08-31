//! Generated Cap'n Proto protocol-v2 bindings and transport conversions.

/// Typed client ownership for backend and session capabilities.
#[cfg(unix)]
pub mod client;
/// Checked conversions between generated wire values and transport-neutral domain values.
pub mod convert;
/// Unix-stream Cap'n Proto backend and session services.
#[cfg(unix)]
pub mod server;
/// Generated bindings for the versioned Moh protocol schema.
pub use crate::moh_capnp;
