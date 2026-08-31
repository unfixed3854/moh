use std::path::PathBuf;

use serde_json::Value;

use crate::session::PlanItem;

use super::RunFailure;

/// The model-facing role of a text message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Text supplied by the user.
    User,
    /// Text returned by the assistant.
    Assistant,
}

/// A committed text-only message in model-facing session history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    /// The message author role.
    pub role: Role,
    /// The message text.
    pub text: String,
}

impl Message {
    /// Creates a text message with the supplied role.
    pub fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
        }
    }
}

/// An opaque identifier assigned by [`Harness`](super::Harness) to one run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RunId(u64);

impl RunId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the monotonic numeric value of this identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Context supplied for a run independently from committed message history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunContext {
    /// The working directory used by relative tool paths.
    pub cwd: PathBuf,
    /// Accepted execution-plan snapshot supplied by the session actor.
    pub plan: Vec<PlanItem>,
}

/// A model-neutral request started by a [`RunEngine`](super::RunEngine).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// The new user prompt for this run.
    pub prompt: String,
    /// A snapshot of successful exchanges committed before this run.
    pub history: Vec<Message>,
    /// Per-run execution context.
    pub context: RunContext,
}

/// An event yielded by a model-neutral run engine.
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    /// Incremental assistant text that is not yet committed to history.
    AssistantDelta(String),
    /// Input-token usage reported for a completed model call.
    ContextUsage {
        /// Input tokens in the completed model call's context.
        input_tokens: u64,
    },
    /// A tool invocation has begun.
    ToolStarted {
        /// The engine-provided tool call identifier.
        call_id: String,
        /// The tool name.
        name: String,
        /// The model-provided tool arguments.
        arguments: Value,
    },
    /// A tool invocation has finished.
    ToolFinished {
        /// The engine-provided tool call identifier.
        call_id: String,
        /// The tool name.
        name: String,
    },
    /// The final assistant response for a successful exchange.
    Completed(String),
}

/// An observable lifecycle event produced by a [`Harness`](super::Harness).
#[derive(Debug)]
pub enum RunEvent {
    /// A run was accepted and assigned an identifier.
    Started {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
    },
    /// Incremental assistant text for the active run.
    AssistantDelta {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// The incremental text.
        text: String,
    },
    /// Input-token usage reported for a completed model call.
    ContextUsage {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// Input tokens in the completed model call's context.
        input_tokens: u64,
    },
    /// A tool invocation began during the active run.
    ToolStarted {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// The engine-provided tool call identifier.
        call_id: String,
        /// The tool name.
        name: String,
        /// The model-provided tool arguments.
        arguments: Value,
    },
    /// A tool invocation finished during the active run.
    ToolFinished {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// The engine-provided tool call identifier.
        call_id: String,
        /// The tool name.
        name: String,
    },
    /// A successful run committed its final response to history.
    Completed {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// The committed final assistant response.
        response: String,
    },
    /// A run ended unsuccessfully without changing committed history.
    Failed {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
        /// The structured reason for the failure.
        failure: RunFailure,
    },
    /// A run was explicitly cancelled without changing committed history.
    Cancelled {
        /// The harness-assigned identifier for this run.
        run_id: RunId,
    },
}
