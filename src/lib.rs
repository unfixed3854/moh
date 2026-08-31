//! Backend, session, runtime, RPC, and tool primitives used by the moh application.
#![warn(missing_docs)]

/// Backend-global activity and lifecycle coordination.
pub mod backend;
/// Dependency-free command-line mode parsing.
pub mod cli;
/// Model-neutral session history and single-run lifecycle management.
pub mod harness;
/// Backend configuration and secure local runtime paths.
pub mod local;
#[allow(clippy::all, clippy::pedantic)]
#[doc(hidden)]
#[path = "rpc/moh_capnp.rs"]
pub mod moh_capnp;
/// Model-provider authentication and transport integrations.
pub mod providers;
/// Versioned Cap'n Proto protocol bindings and domain conversions.
pub mod rpc;
/// Runtime adapters for model and tool integrations.
pub mod runtime;
/// Production backend process and runtime composition helpers.
#[cfg(unix)]
pub mod server;
/// Durable session identity, metadata, and committed-history persistence.
pub mod session;
/// Durable tool state and tool implementations.
pub mod tools;
