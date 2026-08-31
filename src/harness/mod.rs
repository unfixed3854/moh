//! Model-neutral session history and single-run lifecycle management.

mod engine;
mod error;
mod state;
mod types;

/// Model-neutral engine boundary types.
pub use engine::{RunEngine, RunStream};
/// Structured harness command and run failures.
pub use error::{HarnessError, RunFailure, RunFailureKind, RunStage};
/// The model-neutral harness state machine.
pub use state::Harness;
/// Model-facing messages, run identifiers, requests, and lifecycle events.
pub use types::{EngineEvent, Message, Role, RunContext, RunEvent, RunId, RunRequest};
